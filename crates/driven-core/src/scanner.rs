//! Local tree walk; SPEC s6 / DESIGN s5.2.
//!
//! [`scan`] walks one backup source with the [`crate::exclude`]-configured
//! [`ignore::WalkBuilder`], diffs each file against the source's stored
//! `file_state` rows (SPEC s2), and returns the [`ScanResult`] the planner
//! (SPEC s7) turns into a [`crate::types::Plan`].
//!
//! Change detection (DESIGN s5.2 step 2):
//! - [`ScanMode::FastPath`]: a file is unchanged iff its `(size, mtime_ns)`
//!   match the stored row; otherwise it is new-or-changed. No content read.
//! - [`ScanMode::DeepVerify`]: additionally re-hashes (BLAKE3) the bytes of
//!   every file whose `(size, mtime_ns)` matched, and treats a hash that
//!   differs from `file_state.hash_blake3` as a change. This catches silent
//!   bit-rot and filesystem timestamp lies (DESIGN s3.3, s5.2 step 4;
//!   ROADMAP M2 "deep-verify catches bit-rot" row).
//!
//! Symlink policy (DESIGN s5.2.1, [`crate::types::SymlinkPolicy::Skip`]):
//! symlinks are never followed and the link itself is never backed up. The
//! walker is built with `follow_links(false)` so traversal cannot leave the
//! source root; this scanner additionally drops any yielded entry whose own
//! type is a symlink (a link *to* a file would otherwise pass the is_file
//! check on some platforms) and counts it. Because V1 never follows links,
//! traversal cycles cannot occur - we do NOT and need NOT claim
//! visited-inode cycle detection (the "visited-inode set when following"
//! note in DESIGN s5.2.1 applies only to the V2 follow mode). Hardlinks are
//! out of scope: two paths sharing one inode are each backed up
//! independently (DESIGN s5.2.1); a consumer wanting to dedup may do so by
//! `(dev, inode)` at its own layer.
//!
//! Safe deletion (DESIGN s5.2 step 3): a `file_state` path the walk did not
//! yield is reported `deleted` ONLY when the walk completed without error
//! over the subtree that should have visited it. Walker errors (a
//! permission denial on a directory, an unreadable entry) are NOT swallowed
//! with `filter_map(Result::ok)`; instead the errored path prefixes are
//! collected and any known path under such a prefix is suppressed from
//! `deleted` so a transient permission error can never cascade into
//! trashing a whole subtree on Drive. Errors are logged via `tracing`.
//!
//! Threading (DESIGN s5.2, "the app must stay responsive from t=0"): the walk
//! phase runs entirely OFF the async runtime. One `spawn_blocking` task builds
//! the [`crate::exclude::SourceMatcher`] (itself a full, pruned pass over the
//! tree to find the nested ignore files) and then drives
//! [`ignore::WalkParallel`] across several worker threads. Each worker does the
//! per-entry heavy lifting - the include/exclude decision, `stat`, the Windows
//! placeholder + ADS probes, and the BLAKE3 re-hash a deep-verify or coarse-FS
//! pass needs - and streams typed results over a bounded channel.
//!
//! A single async consumer drains that channel and owns everything that is
//! ORDER-SENSITIVE and therefore cannot be decided by a worker in isolation: the
//! `seen` set, NFC collision detection (first path wins, the later collider is
//! recorded and dropped), the throttled progress ticks, the errored-prefix
//! bookkeeping, and the latency reservoir. Awaiting the channel is the yield
//! point that keeps IPC responsive while the walk runs, so a scan over a
//! 600k-file source no longer freezes the UI at t=0 - and multiple workers
//! hashing at once is what finally saturates a fast disk.

use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::Context;
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkState};

use crate::exclude::{build_source_matcher, build_walker_with_matcher, SourceMatcher};
use crate::priority::WorkPriority;
use crate::state::{FileStateRow, SourceRow, StateRepo};
// `PlaceholderPolicy` is consumed only by `should_skip_placeholder`, which is
// cfg-gated the same way; on a non-Windows, non-test lib build the fn (and thus
// this import) is compiled out, so gate the import to match or it trips
// `unused_imports` under `-D warnings` on the Linux/macOS CI legs.
#[cfg(any(windows, test))]
use crate::state::PlaceholderPolicy;
use crate::types::{LocalEntry, RelativePath, ScanMode, ScanResult};

static TARGET: &str = "driven::core::scanner";

/// `FILE_ATTRIBUTE_RECALL_ON_OPEN` (DESIGN s5.2.1): set by OneDrive Files-On-
/// Demand (and similar cloud providers) on a placeholder whose bytes are not
/// resident on disk. Reading such a file forces a network hydration, so the
/// scanner skips these by default rather than pulling down TBs of cloud-only
/// data. Matches the Win32 SDK constant `0x00400000`.
///
/// Compiled on Windows (where the attribute exists) and in test builds on every
/// platform (so [`should_skip_placeholder`] can be exercised cross-OS - the real
/// attribute cannot be synthesised on a temp file portably, mirrored by the
/// chaos `onedrive-placeholder` scenario).
#[cfg(any(windows, test))]
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0040_0000;

/// Decide whether the scanner skips a cloud-only (OneDrive Files-On-Demand)
/// placeholder, given its Win32 file attributes and the source's
/// [`PlaceholderPolicy`] (issue #4, DESIGN s5.2.1).
///
/// Pure (no I/O) so both policy branches are unit-testable on any OS, mirroring
/// the repo's `classify_*` / `fallback_decision` pure-decision pattern:
/// - [`PlaceholderPolicy::Skip`] (default): skip when the
///   `FILE_ATTRIBUTE_RECALL_ON_OPEN` bit is set, so stat/hash never forces a
///   network hydration of dehydrated cloud data.
/// - [`PlaceholderPolicy::ForceDownload`]: never skip on this attribute; the file
///   flows through the normal open/read path, which hydrates it on read.
#[cfg(any(windows, test))]
fn should_skip_placeholder(file_attributes: u32, policy: PlaceholderPolicy) -> bool {
    matches!(policy, PlaceholderPolicy::Skip)
        && file_attributes & FILE_ATTRIBUTE_RECALL_ON_OPEN != 0
}

/// Whether `path` carries one or more WARN-WORTHY NTFS Alternate Data Streams
/// beyond its main unnamed `::$DATA` stream (DESIGN s5.2.1, STRESS_HARNESS
/// s3.5 `ads-alternate-data-stream`).
///
/// Driven backs up the main stream only; a file with named streams (e.g.
/// `foo.txt:secret`) silently loses those streams. The scanner detects them
/// so the orchestrator can surface a one-per-file `local.ads_skipped` warning
/// (SPEC s24) rather than dropping them silently - silent data loss in a
/// backup tool.
///
/// EXEMPTION: `Zone.Identifier` - the mark-of-the-web tag Windows attaches to
/// every downloaded file - is deliberately NOT warn-worthy. It is transient
/// browser provenance metadata, not user data; a Documents tree full of
/// downloads carries it on thousands of files, and warning on each buried the
/// real signal in the 2026-08-14 incident's logs (14,098 of that day's 14,174
/// lines were `ads_skipped`). A file whose ONLY named stream is
/// `Zone.Identifier` returns `false`; any other named stream still warns,
/// Zone.Identifier alongside it or not. Zero extra I/O either way: the stream
/// NAMES arrive in the same enumeration this probe already runs.
///
/// Windows enumerates streams via `FindFirstStreamW` / `FindNextStreamW`
/// (`STREAM_INFO_LEVELS::FindStreamInfoStandard`). The main stream reports as
/// `::$DATA`; any other `:<name>:$DATA` entry is an ADS. Non-Windows targets
/// have no ADS concept and always return `false`.
#[cfg(windows)]
fn has_alternate_data_streams(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    // Win32 surface used here (declared locally to avoid pulling the windows
    // crate into driven-core just for one scan-time probe). `WCHAR` is `u16`;
    // we use `u16` directly to avoid a clippy upper-case-acronym alias.
    #[repr(C)]
    struct Win32FindStreamData {
        stream_size: i64,
        // cStreamName is WCHAR[MAX_PATH + 36]; the leading ':' + name + ':' +
        // type. 296 = MAX_PATH(260) + 36.
        stream_name: [u16; 296],
    }
    const INVALID_HANDLE_VALUE: isize = -1;
    // FindStreamInfoStandard == 0.
    const FIND_STREAM_INFO_STANDARD: i32 = 0;
    extern "system" {
        fn FindFirstStreamW(
            file_name: *const u16,
            info_level: i32,
            find_stream_data: *mut Win32FindStreamData,
            flags: u32,
        ) -> isize;
        fn FindNextStreamW(handle: isize, find_stream_data: *mut Win32FindStreamData) -> i32;
        fn FindClose(handle: isize) -> i32;
    }

    // The stream name up to its NUL terminator.
    fn stream_name(name: &[u16]) -> &[u16] {
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        &name[..len]
    }

    // The unnamed main stream's name, as FindFirstStreamW reports it.
    fn is_main_stream(name: &[u16]) -> bool {
        let main: Vec<u16> = "::$DATA".encode_utf16().collect();
        stream_name(name) == main
    }

    // The mark-of-the-web tag (see the fn doc's EXEMPTION). NTFS stream names
    // are case-insensitive and this one is pure ASCII, so an ASCII
    // case-insensitive compare is exact.
    fn is_zone_identifier_stream(name: &[u16]) -> bool {
        const ZONE: &str = ":Zone.Identifier:$DATA";
        let name = stream_name(name);
        name.len() == ZONE.len()
            && name
                .iter()
                .zip(ZONE.bytes())
                .all(|(&c, e)| c < 128 && (c as u8).eq_ignore_ascii_case(&e))
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path; `data` is a fixed-size
    // stack buffer the API fills; the handle is closed before return on every
    // path. All pointers are valid for the duration of each call.
    unsafe {
        let mut data: Win32FindStreamData = std::mem::zeroed();
        let handle = FindFirstStreamW(
            wide.as_ptr(),
            FIND_STREAM_INFO_STANDARD,
            &mut data as *mut _,
            0,
        );
        if handle == INVALID_HANDLE_VALUE {
            // No stream enumeration available (e.g. not NTFS, or access
            // error): conservatively report no ADS rather than a false notice.
            return false;
        }
        let mut found_ads = false;
        loop {
            if !is_main_stream(&data.stream_name) && !is_zone_identifier_stream(&data.stream_name) {
                found_ads = true;
                break;
            }
            if FindNextStreamW(handle, &mut data as *mut _) == 0 {
                break;
            }
        }
        FindClose(handle);
        found_ads
    }
}

/// Walks one source and returns the new-or-changed / deleted diff (SPEC s6).
///
/// `mode` selects the change-detection predicate (see the module docs).
/// Pure aside from local filesystem reads and the `state` load; emits no
/// ops and mutates no state - the planner (SPEC s7) and executor (SPEC s8)
/// own those side effects.
///
/// Thin wrapper over [`scan_with_latency`] with no telemetry capture; the
/// existing tests + callers that do not thread a reservoir use this.
pub async fn scan(
    source: &SourceRow,
    state: &dyn StateRepo,
    mode: ScanMode,
) -> anyhow::Result<ScanResult> {
    scan_with_latency(source, state, mode, None).await
}

/// [`scan`] with an optional [`LatencyReservoir`](crate::telemetry::LatencyReservoir)
/// for per-file scan-processing latency capture (DESIGN s13 telemetry). When
/// `latency` is `Some` AND capture is enabled, each fully-processed file records
/// its stat-through-change-detection wall-clock time (the dominant cost is the
/// BLAKE3 re-hash on a deep-verify pass); when `None` there is zero overhead.
/// The orchestrator passes its shared reservoir here; every other caller passes
/// `None`.
pub async fn scan_with_latency(
    source: &SourceRow,
    state: &dyn StateRepo,
    mode: ScanMode,
    latency: Option<&crate::telemetry::LatencyReservoir>,
) -> anyhow::Result<ScanResult> {
    scan_with_progress(source, state, mode, latency, None).await
}

/// The live scan-progress sink the scanner AWAITS as the walk advances, so a
/// caller can stream "scanning... N files" to the UI while the walk is still
/// running (issue: "Run now" looked dead for ~10s because the whole scan phase
/// emitted nothing).
///
/// Mirrors the executor's [`OutcomeSink`](crate::executor::OutcomeSink): a boxed
/// future so the sink may do async work (the orchestrator awaits a state
/// transition + broadcast), with the borrow tied to the call so the closure can
/// capture `&self` without a `'static` bound. Each tick is followed by an
/// explicit `yield_now`, which is what hands the otherwise fully synchronous
/// walk back to the runtime so the tick's consumers run mid-scan.
///
/// The argument is the running count of regular files the walk has VISITED -
/// counted before the include/exclude decision, so a long descent through an
/// excluded tree (`node_modules`) still shows movement rather than a frozen
/// counter.
pub type ScanProgressSink<'a> =
    dyn Fn(u64) -> futures::future::BoxFuture<'a, ()> + Send + Sync + 'a;

/// Files visited between two [`ScanProgressSink`] ticks. Paired with
/// [`SCAN_PROGRESS_MIN_INTERVAL`] so a fast walk ticks on the file stride and a
/// slow one (deep-verify re-hashing multi-GB files) still ticks on the clock.
const SCAN_PROGRESS_FILE_STRIDE: u64 = 512;

/// Minimum wall-clock gap between two [`ScanProgressSink`] ticks. Keeps a
/// million-file walk to a few ticks per second instead of one per file, so the
/// orchestrator's bounded event broadcast cannot be flooded by scan progress.
const SCAN_PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// Entries a walk worker accumulates before handing its batch to the consumer.
///
/// Batching amortises the channel hand-off over ~100 entries instead of paying
/// a send + wakeup per file, while staying well under
/// [`SCAN_PROGRESS_FILE_STRIDE`] so the consumer's progress counter still
/// advances in usefully small steps rather than jumping to the total.
const WALK_BATCH_MAX_ENTRIES: usize = 128;

/// How long a worker may hold a partial batch. Without this a worker that is
/// hashing one multi-GB file would sit on the entries it already processed, and
/// the progress readout would stall even though other workers are making
/// progress.
const WALK_BATCH_MAX_INTERVAL: Duration = Duration::from_millis(50);

/// Depth of the bounded worker -> consumer channel, in batches. Bounded so a
/// walk that outruns the consumer applies backpressure to the workers instead of
/// buffering an entire 600k-file tree in memory.
const WALK_CHANNEL_DEPTH: usize = 64;

/// Ceiling on walk worker threads. Past a handful of threads a scan stops being
/// limited by CPU and starts thrashing the disk queue (and competing with the
/// upload pipeline), so the parallelism is capped rather than tracking core
/// count on a 32-core machine.
const WALK_MAX_THREADS: usize = 8;

/// Floor on walk worker threads: even on a single-core box, one thread blocked
/// in `stat` on a slow network share should not stop the rest of the walk.
const WALK_MIN_THREADS: usize = 2;

/// How many worker threads the parallel walk should use.
fn walk_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(WALK_MIN_THREADS)
        .clamp(WALK_MIN_THREADS, WALK_MAX_THREADS)
}

/// Everything a walk worker needs to classify one file, shared read-only across
/// the worker threads.
struct WalkCtx {
    /// The source being scanned (its id for logs, and its placeholder policy).
    source: SourceRow,
    /// Absolute source root, for stripping entries to relative paths.
    root: PathBuf,
    /// The SAME matcher the walker prunes with and the orphan split consults.
    matcher: Arc<SourceMatcher>,
    /// The stored `file_state` rows, read-only: a worker only ever looks a path
    /// up to diff `(size, mtime_ns)` and read the stored hash.
    known: Arc<HashMap<RelativePath, FileStateRow>>,
    mode: ScanMode,
    granularity_ns: u64,
    is_coarse: bool,
    last_scan_end_ns: Option<i64>,
    /// Whether to time each file (the reservoir itself lives on the consumer
    /// side, which is what actually records the sample).
    capture_latency: bool,
    /// The backup-work priority (SPEC s22 `io_priority`) each walk worker
    /// thread applies to itself on its first visit, so the scan's stat + hash
    /// I/O yields to foreground applications. Walker threads live only for the
    /// walk (spawned and joined inside `build_parallel().visit()`), so a
    /// one-shot apply with no restore is correct - nothing pooled is demoted.
    priority: WorkPriority,
    /// App-global disk/net/hash byte counters (issue #308 bottleneck
    /// classifier), or `None` when not wired (tests / chaos harness). A
    /// deep-verify (or coarse-fs-fallback) re-hash credits the file's size
    /// here so the bottleneck classifier's cpu rate reflects scan-time
    /// hashing, not just the upload pipeline's.
    io_counters: Option<Arc<crate::iostat::IoCounters>>,
}

/// What a worker concluded about ONE file. Deliberately excludes every
/// order-sensitive decision: whether the path is a duplicate NFC key, and
/// therefore whether any of this is kept, is the consumer's call.
enum Outcome {
    /// A cloud-only (OneDrive Files-On-Demand) placeholder: marked seen so its
    /// stored row is not read as a deletion, then skipped before stat/hash.
    ///
    /// Only Windows carries the `FILE_ATTRIBUTE_RECALL_ON_OPEN` attribute, so
    /// only the Windows branch of [`process_file`] ever produces this; the
    /// consumer handles it unconditionally to keep one code path.
    #[cfg_attr(not(windows), allow(dead_code))]
    Placeholder,
    /// `metadata()` failed: NOT marked seen, and deletions under it suppressed.
    StatFailed,
    /// Stat-matched (and, where required, content-verified): nothing to emit.
    Unchanged,
    /// New (no stored row) or changed (stat differs, or a verify hash mismatch).
    Changed { size: u64, mtime_ns: i64 },
    /// The content-verify read failed: no evidence of a change, but deletions
    /// under this path are suppressed.
    VerifyFailed,
}

/// One fully-processed file as a worker hands it over.
struct FileRecord {
    rel: RelativePath,
    outcome: Outcome,
    /// The file carries named NTFS Alternate Data Streams (Windows only).
    ads: bool,
    /// Per-file processing time, when telemetry capture is armed.
    latency_ms: Option<u64>,
}

/// A worker's accumulated results between two channel hand-offs.
#[derive(Default)]
struct WalkBatch {
    /// Regular (non-symlink) files the walk yielded, counted BEFORE the
    /// include/exclude decision so a long descent still moves the readout.
    visited: u64,
    /// Symlink entries skipped (DESIGN s5.2.1 `SymlinkPolicy::Skip`).
    skipped_symlinks: u64,
    files: Vec<FileRecord>,
    /// Walk errors: `Some(prefix)` when the errored path is attributable to a
    /// relative prefix, `None` when it is not (which suppresses ALL deletions).
    errors: Vec<Option<RelativePath>>,
    /// Paths not representable as a [`RelativePath`], as lossy strings.
    invalid_filenames: Vec<String>,
}

/// Per-thread visitor for the parallel walk.
struct WalkVisitor {
    ctx: Arc<WalkCtx>,
    tx: tokio::sync::mpsc::Sender<WalkBatch>,
    batch: WalkBatch,
    /// Entries accumulated since the last hand-off (the flush trigger).
    items: usize,
    last_flush: Instant,
    /// Set once the consumer has gone away, so the walk stops instead of
    /// traversing the rest of the tree with nowhere to send the results.
    consumer_gone: bool,
    /// Whether this worker thread has applied [`WalkCtx::priority`] to itself
    /// yet. `build()` may run on the coordinating thread, so the apply happens
    /// on the first `visit()` call, which is guaranteed to run on the worker.
    priority_applied: bool,
}

impl WalkVisitor {
    /// Hand the accumulated batch to the consumer, blocking while the bounded
    /// channel is full (that backpressure is the point).
    fn flush(&mut self) {
        if self.items == 0 {
            return;
        }
        self.items = 0;
        self.last_flush = Instant::now();
        // A closed channel means the consumer is already gone - the scan future
        // was dropped mid-walk. Nobody is left to tell, so stop walking rather
        // than burn a disk's worth of I/O on results nothing will read.
        if self
            .tx
            .blocking_send(std::mem::take(&mut self.batch))
            .is_err()
        {
            self.consumer_gone = true;
        }
    }

    fn maybe_flush(&mut self) {
        if self.items >= WALK_BATCH_MAX_ENTRIES
            || self.last_flush.elapsed() >= WALK_BATCH_MAX_INTERVAL
        {
            self.flush();
        }
    }

    /// [`WalkState::Quit`] once the consumer is gone, else `Continue`.
    fn keep_going(&self) -> WalkState {
        if self.consumer_gone {
            WalkState::Quit
        } else {
            WalkState::Continue
        }
    }
}

impl Drop for WalkVisitor {
    /// The last partial batch of a worker thread. Without this the tail of every
    /// worker's work - up to `WALK_BATCH_MAX_ENTRIES` files each - would be lost.
    fn drop(&mut self) {
        self.flush();
    }
}

impl ParallelVisitor for WalkVisitor {
    fn visit(&mut self, result: Result<DirEntry, ignore::Error>) -> WalkState {
        if !self.priority_applied {
            self.priority_applied = true;
            crate::priority::apply_to_current_thread(self.ctx.priority);
        }
        let ctx = Arc::clone(&self.ctx);
        let entry = match result {
            Ok(e) => e,
            Err(err) => {
                // DESIGN s5.2 step 3: never swallow walker errors. Record the
                // errored path's relative prefix (when one is recoverable) so
                // deletions under it are suppressed; with no attributable
                // prefix the consumer suppresses ALL deletions this cycle.
                let prefix = error_path(&err)
                    .and_then(|p| p.strip_prefix(&ctx.root).ok())
                    .and_then(|p| RelativePath::try_from(p).ok());
                tracing::warn!(target: TARGET, source_id = %ctx.source.id, %err, "walk error; suppressing deletes under this subtree");
                self.batch.errors.push(prefix);
                self.items += 1;
                self.maybe_flush();
                return self.keep_going();
            }
        };

        // Symlink check BEFORE the is_file filter: a link to a file can read
        // as a file on some platforms, and we must skip + count it either
        // way (DESIGN s5.2.1 SymlinkPolicy::Skip).
        if entry.file_type().is_some_and(|t| t.is_symlink()) {
            self.batch.skipped_symlinks += 1;
            self.items += 1;
            tracing::trace!(target: TARGET, source_id = %ctx.source.id, path = %entry.path().display(), "skipping symlink");
            self.maybe_flush();
            return self.keep_going();
        }
        // Files only; directories and other non-files are not backed up.
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            return WalkState::Continue;
        }

        // Counted here - BEFORE the include/exclude decision and before the
        // (possibly slow) stat + deep-verify hash - so descending a large
        // excluded tree still moves the counter.
        self.batch.visited += 1;
        self.items += 1;

        let abs = entry.path();
        let rel = match abs
            .strip_prefix(&ctx.root)
            .ok()
            .and_then(|p| RelativePath::try_from(p).ok())
        {
            Some(r) => r,
            None => {
                // Not representable as a RelativePath (e.g. an unpaired UTF-16
                // surrogate that fails UTF-8 conversion). Record it so the
                // orchestrator emits a local.invalid_filename warning (SPEC s24)
                // rather than dropping the file silently; the scan continues.
                let shown = abs.to_string_lossy().into_owned();
                tracing::warn!(target: TARGET, source_id = %ctx.source.id, path = %shown, "local.invalid_filename: skipping path not representable as a relative path");
                self.batch.invalid_filenames.push(shown);
                self.maybe_flush();
                return self.keep_going();
            }
        };

        // Include/exclude decision (DESIGN s5.2): consult the same matcher the
        // orphan split below uses, on the NFC `RelativePath` so both agree. We
        // are past the is_file filter, so `is_dir = false`. An excluded file is
        // simply not backed up - and, crucially, never lands in `seen`, so if a
        // stored `file_state` row exists for it the orphan split classifies it
        // as an excluded-orphan (no trash), not a deletion.
        if !ctx.matcher.is_included(Path::new(rel.as_str()), false) {
            tracing::trace!(target: TARGET, source_id = %ctx.source.id, path = %rel, "excluded by ignore rules");
            self.maybe_flush();
            return self.keep_going();
        }

        let record = process_file(&ctx, &entry, rel);
        self.batch.files.push(record);
        self.maybe_flush();
        self.keep_going()
    }
}

/// Builds one [`WalkVisitor`] per walk worker thread.
struct WalkVisitorBuilder {
    ctx: Arc<WalkCtx>,
    tx: tokio::sync::mpsc::Sender<WalkBatch>,
}

impl<'s> ParallelVisitorBuilder<'s> for WalkVisitorBuilder {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(WalkVisitor {
            ctx: Arc::clone(&self.ctx),
            tx: self.tx.clone(),
            batch: WalkBatch::default(),
            items: 0,
            last_flush: Instant::now(),
            consumer_gone: false,
            priority_applied: false,
        })
    }
}

/// Classify ONE included regular file: stat it, apply the placeholder policy,
/// probe for alternate data streams, and run the change detection (including the
/// BLAKE3 re-hash a deep-verify or coarse-FS pass calls for).
///
/// Runs on a walk worker thread and touches nothing shared but the read-only
/// [`WalkCtx`], which is exactly what lets several files hash concurrently.
fn process_file(ctx: &WalkCtx, entry: &DirEntry, rel: RelativePath) -> FileRecord {
    // Telemetry (DESIGN s13): time this file's stat-through-change-detection
    // processing. Only armed when a reservoir is threaded in AND capture is
    // enabled, so a scan with no telemetry pays nothing (not even the
    // `Instant::now`). The consumer records the sample for a fully-processed
    // file; the early returns below (cloud-only placeholder, stat failure)
    // intentionally record nothing, as does a collider the consumer drops.
    let file_timer = ctx.capture_latency.then(Instant::now);
    let elapsed_ms = |timer: Option<Instant>| {
        timer.map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX))
    };
    let abs = entry.path();

    let meta = match entry.metadata() {
        Ok(m) => m,
        Err(err) => {
            // A stat failure on a single file is a per-file error, not a
            // subtree walk error; suppress this path from deletion (its row, if
            // any, should not be trashed) and move on.
            tracing::warn!(target: TARGET, source_id = %ctx.source.id, path = %abs.display(), %err, "metadata read failed; skipping file");
            return FileRecord {
                rel,
                outcome: Outcome::StatFailed,
                ads: false,
                latency_ms: None,
            };
        }
    };

    // OneDrive Files-On-Demand policy (issue #4, DESIGN s5.2.1): a cloud-only
    // placeholder has FILE_ATTRIBUTE_RECALL_ON_OPEN set; opening it to stat/hash
    // would force a network hydration. Under the default
    // `PlaceholderPolicy::Skip` we skip it, but the consumer FIRST marks it
    // `seen` so the deletion sweep does NOT treat its existing `file_state` row
    // as "known but missing" and trash the Drive backup - a placeholder is still
    // present locally, just dehydrated. Under `ForceDownload` we do NOT skip:
    // the file flows through the normal stat + change-detection path, and the
    // subsequent read (deep-verify hash / upload) hydrates it on demand.
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if should_skip_placeholder(meta.file_attributes(), ctx.source.placeholder_policy) {
            tracing::trace!(target: TARGET, source_id = %ctx.source.id, path = %rel, "skipping cloud-only (OneDrive Files-On-Demand) placeholder");
            return FileRecord {
                rel,
                outcome: Outcome::Placeholder,
                ads: false,
                latency_ms: None,
            };
        }
    }

    let size = meta.len();
    let mtime_ns = mtime_ns(&meta);

    // NTFS Alternate Data Stream detection (DESIGN s5.2.1, SPEC s24
    // `local.ads_skipped`): this file is about to be backed up (main stream
    // only), so if it carries any named data stream record the path. The
    // orchestrator surfaces one warning per affected file; the named streams are
    // NOT uploaded (a documented V1 limitation that must be visible, not
    // silent). Windows-only; a no-op elsewhere.
    #[cfg(windows)]
    let ads = {
        let found = has_alternate_data_streams(abs);
        if found {
            tracing::warn!(target: TARGET, source_id = %ctx.source.id, path = %rel, "local.ads_skipped: file has NTFS alternate data stream(s); only the main stream is backed up");
        }
        found
    };
    #[cfg(not(windows))]
    let ads = false;

    let stored = ctx.known.get(&rel);
    let stat_match = matches!(stored, Some(row) if row.size == size && row.mtime_ns == mtime_ns);

    let outcome = if stat_match {
        // A stat-matched file is unchanged on the fast path UNLESS its content
        // must be verified. Two triggers re-hash it (DESIGN s5.2 step 2):
        // DeepVerify always re-hashes (bit-rot / mtime lies); and on a
        // coarse-granularity filesystem the fallback re-hashes a stat-matched
        // file whose ctime/birth post-dates the last scan or whose mtime is
        // within one granularity window of it - a within-quantum in-place edit
        // reports an unchanged mtime and would otherwise be missed. ctime/birth
        // are read only when the FS is coarse, so a fine-FS fast scan pays
        // nothing.
        let coarse_suspect = ctx.is_coarse
            && coarse_fs_needs_rehash(
                ctx.granularity_ns,
                mtime_ns,
                ctime_ns(&meta),
                birth_ns(&meta),
                ctx.last_scan_end_ns,
            );
        if ctx.mode == ScanMode::DeepVerify || coarse_suspect {
            let stored_hash = stored.map(|r| r.hash_blake3);
            let hash_result = hash_file(abs, ctx.priority);
            // issue #308: credit the WHOLE file's bytes on a successful hash,
            // win or lose the comparison below - the bottleneck classifier
            // cares that hashing happened, not what it concluded. A failed
            // read (handled in the `Err` arm) credits nothing, matching the
            // "no evidence" treatment the rest of this branch already gives it.
            if hash_result.is_ok() {
                if let Some(io) = ctx.io_counters.as_ref() {
                    io.add_hashed(size);
                }
            }
            match hash_result {
                Ok(hash) if stored_hash != Some(hash) => {
                    let reason = if ctx.mode == ScanMode::DeepVerify {
                        "deep-verify"
                    } else {
                        "coarse-fs mtime-granularity fallback"
                    };
                    tracing::info!(target: TARGET, source_id = %ctx.source.id, path = %rel, reason, "hash mismatch; marking changed");
                    Outcome::Changed { size, mtime_ns }
                }
                Ok(_) => Outcome::Unchanged,
                Err(err) => {
                    // Could not read to verify: don't claim changed (no
                    // evidence) but suppress this path from deletion.
                    tracing::warn!(target: TARGET, source_id = %ctx.source.id, path = %rel, %err, "content-verify read failed; skipping file");
                    Outcome::VerifyFailed
                }
            }
        } else {
            // A fast-path stat-match: unchanged, nothing to emit.
            Outcome::Unchanged
        }
    } else {
        // New (no row) or changed (stat differs).
        Outcome::Changed { size, mtime_ns }
    };

    FileRecord {
        rel,
        outcome,
        ads,
        latency_ms: elapsed_ms(file_timer),
    }
}

/// [`scan_with_latency`] plus an optional [`ScanProgressSink`] the walk ticks as
/// it advances (throttled to [`SCAN_PROGRESS_FILE_STRIDE`] files /
/// [`SCAN_PROGRESS_MIN_INTERVAL`], with a final tick carrying the total once the
/// walk ends). Passing `None` is exactly the old behaviour at zero cost.
pub async fn scan_with_progress(
    source: &SourceRow,
    state: &dyn StateRepo,
    mode: ScanMode,
    latency: Option<&crate::telemetry::LatencyReservoir>,
    on_progress: Option<&ScanProgressSink<'_>>,
) -> anyhow::Result<ScanResult> {
    scan_with_priority(
        source,
        state,
        mode,
        latency,
        on_progress,
        WorkPriority::Normal,
        None,
    )
    .await
}

/// [`scan_with_progress`] plus the backup-work [`WorkPriority`] (SPEC s22
/// `io_priority`) the walk should run at. The blocking coordinator task takes an
/// RAII [`begin_background_work`](crate::priority::begin_background_work) guard
/// (its thread is POOLED, so the demotion must be undone), and each transient
/// walk worker thread applies the priority once on its first visit (no restore
/// needed - workers die with the walk). `Normal` is exactly the old behaviour;
/// the orchestrator passes its live [`PriorityCell`](crate::priority::PriorityCell)
/// value so a settings change applies from the next scan.
pub async fn scan_with_priority(
    source: &SourceRow,
    state: &dyn StateRepo,
    mode: ScanMode,
    latency: Option<&crate::telemetry::LatencyReservoir>,
    on_progress: Option<&ScanProgressSink<'_>>,
    priority: WorkPriority,
    // issue #308: the app-global disk/net/hash byte counters, so a
    // deep-verify (or coarse-fs fallback) re-hash credits the SAME hash
    // counter the upload pipeline's cpu stage does. `None` in tests / the
    // chaos harness - the walk still runs, it just credits nothing.
    io_counters: Option<Arc<crate::iostat::IoCounters>>,
) -> anyhow::Result<ScanResult> {
    let known = state
        .load_source_file_state(source.id)
        .await
        .with_context(|| format!("loading file_state for source {}", source.id))?;

    let root = Path::new(&source.local_path);

    // Root pre-check (DESIGN s5.2 step 3, mass-delete guard): if the source
    // root is missing or not a directory - an unmounted external drive, a
    // dropped network share - the walk yields zero files and a naive diff
    // would report EVERY known path as deleted, cascading into trashing the
    // whole Drive backup. Refuse: report nothing new and nothing deleted,
    // log, and let the next scan retry once the root is back.
    match std::fs::metadata(root) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            tracing::warn!(target: TARGET, source_id = %source.id, path = %root.display(), "source root is not a directory; skipping scan (no deletions)");
            return Ok(ScanResult::default());
        }
        Err(err) => {
            tracing::warn!(target: TARGET, source_id = %source.id, path = %root.display(), %err, "source root unreadable/missing; skipping scan (no deletions)");
            return Ok(ScanResult::default());
        }
    }

    // FS mtime-granularity (DESIGN s5.2 step 2). Read the persisted per-source
    // value; when unprobed (`None`) the write-stat probe (on `spawn_blocking` -
    // it may sleep on a coarse FS) measures it and hands the result back to the
    // orchestrator via `ScanResult::probed_granularity_ns` to persist.
    // `granularity_ns` is the window used for THIS scan (0 = fine); a probe I/O
    // failure degrades to fine for this cycle and is NOT persisted, so a later
    // scan re-probes.
    //
    // The probe is DEFERRED off the very first scan of a source. That first scan
    // is the source's initial backup cycle, where every file is new and each
    // path's FIRST upload must commit its `file_state` row cleanly; adding the
    // probe's stat/sleep I/O (up to ~2.2s on a coarse FS) plus temp-file churn
    // in the source root to that hot path is exactly the latency we don't want
    // while the first creates are landing. So the first scan (no completed scan
    // yet -> `last_full_scan_at` is `None`) TRUSTS mtime with no probe and no
    // coarse fallback, persisting nothing; the probe runs on the NEXT scan, once
    // a completed scan exists. This mirrors the probe-failure degradation below
    // ("trust mtime this cycle, re-probe next") and costs at most a one-cycle
    // delay before the coarse fallback first engages on a coarse FS.
    let (granularity_ns, probed_granularity_ns): (u64, Option<i64>) = match source
        .mtime_granularity_ns
    {
        Some(stored) => (u64::try_from(stored).unwrap_or(0), None),
        // First scan of the source: defer the probe (see above).
        None if source.last_full_scan_at.is_none() => (0, None),
        None => {
            let root_owned = root.to_path_buf();
            match tokio::task::spawn_blocking(move || probe_mtime_granularity(&root_owned)).await {
                Ok(Ok(g)) => {
                    tracing::debug!(target: TARGET, source_id = %source.id, granularity_ns = g, coarse = granularity_is_coarse(g), "probed fs mtime granularity");
                    (g, Some(i64::try_from(g).unwrap_or(i64::MAX)))
                }
                Ok(Err(err)) => {
                    tracing::debug!(target: TARGET, source_id = %source.id, %err, "fs mtime-granularity probe failed; trusting mtime this cycle (will re-probe)");
                    (0, None)
                }
                Err(err) => {
                    tracing::warn!(target: TARGET, source_id = %source.id, %err, "fs mtime-granularity probe task join failed; trusting mtime this cycle");
                    (0, None)
                }
            }
        }
    };
    let is_coarse = granularity_is_coarse(granularity_ns);
    // The last-scan-end boundary the coarse-FS window is measured against
    // (DESIGN s5.2 step 2). `last_full_scan_at` is Unix millis; convert to ns to
    // compare against file mtime/ctime/birth. `None` before the first completed
    // scan -> the fallback re-hashes conservatively.
    let last_scan_end_ns: Option<i64> = source
        .last_full_scan_at
        .map(|ms| ms.saturating_mul(1_000_000));

    let known = Arc::new(known);

    let mut seen: HashSet<RelativePath> = HashSet::new();
    let mut new_or_changed: Vec<LocalEntry> = Vec::new();
    // NFC collision keys: a second distinct raw path that normalises onto an
    // already-seen `RelativePath` (DESIGN s5.2.3, SPEC s24
    // `local.unicode_collision`). We keep the first file, drop the later one,
    // and record the key here instead of emitting a duplicate op.
    let mut collisions: Vec<RelativePath> = Vec::new();
    // Count of cloud-only (OneDrive Files-On-Demand) placeholders skipped
    // before stat/hash (DESIGN s5.2.1). Only Windows ever produces the
    // `Outcome::Placeholder` that increments it; the attribute does not exist
    // on other targets.
    let mut skipped_cloud_only: u64 = 0;
    // Directory prefixes (relative, `/`-joined) under which the walk hit an
    // error. Any known path under one of these is held back from `deleted`.
    let mut errored_prefixes: Vec<RelativePath> = Vec::new();
    // Set when a walk error could NOT be attributed to a recoverable
    // relative prefix (e.g. a root-level error, or one whose path does not
    // strip under the root). Such an error means our view of the tree is
    // incomplete in an unknown region, so we cannot safely propagate ANY
    // deletion this cycle (DESIGN s5.2 step 3).
    let mut unattributed_error = false;
    let mut skipped_symlinks: u64 = 0;
    // Files whose named NTFS Alternate Data Streams are NOT backed up
    // (DESIGN s5.2.1, SPEC s24 `local.ads_skipped`). Populated only on
    // Windows + NTFS; the orchestrator turns each into a one-per-file
    // warning so the dropped stream is not silent data loss.
    let mut ads_skipped: Vec<RelativePath> = Vec::new();
    // Local paths skipped because they are not representable as a RelativePath
    // (e.g. an unpaired UTF-16 surrogate name, SPEC s24 `local.invalid_filename`).
    // The orchestrator surfaces each as a one-per-path warning so the omission
    // is visible rather than silent.
    let mut invalid_filenames: Vec<String> = Vec::new();

    // Live scan-progress state (see [`ScanProgressSink`]). `visited` counts every
    // regular file the walk yields; `reported` / `last_tick` throttle the sink so
    // a huge tree ticks a few times a second, not once per file.
    let mut visited: u64 = 0;
    let mut reported: u64 = 0;
    let mut last_tick = Instant::now();

    // The walk phase runs entirely OFF the async runtime (see the module docs).
    // The blocking task builds the matcher - a full (pruned) pass over the tree
    // to discover its nested ignore files, easily seconds on a large source -
    // and only then walks, so after the cheap root + probe checks the very next
    // thing this function does is AWAIT. That is what keeps IPC responsive from
    // t=0 instead of the app freezing at "Loading..." as a scan starts.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<WalkBatch>(WALK_CHANNEL_DEPTH);
    let walk_source = source.clone();
    let walk_root = root.to_path_buf();
    let walk_known = Arc::clone(&known);
    let capture_latency = latency.is_some_and(|r| r.is_enabled());
    let walk_task = tokio::task::spawn_blocking(move || -> anyhow::Result<Arc<SourceMatcher>> {
        // This spawn_blocking thread is POOLED, so the demotion is held in an
        // RAII guard that restores normal priority when the walk ends; the
        // matcher build's ignore-file collection I/O runs demoted too.
        let _priority_guard = crate::priority::begin_background_work(priority);
        // ONE matcher for the whole scan: the walker's prune predicate, every
        // per-entry include check, and the orphan split share this `Arc` rather
        // than each rebuilding the cascade (the walker used to build a second,
        // identical one of its own).
        let matcher = Arc::new(build_source_matcher(&walk_source)?);
        let ctx = Arc::new(WalkCtx {
            source: walk_source.clone(),
            root: walk_root,
            matcher: Arc::clone(&matcher),
            known: walk_known,
            mode,
            granularity_ns,
            is_coarse,
            last_scan_end_ns,
            capture_latency,
            priority,
            io_counters,
        });
        let mut wb = build_walker_with_matcher(&walk_source, Arc::clone(&matcher));
        wb.threads(walk_threads());
        let mut builder = WalkVisitorBuilder { ctx, tx };
        wb.build_parallel().visit(&mut builder);
        // The builder holds the last sender clone; dropping it closes the
        // channel, which is what ends the consumer loop below.
        drop(builder);
        Ok(matcher)
    });

    // The single consumer. Everything ORDER-SENSITIVE lives here: the `seen`
    // set, NFC collision detection, the progress throttle, the errored-prefix
    // bookkeeping and the latency reservoir. Awaiting the channel is the yield
    // point that lets the orchestrator's tick consumers run mid-scan.
    while let Some(batch) = rx.recv().await {
        visited += batch.visited;
        skipped_symlinks += batch.skipped_symlinks;
        for err in batch.errors {
            match err {
                Some(prefix) => errored_prefixes.push(prefix),
                None => unattributed_error = true,
            }
        }
        invalid_filenames.extend(batch.invalid_filenames);

        for record in batch.files {
            let FileRecord {
                rel,
                outcome,
                ads,
                latency_ms,
            } = record;
            match outcome {
                // A stat failure never marks the path seen (matching the
                // pre-parallel scanner): the deletion suppression alone keeps
                // its stored row out of `deleted`.
                Outcome::StatFailed => errored_prefixes.push(rel),
                // A placeholder IS marked seen - it is present locally, just
                // dehydrated - but without a collision check, again matching
                // the pre-parallel scanner.
                Outcome::Placeholder => {
                    seen.insert(rel);
                    skipped_cloud_only += 1;
                }
                other => {
                    // NFC collision detection (DESIGN s5.2.3, SPEC s24
                    // local.unicode_collision): `RelativePath::try_from`
                    // NFC-normalised `rel`, so two byte-distinct raw paths can
                    // collapse to one key. Keep the first file, record + drop
                    // the later collider rather than emitting a duplicate upload
                    // op for the same `file_state` key. Only this consumer can
                    // make that call - a worker cannot know which key another
                    // thread already claimed.
                    //
                    // Probed with `contains` rather than `insert` so the path can
                    // be MOVED into whichever collection claims it below. The
                    // overwhelmingly common outcome on an incremental scan is
                    // `Unchanged`, which owns `rel` outright - hashing twice is
                    // cheaper than cloning a path per file across a 600k-file
                    // source.
                    if seen.contains(&rel) {
                        tracing::warn!(target: TARGET, source_id = %source.id, path = %rel, "local.unicode_collision: distinct raw paths normalise to one NFC key; dropping later duplicate");
                        collisions.push(rel);
                        continue;
                    }
                    if ads {
                        ads_skipped.push(rel.clone());
                    }
                    match other {
                        Outcome::Changed { size, mtime_ns } => {
                            seen.insert(rel.clone());
                            new_or_changed.push(LocalEntry {
                                rel,
                                size,
                                mtime_ns,
                            });
                        }
                        Outcome::VerifyFailed => {
                            seen.insert(rel.clone());
                            errored_prefixes.push(rel);
                        }
                        // Unchanged: nothing to emit, so the path is consumed by
                        // the `seen` set alone.
                        _ => {
                            seen.insert(rel);
                        }
                    }
                    // Telemetry (DESIGN s13): the worker timed the file, the
                    // reservoir lives here. `record_scan_ms` re-checks the
                    // enable gate (a no-op if telemetry was disabled mid-scan).
                    if let (Some(res), Some(ms)) = (latency, latency_ms) {
                        res.record_scan_ms(ms);
                    }
                }
            }
        }

        // Live progress tick, on the same stride / interval throttle as before -
        // plus one on the FIRST batch that carries anything, so the readout
        // starts moving as soon as the walk produces something instead of
        // waiting out a whole stride on a fast tree.
        if let Some(sink) = on_progress {
            if visited > 0
                && (reported == 0
                    || visited - reported >= SCAN_PROGRESS_FILE_STRIDE
                    || last_tick.elapsed() >= SCAN_PROGRESS_MIN_INTERVAL)
            {
                reported = visited;
                last_tick = Instant::now();
                sink(visited).await;
                // Awaiting the sink is NOT by itself a yield: the orchestrator's
                // sink resolves without ever returning Pending (an uncontended
                // lock write plus a broadcast send). Yield explicitly so the
                // consumers of the event we just emitted are actually scheduled.
                tokio::task::yield_now().await;
            }
        }
    }

    // Surface a matcher-build failure or a panicked walk thread rather than
    // silently treating an aborted walk as "the tree is empty" - which the
    // orphan split below would read as a mass deletion.
    let matcher = walk_task
        .await
        .context("scan walk task failed")?
        .context("building the source matcher for the scan")?;

    // Final progress tick: the walk is done, so report the true total even when
    // the last stride/interval boundary fell short of it. Always emitted (even
    // for a zero-file source) so a consumer can render a settled count rather
    // than a stale partial one.
    if let Some(sink) = on_progress {
        sink(visited).await;
        // Same reason as the in-walk ticks: hand control back so the settled
        // count reaches its consumers BEFORE the plan/execute phases start
        // emitting their own (far noisier) events.
        tokio::task::yield_now().await;
    }

    // Split the known-but-not-seen paths into genuine deletions vs
    // excluded-orphans (DESIGN s5.5). A `file_state` key the walk did not
    // yield is either (a) genuinely gone from disk -> `deleted` (planner
    // trashes), or (b) still present locally but now EXCLUDED by the current
    // ignore rules -> `excluded_orphan` (planner emits NO trash; an
    // ignore-rule change must never delete a backed-up file). A `file_state`
    // key is always a file, so consult the matcher with `is_dir = false`.
    //
    // The error-suppression guard applies ONLY to `deleted`: trashing is the
    // destructive action, so a transient permission error must hold it back.
    // Classifying an orphan is non-destructive (no op either way), so it is
    // safe even mid-error and is not gated.
    //
    // Runs on a blocking thread like the walk: a source with 600k stored rows
    // means 600k matcher queries here, which would otherwise seize the runtime
    // thread the moment the walk finally released it.
    let errored_prefix_count = errored_prefixes.len();
    let split_matcher = Arc::clone(&matcher);
    let split_known = Arc::clone(&known);
    let (deleted, excluded_orphans) = tokio::task::spawn_blocking(move || {
        let mut deleted: Vec<RelativePath> = Vec::new();
        let mut excluded_orphans: Vec<RelativePath> = Vec::new();
        for path in split_known.keys().filter(|p| !seen.contains(*p)) {
            if !split_matcher.is_included(Path::new(path.as_str()), false) {
                // (b) excluded by a (possibly new) ignore rule - not a deletion.
                excluded_orphans.push(path.clone());
                continue;
            }
            // (a) still included, so genuinely missing from disk -> a deletion,
            // subject to the walk-error suppression.
            if unattributed_error || is_under_any(path, &errored_prefixes) {
                continue;
            }
            deleted.push(path.clone());
        }
        (deleted, excluded_orphans)
    })
    .await
    .context("scan orphan-split task failed")?;

    tracing::debug!(
        target: TARGET,
        source_id = %source.id,
        ?mode,
        visited,
        new_or_changed = new_or_changed.len(),
        deleted = deleted.len(),
        excluded_orphans = excluded_orphans.len(),
        skipped_symlinks,
        skipped_cloud_only,
        collisions = collisions.len(),
        ads_skipped = ads_skipped.len(),
        invalid_filenames = invalid_filenames.len(),
        errored_prefixes = errored_prefix_count,
        unattributed_error,
        granularity_ns,
        is_coarse,
        "scan complete"
    );

    Ok(ScanResult {
        new_or_changed,
        deleted,
        collisions,
        excluded_orphans,
        ads_skipped,
        invalid_filenames,
        probed_granularity_ns,
    })
}

/// Modification time in signed nanoseconds since the Unix epoch.
///
/// Matches `file_state.mtime_ns` (SPEC s2) and [`LocalEntry::mtime_ns`].
/// Signed so a pre-epoch mtime is representable rather than clamped, which
/// keeps the equality test against a stored pre-epoch value exact. A
/// platform that cannot report an mtime yields `0` (epoch), which simply
/// makes the file look changed until a real mtime is observed - safe, since
/// the failure mode is "upload again", never "miss a change".
fn mtime_ns(meta: &Metadata) -> i64 {
    let modified = match meta.modified() {
        Ok(t) => t,
        Err(_) => return 0,
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        // Pre-epoch mtime: negate the magnitude of the reverse duration.
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

/// One second in nanoseconds - the DESIGN s5.2 threshold above which a
/// filesystem's mtime granularity is "coarse" and the re-hash fallback engages.
const ONE_SECOND_NS: u64 = 1_000_000_000;

/// The wall-clock ceiling (~2.2s) the [`probe_mtime_granularity`] phase-2 loop
/// spends waiting for a coarse filesystem's mtime to advance. Sized so even a
/// 2s-resolution FAT32 volume resolves in a single probe; a filesystem that
/// still has not ticked by then is classified at least this coarse.
const PROBE_BUDGET_NS: u64 = 2_200_000_000;

/// Whether a measured granularity window (ns) means the filesystem is coarse
/// enough to need the re-hash fallback (DESIGN s5.2 step 2: strictly greater
/// than one second). Pure so the boundary is unit-testable.
fn granularity_is_coarse(granularity_ns: u64) -> bool {
    granularity_ns > ONE_SECOND_NS
}

/// Classify a probe observation into an effective mtime-granularity window (ns).
///
/// Pure decision half of [`probe_mtime_granularity`] (mirrors the repo's
/// `classify_*` pattern): `Some(gap)` is the elapsed span after which the
/// probe's rewrite first produced a distinct reported mtime - that span bounds
/// the granularity. `None` means the reported mtime never advanced within the
/// probe budget, so the filesystem is at least [`PROBE_BUDGET_NS`] coarse.
fn classify_granularity_ns(observed_gap_ns: Option<u64>) -> u64 {
    observed_gap_ns.unwrap_or(PROBE_BUDGET_NS)
}

/// Probes the effective mtime granularity (ns) of the filesystem backing
/// `root`, via the DESIGN s5.2 write-stat probe. Returns `0` for a fine
/// (sub-second) filesystem whose exact mtime can be trusted, or the measured
/// coarse window otherwise.
///
/// Blocking (does file I/O and, on a coarse FS, sleeps up to ~[`PROBE_BUDGET_NS`]);
/// the async scanner runs it on `spawn_blocking`. A temp file is written INTO
/// `root` so the probe measures the source's own filesystem (a system temp dir
/// is often a different volume); it is `*.tmp`-named (a default exclude) and
/// RAII-deleted so a mid-probe error never leaks it or feeds the walk.
fn probe_mtime_granularity(root: &Path) -> std::io::Result<u64> {
    use std::io::Write;

    let path = root.join(format!(".driven-mtime-probe-{}.tmp", uuid::Uuid::new_v4()));

    // RAII cleanup: remove the temp file on every exit path (success, error, or
    // panic) so it is gone before the walk and never leaks on disk.
    struct Cleanup<'a>(&'a Path);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }

    let rewrite = |byte: u8| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        f.write_all(&[byte])?;
        f.flush()?;
        Ok(())
    };
    let sample = || -> std::io::Result<i64> { Ok(mtime_ns(&std::fs::metadata(&path)?)) };

    rewrite(0)?;
    let _cleanup = Cleanup(&path);
    let m0 = sample()?;

    // Phase 1: a short burst of back-to-back rewrites, no sleep. If the reported
    // mtime changes across writes only microseconds apart, the FS resolves finer
    // than a handful of syscalls -> fine (window 0). This is the common case and
    // costs microseconds.
    for _ in 0..8 {
        rewrite(1)?;
        if sample()? != m0 {
            return Ok(0);
        }
    }

    // Phase 2: the reported mtime is stuck after a rapid burst -> coarse. Rewrite
    // after growing sleeps until it advances; the elapsed span bounds the
    // granularity window. Bounded by PROBE_BUDGET_NS.
    let start = std::time::Instant::now();
    for step_ms in [2u64, 8, 32, 128, 512, 1024, 512] {
        std::thread::sleep(std::time::Duration::from_millis(step_ms));
        rewrite(2)?;
        if sample()? != m0 {
            let elapsed_ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
            return Ok(classify_granularity_ns(Some(elapsed_ns)));
        }
    }
    Ok(classify_granularity_ns(None))
}

/// The file's ctime (inode change time) in ns since the epoch, where the OS
/// exposes it. Unix-only (`MetadataExt::ctime`); Windows has no ctime (its
/// "creation time" is the inode BIRTH time, handled by [`birth_ns`]), so this
/// returns `None` there. Used LIVE by [`coarse_fs_needs_rehash`] as a
/// "changed since the last scan" signal (DESIGN s5.2 step 2); never stored.
#[cfg(unix)]
fn ctime_ns(meta: &Metadata) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    Some(
        meta.ctime()
            .saturating_mul(1_000_000_000)
            .saturating_add(meta.ctime_nsec()),
    )
}

#[cfg(not(unix))]
fn ctime_ns(_meta: &Metadata) -> Option<i64> {
    None
}

/// The file's inode birth (creation) time in ns since the epoch, where the OS
/// and filesystem record it (`Metadata::created`, which errs on filesystems
/// that do not). Cross-platform best-effort; `None` when unavailable. Also a
/// live signal for [`coarse_fs_needs_rehash`], never stored.
fn birth_ns(meta: &Metadata) -> Option<i64> {
    let created = meta.created().ok()?;
    match created.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).ok(),
        Err(e) => i64::try_from(e.duration().as_nanos()).ok().map(|n| -n),
    }
}

/// The coarse-filesystem re-hash fallback decision (DESIGN s5.2 step 2), as a
/// pure function so it is table-testable with synthetic timestamps (mirroring
/// the repo's `fallback_decision` / `classify_*` pure-decision pattern).
///
/// A file whose `(size, mtime_ns)` already matches its stored `file_state` row
/// looks unchanged on the fast path. But on a filesystem whose mtime resolution
/// is coarser than one second, an in-place edit that preserves the byte count
/// and lands in the SAME mtime quantum reports an unchanged mtime and would be
/// missed. For such a filesystem the scanner re-hashes a stat-matched file when
/// ANY of these hold:
///   - its ctime (inode change time, where the OS exposes it) post-dates the
///     last-scan-end - it was touched since we last looked; or
///   - its inode birth time (where available) post-dates the last-scan-end; or
///   - its mtime falls within one granularity window of the last-scan-end - an
///     edit around the previous scan boundary can share that scan's quantum.
///
/// `last_scan_end_ns` is `None` before the first completed scan (or after a
/// failed cycle left it unadvanced); with no boundary to bound the window the
/// scanner conservatively re-hashes (an extra hash beats a missed edit).
///
/// On a fine filesystem (`granularity_ns <= 1s`) this always returns `false`:
/// exact `(size, mtime)` equality is trusted and nothing is re-hashed, so a
/// fine-FS scan pays zero extra cost.
fn coarse_fs_needs_rehash(
    granularity_ns: u64,
    file_mtime_ns: i64,
    file_ctime_ns: Option<i64>,
    file_birth_ns: Option<i64>,
    last_scan_end_ns: Option<i64>,
) -> bool {
    if !granularity_is_coarse(granularity_ns) {
        return false;
    }
    let Some(scan_end) = last_scan_end_ns else {
        // No prior scan boundary to compare against - re-hash to be safe.
        return true;
    };
    if file_ctime_ns.is_some_and(|c| c > scan_end) {
        return true;
    }
    if file_birth_ns.is_some_and(|b| b > scan_end) {
        return true;
    }
    let window = i64::try_from(granularity_ns).unwrap_or(i64::MAX);
    file_mtime_ns.saturating_sub(scan_end).saturating_abs() <= window
}

/// Extracts the offending path from an [`ignore::Error`], if it carries one.
///
/// `ignore::Error` exposes no public `path()` accessor; the path lives in
/// the public [`ignore::Error::WithPath`] variant. Walk errors are commonly
/// wrapped (`WithDepth` / `WithLineNumber` around `WithPath`, or a `Partial`
/// bundle), so this recurses through the wrapper variants rather than
/// matching only the top level - a top-level-only match would drop the path
/// for a wrapped error and wrongly flip it to `unattributed_error`,
/// suppressing every deletion that cycle (DESIGN s5.2 step 3). The leaf
/// variants that carry no path (`Io`, `Glob`, `Loop`, etc.) yield `None`,
/// which the caller treats as an unattributable error - the conservative
/// choice (suppress all deletes this cycle).
fn error_path(err: &ignore::Error) -> Option<&Path> {
    match err {
        ignore::Error::WithPath { path, .. } => Some(path.as_path()),
        ignore::Error::WithLineNumber { err, .. } | ignore::Error::WithDepth { err, .. } => {
            error_path(err)
        }
        ignore::Error::Partial(errs) => errs.iter().find_map(error_path),
        _ => None,
    }
}

/// BLAKE3 hash of a file's bytes, read in bounded chunks (DESIGN s3.3
/// deep-verify). Streams so memory stays bounded regardless of file size.
fn hash_file(path: &Path, priority: WorkPriority) -> anyhow::Result<[u8; 32]> {
    // DESIGN s5.3: open through the shared platform helper, NOT a bare
    // `File::open`, so this read gets the same two guarantees the executor's
    // read path has:
    //
    // - The explicit `FILE_SHARE_READ | WRITE | DELETE` share mode, stated by
    //   Driven rather than inherited from a std default, so a deep-verify -
    //   which holds the handle for as long as it takes to stream the whole
    //   file - provably never blocks the user's own editor from saving over,
    //   renaming, or deleting the file.
    // - The SPEC s22 `io_priority` hint on the HANDLE. This is the part that
    //   was actually missing: on Windows `WorkPriority::Low` demotes only CPU
    //   priority at the thread level, so a deep-verify's hash reads ran at
    //   NORMAL I/O priority no matter what `io_priority` was set to, while the
    //   executor's uploads honoured it. `Idle` was already covered (background
    //   mode floors thread I/O priority too), so this closes the `low` gap.
    let mut file = crate::platform_open::open_read_shared(path, priority)
        .with_context(|| format!("opening {} for deep-verify", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {} for deep-verify", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// True if `path` equals, or is a descendant of, any prefix in `prefixes`.
///
/// Compares on `/`-separated path segments so a prefix `a/b` matches
/// `a/b/c.txt` but NOT the sibling `a/bc.txt` (a raw `starts_with` on the
/// string would wrongly match the latter).
fn is_under_any(path: &RelativePath, prefixes: &[RelativePath]) -> bool {
    prefixes.iter().any(|prefix| is_under(path, prefix))
}

fn is_under(path: &RelativePath, prefix: &RelativePath) -> bool {
    let p = path.as_str();
    let pre = prefix.as_str();
    if p == pre {
        return true;
    }
    // Segment-boundary descendant check.
    let pb = PathBuf::from(p);
    let preb = PathBuf::from(pre);
    pb.starts_with(&preb)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    /// `Zone.Identifier` alone must NOT flag a file (log-noise follow-up to
    /// the 2026-08-14 incident, where it accounted for ~all `ads_skipped`
    /// warnings); any OTHER named stream still must, with or without a
    /// Zone.Identifier alongside. Skips gracefully when the temp volume
    /// cannot hold ADS (non-NTFS).
    #[cfg(windows)]
    #[test]
    fn ads_probe_exempts_zone_identifier_only_files() {
        let dir = tempfile::tempdir().unwrap();

        let plain = dir.path().join("plain.txt");
        fs::write(&plain, b"body").unwrap();
        assert!(!has_alternate_data_streams(&plain), "no streams, no flag");

        let zoned = dir.path().join("zoned.txt");
        fs::write(&zoned, b"body").unwrap();
        if fs::write(
            format!("{}:Zone.Identifier", zoned.display()),
            b"[ZoneTransfer]\r\nZoneId=3\r\n",
        )
        .is_err()
        {
            eprintln!("temp volume does not support ADS; skipping");
            return;
        }
        assert!(
            !has_alternate_data_streams(&zoned),
            "Zone.Identifier alone is exempt (mark-of-the-web metadata)"
        );

        // NTFS stream names are case-insensitive; the exemption must be too.
        let cased = dir.path().join("cased.txt");
        fs::write(&cased, b"body").unwrap();
        fs::write(format!("{}:zone.identifier", cased.display()), b"x").unwrap();
        assert!(
            !has_alternate_data_streams(&cased),
            "the exemption is case-insensitive"
        );

        let secret = dir.path().join("secret.txt");
        fs::write(&secret, b"body").unwrap();
        fs::write(format!("{}:secret", secret.display()), b"hidden").unwrap();
        assert!(
            has_alternate_data_streams(&secret),
            "a real named stream still flags"
        );

        let both = dir.path().join("both.txt");
        fs::write(&both, b"body").unwrap();
        fs::write(format!("{}:Zone.Identifier", both.display()), b"x").unwrap();
        fs::write(format!("{}:extra", both.display()), b"y").unwrap();
        assert!(
            has_alternate_data_streams(&both),
            "Zone.Identifier must not mask another named stream"
        );
    }
    use crate::state::{
        AccountRow, ActivityFilter, ActivityPage, FileSearchHit, FileStateRow, NewActivity,
        NewPendingOp, PageRequest, PendingOpRow,
    };
    use crate::types::{
        AccountId, AccountState, ActivityId, FileStateStatus, PendingOpId, SourceId,
    };

    fn source_at(root: &Path) -> SourceRow {
        SourceRow {
            id: SourceId::new_v4(),
            account_id: AccountId::new_v4(),
            display_name: "t".into(),
            enabled: true,
            local_path: root.to_string_lossy().into_owned(),
            drive_folder_id: "f".into(),
            drive_id: None,
            drive_folder_path: "/f".into(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            mtime_granularity_ns: None,
            created_at: 0,
        }
    }

    fn write(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, contents).expect("write");
    }

    /// Minimal in-memory `StateRepo` covering only the methods the scanner
    /// calls (`load_source_file_state`); the rest are unreachable in these
    /// tests and bail loudly if ever hit.
    #[derive(Default)]
    struct FakeState {
        files: Mutex<HashMap<(SourceId, RelativePath), FileStateRow>>,
    }

    impl FakeState {
        fn put(&self, row: FileStateRow) {
            self.files
                .lock()
                .unwrap()
                .insert((row.source_id, row.relative_path.clone()), row);
        }
    }

    fn row(source: SourceId, rel: &str, size: u64, mtime_ns: i64, hash: [u8; 32]) -> FileStateRow {
        FileStateRow {
            source_id: source,
            relative_path: RelativePath::try_from(rel.to_string()).unwrap(),
            size,
            mtime_ns,
            hash_blake3: hash,
            drive_file_id: Some("drive-id".into()),
            drive_md5: None,
            encrypted_remote_path: None,
            status: FileStateStatus::Synced,
            last_uploaded_at: None,
            last_verified_at: None,
        }
    }

    #[async_trait]
    impl StateRepo for FakeState {
        async fn load_source_file_state(
            &self,
            source: SourceId,
        ) -> anyhow::Result<HashMap<RelativePath, FileStateRow>> {
            Ok(self
                .files
                .lock()
                .unwrap()
                .iter()
                .filter(|((s, _), _)| *s == source)
                .map(|((_, p), r)| (p.clone(), r.clone()))
                .collect())
        }

        async fn list_accounts(&self) -> anyhow::Result<Vec<AccountRow>> {
            unimplemented!("not used by scanner tests")
        }
        async fn upsert_account(&self, _row: &AccountRow) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn mark_account_state(
            &self,
            _id: AccountId,
            _state: AccountState,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn account_state(&self, _id: AccountId) -> anyhow::Result<Option<AccountState>> {
            unimplemented!()
        }
        async fn mark_account_synced(&self, _id: AccountId, _at: i64) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn delete_account(&self, _id: AccountId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn list_sources(&self) -> anyhow::Result<Vec<SourceRow>> {
            unimplemented!()
        }
        async fn list_enabled_sources_for(
            &self,
            _account: AccountId,
        ) -> anyhow::Result<Vec<SourceRow>> {
            unimplemented!()
        }
        async fn upsert_source(&self, _row: &SourceRow) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn mark_source_scanned(
            &self,
            _id: SourceId,
            _full_scan_at: i64,
            _deep_verify_at: Option<i64>,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn set_source_mtime_granularity(
            &self,
            _id: SourceId,
            _granularity_ns: i64,
        ) -> anyhow::Result<()> {
            unimplemented!("not used by scanner tests")
        }
        async fn delete_source(&self, _id: SourceId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn get_file_state(
            &self,
            _source: SourceId,
            _path: &RelativePath,
        ) -> anyhow::Result<Option<FileStateRow>> {
            unimplemented!()
        }
        async fn upsert_file_state(&self, _row: &FileStateRow) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn delete_file_state(
            &self,
            _source: SourceId,
            _path: &RelativePath,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn clear_file_state_drive_file_id(
            &self,
            _source: SourceId,
            _path: &RelativePath,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn bump_checksum_mismatch_count(
            &self,
            _source: SourceId,
            _path: &RelativePath,
        ) -> anyhow::Result<u32> {
            unimplemented!("not used by scanner tests")
        }
        async fn clear_checksum_mismatch_count(
            &self,
            _source: SourceId,
            _path: &RelativePath,
        ) -> anyhow::Result<()> {
            unimplemented!("not used by scanner tests")
        }
        async fn mark_excluded_orphans(
            &self,
            _source: SourceId,
            _paths: &[RelativePath],
        ) -> anyhow::Result<u64> {
            unimplemented!("not used by scanner tests")
        }
        async fn enqueue_pending_op(&self, _row: NewPendingOp) -> anyhow::Result<PendingOpId> {
            unimplemented!()
        }
        async fn get_pending_ops_due(
            &self,
            _now_ms: i64,
            _limit: u32,
        ) -> anyhow::Result<Vec<PendingOpRow>> {
            unimplemented!()
        }
        async fn get_pending_ops_for_source(
            &self,
            _source: SourceId,
        ) -> anyhow::Result<Vec<PendingOpRow>> {
            unimplemented!()
        }
        async fn mark_pending_op_attempted(
            &self,
            _id: PendingOpId,
            _error: Option<&str>,
            _next_attempt_ms: i64,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn delete_pending_op(&self, _id: PendingOpId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn update_pending_op_payload(
            &self,
            _id: PendingOpId,
            _payload_json: &serde_json::Value,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn commit_create_result(
            &self,
            _op_id: PendingOpId,
            _file_state: &FileStateRow,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn commit_update_result(
            &self,
            _op_id: PendingOpId,
            _file_state: &FileStateRow,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn write_activity(&self, _row: NewActivity) -> anyhow::Result<ActivityId> {
            unimplemented!()
        }
        async fn query_activity(
            &self,
            _filter: ActivityFilter,
            _page: PageRequest,
        ) -> anyhow::Result<ActivityPage> {
            unimplemented!()
        }
        async fn prune_activity_older_than(
            &self,
            _before_ms: i64,
            _hard_cap: u64,
            _batch_size: Option<u32>,
        ) -> anyhow::Result<u64> {
            unimplemented!()
        }
        async fn delete_activity_by_source(&self, _source: SourceId) -> anyhow::Result<u64> {
            unimplemented!()
        }
        async fn schema_version(&self) -> anyhow::Result<i64> {
            unimplemented!()
        }
        async fn table_row_count(&self, _table: &str) -> anyhow::Result<i64> {
            unimplemented!()
        }
        async fn get_setting(&self, _key: &str) -> anyhow::Result<Option<serde_json::Value>> {
            unimplemented!()
        }
        async fn set_setting(&self, _key: &str, _value: &serde_json::Value) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn patch_setting_field(
            &self,
            _key: &str,
            _field: &str,
            _value: &serde_json::Value,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn search_files(
            &self,
            _source: Option<SourceId>,
            _query: &str,
            _limit: u32,
        ) -> anyhow::Result<Vec<FileSearchHit>> {
            unimplemented!()
        }
    }

    fn rel(s: &str) -> RelativePath {
        RelativePath::try_from(s.to_string()).unwrap()
    }

    /// Read back the on-disk (size, mtime_ns) the scanner would observe for
    /// a path, so tests can seed a matching `file_state` row.
    fn stat_of(path: &Path) -> (u64, i64) {
        let meta = fs::metadata(path).unwrap();
        (meta.len(), mtime_ns(&meta))
    }

    #[tokio::test]
    async fn first_scan_all_new() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.txt"), b"hello");
        write(&root.join("sub/b.txt"), b"world");

        let src = source_at(root);
        let state = FakeState::default();
        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();

        let mut got: Vec<String> = res
            .new_or_changed
            .iter()
            .map(|e| e.rel.as_str().to_string())
            .collect();
        got.sort();
        assert_eq!(got, vec!["a.txt".to_string(), "sub/b.txt".to_string()]);
        assert!(res.deleted.is_empty());
    }

    #[tokio::test]
    async fn unchanged_scan_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = root.join("a.txt");
        write(&p, b"hello");

        let src = source_at(root);
        let state = FakeState::default();
        let (size, mtime) = stat_of(&p);
        state.put(row(
            src.id,
            "a.txt",
            size,
            mtime,
            *blake3::hash(b"hello").as_bytes(),
        ));

        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert!(res.new_or_changed.is_empty(), "{:?}", res.new_or_changed);
        assert!(res.deleted.is_empty());
    }

    #[tokio::test]
    async fn single_mtime_change_yields_one() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = root.join("a.txt");
        write(&p, b"hello");

        let src = source_at(root);
        let state = FakeState::default();
        let (size, mtime) = stat_of(&p);
        // Stored mtime differs by one ns => changed under FastPath.
        state.put(row(
            src.id,
            "a.txt",
            size,
            mtime + 1,
            *blake3::hash(b"hello").as_bytes(),
        ));

        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert_eq!(res.new_or_changed.len(), 1);
        assert_eq!(res.new_or_changed[0].rel, rel("a.txt"));
        assert!(res.deleted.is_empty());
    }

    #[tokio::test]
    async fn deleted_file_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = root.join("present.txt");
        write(&p, b"x");

        let src = source_at(root);
        let state = FakeState::default();
        let (size, mtime) = stat_of(&p);
        state.put(row(
            src.id,
            "present.txt",
            size,
            mtime,
            *blake3::hash(b"x").as_bytes(),
        ));
        // A row whose file is gone from disk.
        state.put(row(src.id, "gone.txt", 1, 1, [0u8; 32]));

        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert!(res.new_or_changed.is_empty(), "{:?}", res.new_or_changed);
        assert_eq!(res.deleted, vec![rel("gone.txt")]);
    }

    #[tokio::test]
    async fn dot_ignore_excluded_row_is_orphan_not_deleted() {
        // P1-1/P1-2: a path STILL ON DISK but excluded by a `.ignore` file
        // (not just `.gitignore`) must be reported as an excluded_orphan, NOT
        // deleted - so an ignore-rule change never trashes a backed-up file.
        // Before the fix the matcher only loaded `.gitignore`, so the
        // WalkBuilder's native `.ignore` layer would drop the file from the
        // walk while the matcher still considered it included, and the orphan
        // split would misclassify it as `deleted`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let hidden = root.join("hidden.txt");
        write(&hidden, b"x");
        // The `.ignore` excludes both the target file AND itself, so the only
        // walk-yielded, matcher-included file is none - keeping new_or_changed
        // empty so the assertion below is a strong one.
        write(&root.join(".ignore"), b"hidden.txt\n.ignore\n");

        let src = source_at(root);
        let state = FakeState::default();
        let (size, mtime) = stat_of(&hidden);
        // A synced row for the now-excluded, still-present file.
        state.put(row(
            src.id,
            "hidden.txt",
            size,
            mtime,
            *blake3::hash(b"x").as_bytes(),
        ));

        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert_eq!(
            res.excluded_orphans,
            vec![rel("hidden.txt")],
            "still-present .ignore-excluded file must be an excluded_orphan: {res:?}"
        );
        assert!(
            res.deleted.is_empty(),
            "excluded-but-present file must NOT be deleted: {:?}",
            res.deleted
        );
        assert!(res.new_or_changed.is_empty(), "{:?}", res.new_or_changed);
    }

    #[tokio::test]
    async fn negation_above_an_excluded_dir_keeps_the_file_out_of_deleted() {
        // P1-1 (data-loss guard): a `!`-rule that can reach INTO an excluded
        // directory keeps that directory walked. Here the root `.gitignore`
        // excludes `vendor/` and then re-includes `vendor/keep.txt`; the
        // matcher classifies the file INCLUDED, so if the walk had pruned
        // `vendor/` the file would land un-`seen` and the orphan split would
        // false-classify its stored row as `deleted` - trashing a file that
        // still exists. The per-directory reachability check keeps `vendor/`
        // walked, so the row is neither deleted nor an excluded_orphan.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), b"vendor/\n!vendor/keep.txt\n");
        let keep = root.join("vendor/keep.txt");
        write(&keep, b"x");
        write(&root.join("vendor/drop.txt"), b"x");
        write(&root.join("top.txt"), b"x");

        let src = source_at(root);
        let state = FakeState::default();
        // Seed a synced row for the re-included file: if pruning false-deletes
        // it, this row lands in `deleted`.
        let (size, mtime) = stat_of(&keep);
        state.put(row(
            src.id,
            "vendor/keep.txt",
            size,
            mtime,
            *blake3::hash(b"x").as_bytes(),
        ));

        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert!(
            !res.deleted.contains(&rel("vendor/keep.txt")),
            "a re-included file under an excluded dir must NOT be deleted: {:?}",
            res.deleted
        );
        assert!(
            !res.excluded_orphans.contains(&rel("vendor/keep.txt")),
            "the re-included file is INCLUDED, so it is not an excluded_orphan either: {:?}",
            res.excluded_orphans
        );
    }

    #[tokio::test]
    async fn nested_negation_under_pruned_dir_is_orphan_not_deleted() {
        // The other half of the P1-1 lockstep invariant. Nothing at or above
        // `vendor/` carries a `!`-rule, so the matcher build prunes it and never
        // reads `vendor/.gitignore` - exactly as git would not - and the walk
        // prunes it too. The two agreeing is what makes this SAFE: the matcher
        // classifies `vendor/keep.txt` EXCLUDED, so its stored `file_state` row
        // is an excluded_orphan (the planner emits no trash op) and can never be
        // read as a deletion, even though the file was never walked.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), b"vendor/\n");
        write(&root.join("vendor/.gitignore"), b"!keep.txt\n");
        let keep = root.join("vendor/keep.txt");
        write(&keep, b"x");
        write(&root.join("top.txt"), b"x");

        let src = source_at(root);
        let state = FakeState::default();
        let (size, mtime) = stat_of(&keep);
        state.put(row(
            src.id,
            "vendor/keep.txt",
            size,
            mtime,
            *blake3::hash(b"x").as_bytes(),
        ));

        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert!(
            res.deleted.is_empty(),
            "a pruned directory must NEVER produce a deletion: {:?}",
            res.deleted
        );
        assert_eq!(
            res.excluded_orphans,
            vec![rel("vendor/keep.txt")],
            "the stored row is an excluded_orphan (no trash op): {res:?}"
        );
    }

    #[tokio::test]
    async fn git_info_exclude_drops_from_walk() {
        // P1-2: a `.git/info/exclude` rule must exclude a file from the walk
        // (not yielded in new_or_changed).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("secret.bin"), b"x");
        write(&root.join("keep.txt"), b"x");
        write(&root.join(".git/info/exclude"), b"secret.bin\n");

        let src = source_at(root);
        let state = FakeState::default();
        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();

        let got: Vec<String> = res
            .new_or_changed
            .iter()
            .map(|e| e.rel.as_str().to_string())
            .collect();
        assert!(got.contains(&"keep.txt".to_string()), "{got:?}");
        assert!(
            !got.contains(&"secret.bin".to_string()),
            ".git/info/exclude rule must drop the file from the walk: {got:?}"
        );
    }

    #[tokio::test]
    async fn missing_root_never_deletes() {
        // The mass-delete guard: a missing / unmounted source root must
        // yield an empty scan, NEVER report every known path as deleted
        // (DESIGN s5.2 step 3).
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("not-mounted");
        let src = source_at(&missing);

        let state = FakeState::default();
        // Seed several known rows whose files cannot possibly be seen.
        state.put(row(src.id, "a.txt", 1, 1, [0u8; 32]));
        state.put(row(src.id, "sub/b.txt", 1, 1, [0u8; 32]));

        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert!(res.new_or_changed.is_empty(), "{:?}", res.new_or_changed);
        assert!(
            res.deleted.is_empty(),
            "missing root must never cascade deletions: {:?}",
            res.deleted
        );
    }

    #[tokio::test]
    async fn deep_verify_catches_corrupted_bytes() {
        // Size + mtime unchanged but bytes differ from the stored hash:
        // FastPath misses it, DeepVerify catches it (bit-rot row).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = root.join("a.txt");
        write(&p, b"corrupted-bytes");

        let src = source_at(root);
        let state = FakeState::default();
        let (size, mtime) = stat_of(&p);
        // Stored hash is of DIFFERENT content, but size+mtime match disk.
        let stale_hash = *blake3::hash(b"original-content").as_bytes();
        // Make the stale row's size equal the on-disk size so (size, mtime)
        // matches and only the hash differs.
        state.put(row(src.id, "a.txt", size, mtime, stale_hash));

        // FastPath: stat matches => unchanged.
        let fast = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert!(fast.new_or_changed.is_empty(), "{:?}", fast.new_or_changed);

        // DeepVerify: hash mismatch => changed.
        let deep = scan(&src, &state, ScanMode::DeepVerify).await.unwrap();
        assert_eq!(deep.new_or_changed.len(), 1, "{:?}", deep.new_or_changed);
        assert_eq!(deep.new_or_changed[0].rel, rel("a.txt"));
    }

    #[tokio::test]
    async fn deep_verify_clean_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = root.join("a.txt");
        write(&p, b"hello");

        let src = source_at(root);
        let state = FakeState::default();
        let (size, mtime) = stat_of(&p);
        state.put(row(
            src.id,
            "a.txt",
            size,
            mtime,
            *blake3::hash(b"hello").as_bytes(),
        ));

        let res = scan(&src, &state, ScanMode::DeepVerify).await.unwrap();
        assert!(res.new_or_changed.is_empty(), "{:?}", res.new_or_changed);
    }

    /// Issue #35 item e: a BUNDLED member - a `file_state` row with
    /// `drive_file_id = NULL` (its bytes live inside a `.tar.gz` bundle), status
    /// Synced, and a matching stored hash - must NOT be re-emitted as changed by a
    /// deep-verify cycle. The verify path is purely local-content (re-hash vs the
    /// stored blake3) and never consults `drive_file_id`, so a bundled member that
    /// is byte-identical stays unchanged and is never spuriously re-uploaded.
    #[tokio::test]
    async fn deep_verify_does_not_touch_bundled_member() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = root.join("logs/app.log");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        write(&p, b"bundled member bytes");

        let src = source_at(root);
        let state = FakeState::default();
        let (size, mtime) = stat_of(&p);
        // The bundled-member shape: NULL drive_file_id, Synced, correct hash.
        let mut r = row(
            src.id,
            "logs/app.log",
            size,
            mtime,
            *blake3::hash(b"bundled member bytes").as_bytes(),
        );
        r.drive_file_id = None;
        r.drive_md5 = None;
        state.put(r);

        // A deep-verify cycle re-hashes the local bytes; they match, so the
        // member is unchanged - no re-upload op is produced for it.
        let res = scan(&src, &state, ScanMode::DeepVerify).await.unwrap();
        assert!(
            res.new_or_changed.is_empty(),
            "a byte-identical bundled member must not be re-emitted: {:?}",
            res.new_or_changed
        );
        assert!(
            res.deleted.is_empty(),
            "the member must not be treated as deleted"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_is_skipped() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let target = root.join("real.txt");
        write(&target, b"hello");
        symlink(&target, root.join("link.txt")).unwrap();

        let src = source_at(root);
        let state = FakeState::default();
        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();

        let got: Vec<String> = res
            .new_or_changed
            .iter()
            .map(|e| e.rel.as_str().to_string())
            .collect();
        assert!(got.contains(&"real.txt".to_string()), "{got:?}");
        assert!(
            !got.contains(&"link.txt".to_string()),
            "symlink must be skipped, not backed up: {got:?}"
        );
    }

    /// Two byte-distinct raw filenames that NFC-normalise to the same key
    /// (precomposed "cafe-acute" vs the decomposed "e + combining acute")
    /// must collapse to ONE `file_state` key: exactly one upload op and one
    /// recorded collision, never a duplicate op (DESIGN s5.2.3, SPEC s24
    /// `local.unicode_collision`).
    ///
    /// macOS/APFS itself normalises filenames, so the two paths would be the
    /// SAME file on disk and the collision could never arise from a walk;
    /// gate the on-disk variant off macOS. NTFS and ext4 keep them distinct,
    /// so the walk yields two entries and the scanner must dedup them. The
    /// `dedup_logic_drops_nfc_collider` test below exercises the same dedup
    /// branch directly with no filesystem dependency as a portable backstop.
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn nfc_collision_dedups_to_one_op() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Precomposed U+00E9 vs decomposed "e" + U+0301; both render "cafe"
        // with an acute accent.
        write(&root.join("caf\u{00e9}.txt"), b"precomposed");
        write(&root.join("cafe\u{0301}.txt"), b"decomposed");

        let src = source_at(root);
        let state = FakeState::default();
        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();

        // If the underlying FS silently merged the two names into one file
        // (some configurations do), there is nothing to dedup; the dedicated
        // unit test below still covers the logic, so only assert the
        // collision invariant when the walk actually saw two raw entries.
        if res.new_or_changed.len() + res.collisions.len() >= 2 {
            assert_eq!(
                res.new_or_changed.len(),
                1,
                "collision must yield exactly one upload op: {:?}",
                res.new_or_changed
            );
            assert_eq!(
                res.collisions.len(),
                1,
                "collision must be recorded exactly once: {:?}",
                res.collisions
            );
        }
        assert!(res.deleted.is_empty());
    }

    /// Portable backstop for the NFC dedup branch: drive the `HashSet::insert`
    /// contract the scanner relies on. The constructor NFC-normalises, so the
    /// two spellings produce equal keys; the second `insert` returns `false`,
    /// which is exactly the signal the walk loop uses to record a collision
    /// and drop the duplicate.
    #[test]
    fn dedup_logic_drops_nfc_collider() {
        let precomposed = RelativePath::try_from("caf\u{00e9}.txt".to_string()).unwrap();
        let decomposed = RelativePath::try_from("cafe\u{0301}.txt".to_string()).unwrap();
        assert_eq!(
            precomposed, decomposed,
            "NFC normalisation must collapse the two spellings to one key"
        );

        let mut seen: HashSet<RelativePath> = HashSet::new();
        assert!(seen.insert(precomposed.clone()), "first insert is new");
        assert!(
            !seen.insert(decomposed.clone()),
            "second insert of an NFC-equal key must report a collision"
        );
    }

    /// The cloud-only skip constant must match the Win32 SDK value
    /// `FILE_ATTRIBUTE_RECALL_ON_OPEN` (DESIGN s5.2.1). Synthesising the
    /// actual attribute on a temp file is not portably possible, so pin the
    /// bit value to guard against an accidental edit.
    #[test]
    fn recall_on_open_bit_is_correct() {
        assert_eq!(super::FILE_ATTRIBUTE_RECALL_ON_OPEN, 0x0040_0000);
    }

    /// Issue #4: [`should_skip_placeholder`] honours the per-source policy. The
    /// real `FILE_ATTRIBUTE_RECALL_ON_OPEN` bit cannot be set on a temp file
    /// cross-platform, so this drives the pure decision fn directly (the same
    /// bit the `#[cfg(windows)]` scanner path passes from `meta.file_attributes()`).
    #[test]
    fn placeholder_skip_policy_honoured() {
        use crate::state::PlaceholderPolicy;
        let recall = super::FILE_ATTRIBUTE_RECALL_ON_OPEN;
        let plain = 0x0000_0020; // FILE_ATTRIBUTE_ARCHIVE - not a placeholder.

        // Default Skip: a recall-on-open placeholder is skipped; an ordinary file
        // (and a placeholder combined with other attribute bits) behaves as its
        // recall bit dictates.
        assert!(super::should_skip_placeholder(
            recall,
            PlaceholderPolicy::Skip
        ));
        assert!(super::should_skip_placeholder(
            recall | plain,
            PlaceholderPolicy::Skip
        ));
        assert!(!super::should_skip_placeholder(
            plain,
            PlaceholderPolicy::Skip
        ));

        // ForceDownload: a placeholder is NEVER skipped, so it flows through the
        // normal read path and hydrates; an ordinary file is likewise not skipped.
        assert!(!super::should_skip_placeholder(
            recall,
            PlaceholderPolicy::ForceDownload
        ));
        assert!(!super::should_skip_placeholder(
            recall | plain,
            PlaceholderPolicy::ForceDownload
        ));
        assert!(!super::should_skip_placeholder(
            plain,
            PlaceholderPolicy::ForceDownload
        ));
    }

    // --- FS mtime-granularity probe + coarse-FS fallback (DESIGN s5.2) -------

    /// The pure classifier maps a probe observation to a granularity window:
    /// an observed change-gap is that gap; "never observed within budget" is at
    /// least the probe budget (coarse).
    #[test]
    fn classify_granularity_ns_maps_observation() {
        assert_eq!(super::classify_granularity_ns(Some(500)), 500);
        assert_eq!(
            super::classify_granularity_ns(Some(2_000_000_000)),
            2_000_000_000
        );
        assert_eq!(
            super::classify_granularity_ns(None),
            super::PROBE_BUDGET_NS,
            "no observed change within budget => at least the budget window"
        );
    }

    /// The coarse boundary is strictly greater than one second (DESIGN s5.2).
    #[test]
    fn granularity_is_coarse_boundary() {
        assert!(!super::granularity_is_coarse(0), "fine FS is not coarse");
        assert!(
            !super::granularity_is_coarse(super::ONE_SECOND_NS),
            "exactly 1s is NOT coarse (threshold is > 1s)"
        );
        assert!(
            super::granularity_is_coarse(super::ONE_SECOND_NS + 1),
            "just over 1s is coarse"
        );
        assert!(super::granularity_is_coarse(2_000_000_000));
    }

    /// The coarse-FS re-hash decision (DESIGN s5.2 step 2) as a pure table.
    #[test]
    fn coarse_fs_needs_rehash_decision_table() {
        let coarse = 2_000_000_000u64; // 2s window
        let fine = super::ONE_SECOND_NS; // exactly 1s => not coarse
        let scan_end = 1_000_000_000_000i64; // arbitrary ns boundary

        // A fine FS never re-hashes, regardless of the timestamps.
        assert!(!super::coarse_fs_needs_rehash(
            fine,
            scan_end,
            Some(scan_end + 10),
            Some(scan_end + 10),
            Some(scan_end)
        ));

        // Coarse FS, mtime far from the boundary and no fresher ctime/birth:
        // trusted, no re-hash.
        assert!(!super::coarse_fs_needs_rehash(
            coarse,
            scan_end - 10_000_000_000, // 10s before the boundary, outside the 2s window
            Some(scan_end - 10_000_000_000),
            None,
            Some(scan_end)
        ));

        // Coarse FS, mtime within one window of the boundary: re-hash.
        assert!(super::coarse_fs_needs_rehash(
            coarse,
            scan_end - 1_000_000_000, // 1s before, inside the 2s window
            None,
            None,
            Some(scan_end)
        ));

        // Coarse FS, ctime post-dates the last scan: re-hash even if mtime is old.
        assert!(super::coarse_fs_needs_rehash(
            coarse,
            scan_end - 10_000_000_000,
            Some(scan_end + 1),
            None,
            Some(scan_end)
        ));

        // Coarse FS, birth post-dates the last scan: re-hash.
        assert!(super::coarse_fs_needs_rehash(
            coarse,
            scan_end - 10_000_000_000,
            None,
            Some(scan_end + 1),
            Some(scan_end)
        ));

        // Coarse FS, no prior scan boundary: conservatively re-hash.
        assert!(super::coarse_fs_needs_rehash(
            coarse, scan_end, None, None, None
        ));
    }

    /// The write-stat probe on the test filesystem (a fast local FS) classifies
    /// non-coarse and leaves no temp file behind before the walk. NOT exactly 0:
    /// NTFS stamps mtimes from the interrupt-time clock, so a burst of rewrites
    /// landing inside one timer tick legitimately measures a millisecond-scale
    /// window (~2.5ms seen on windows-latest) instead of 0.
    #[test]
    fn probe_classifies_local_fs_fine() {
        let dir = tempfile::tempdir().unwrap();
        let g = super::probe_mtime_granularity(dir.path()).expect("probe");
        assert!(
            !super::granularity_is_coarse(g),
            "a fast local FS must probe as non-coarse, got {g}ns"
        );
        // No probe temp file leaked.
        let leaked: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".driven-mtime-probe-")
            })
            .collect();
        assert!(leaked.is_empty(), "probe temp file must be cleaned up");
    }

    /// Integration: on a source whose persisted granularity is COARSE, a
    /// stat-matched file (size+mtime equal the stored row) whose mtime sits
    /// within one granularity window of the last-scan-end is re-hashed on the
    /// FAST path, and a stored-hash mismatch marks it changed - catching a
    /// within-quantum in-place edit that mtime alone would miss (DESIGN s5.2).
    #[tokio::test]
    async fn coarse_fs_rehashes_stat_matched_within_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = root.join("a.txt");
        write(&p, b"edited-within-quantum");

        let mut src = source_at(root);
        // Persisted coarse (2s) granularity => no probe, fallback engaged.
        src.mtime_granularity_ns = Some(2_000_000_000);
        let (size, mtime) = stat_of(&p);
        // Last-scan-end at (about) the file's own mtime, so |mtime - scan_end|
        // is well inside the 2s window.
        src.last_full_scan_at = Some(mtime / 1_000_000);

        let state = FakeState::default();
        // A stored row with the SAME size+mtime but a STALE hash (of different
        // bytes) - FastPath alone would call this unchanged.
        state.put(row(
            src.id,
            "a.txt",
            size,
            mtime,
            *blake3::hash(b"original-content").as_bytes(),
        ));

        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert_eq!(
            res.new_or_changed.len(),
            1,
            "coarse-FS fallback must re-hash + flag the within-window edit: {:?}",
            res.new_or_changed
        );
        assert_eq!(res.new_or_changed[0].rel, rel("a.txt"));
    }

    /// The inverse of the above: on a FINE-granularity source the same
    /// stat-matched, stale-hash file is NOT re-hashed on the fast path (exact
    /// mtime is trusted), so it stays unchanged - proving the fallback is gated
    /// on the coarse classification, not always on.
    #[tokio::test]
    async fn fine_fs_trusts_stat_match_no_rehash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = root.join("a.txt");
        write(&p, b"some-bytes");

        let mut src = source_at(root);
        src.mtime_granularity_ns = Some(0); // probed fine
        let (size, mtime) = stat_of(&p);
        src.last_full_scan_at = Some(mtime / 1_000_000);

        let state = FakeState::default();
        state.put(row(
            src.id,
            "a.txt",
            size,
            mtime,
            *blake3::hash(b"a-different-content").as_bytes(),
        ));

        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert!(
            res.new_or_changed.is_empty(),
            "a fine FS must trust the stat match (no fallback re-hash): {:?}",
            res.new_or_changed
        );
    }

    /// The scan reports the probed granularity via `ScanResult` when the source
    /// has none persisted AND a prior scan exists (the orchestrator persists it),
    /// and reports `None` when the source already has a stored value (no
    /// re-probe). The first-scan deferral is covered by
    /// [`first_scan_defers_probe_and_coarse_fallback`].
    #[tokio::test]
    async fn scan_reports_probe_only_when_unpersisted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.txt"), b"x");
        let state = FakeState::default();

        // Unpersisted (None) but a prior scan exists => the scan probes and
        // hands back Some(_).
        let mut src = source_at(root);
        src.mtime_granularity_ns = None;
        src.last_full_scan_at = Some(1);
        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        let g = res
            .probed_granularity_ns
            .expect("an unprobed source with a prior scan is probed and surfaced");
        assert!(
            !super::granularity_is_coarse(u64::try_from(g).unwrap()),
            "the test FS must probe as non-coarse, got {g}ns"
        );

        // Already persisted => no re-probe, so nothing to hand back.
        src.mtime_granularity_ns = Some(0);
        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert_eq!(res.probed_granularity_ns, None);
    }

    /// Regression (chaos `frequent-edits`, PR #141): the FIRST scan of a source
    /// (no completed scan yet) must NOT run the write-stat probe and must NOT
    /// engage the coarse re-hash fallback - it is pure fast-path. Deferring the
    /// probe keeps its latency + temp-file churn off the initial backup cycle,
    /// where each path's first upload must commit cleanly; the probe runs from
    /// the next scan. Before the deferral the probe ran inline on scan 1, and on
    /// the windows-latest hermetic runner that first-scan cost tipped a
    /// pre-existing create-during-upload race into a duplicate upload.
    #[tokio::test]
    async fn first_scan_defers_probe_and_coarse_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let p = root.join("a.txt");
        write(&p, b"some-bytes");

        // First scan: unprobed AND no prior completed scan.
        let mut src = source_at(root);
        src.mtime_granularity_ns = None;
        assert!(src.last_full_scan_at.is_none(), "precondition: first scan");

        // A stored row with the SAME (size, mtime) but a STALE hash. If the first
        // scan probed-and-classified-coarse (or otherwise engaged the fallback)
        // it would re-hash and flag this changed; the fast path must instead
        // trust the stat match and emit nothing.
        let (size, mtime) = stat_of(&p);
        let state = FakeState::default();
        state.put(row(
            src.id,
            "a.txt",
            size,
            mtime,
            *blake3::hash(b"a-different-content").as_bytes(),
        ));

        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert_eq!(
            res.probed_granularity_ns, None,
            "the first scan must not probe (nothing to persist)"
        );
        assert!(
            res.new_or_changed.is_empty(),
            "the first scan must be pure fast-path (no coarse fallback re-hash): {:?}",
            res.new_or_changed
        );
        // The probe never ran, so no probe temp file was ever written.
        let leaked: Vec<_> = fs::read_dir(root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".driven-mtime-probe-")
            })
            .collect();
        assert!(
            leaked.is_empty(),
            "no probe temp file on a deferred first scan"
        );

        // Once a scan has completed (last_full_scan_at set), the NEXT scan probes.
        src.last_full_scan_at = Some(mtime / 1_000_000);
        let res = scan(&src, &state, ScanMode::FastPath).await.unwrap();
        assert!(
            res.probed_granularity_ns.is_some(),
            "the probe runs on the scan after the first"
        );
    }

    /// The live scan-progress sink (the "Run now looks dead" fix): the walk must
    /// tick it AS it goes, and the last tick must carry the true total.
    #[tokio::test]
    async fn scan_ticks_the_progress_sink_and_reports_the_final_total() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // More than one stride so the in-walk throttle fires at least once
        // before the final tick.
        let files = (SCAN_PROGRESS_FILE_STRIDE + 7) as usize;
        for i in 0..files {
            write(&root.join(format!("sub{}/f{}.txt", i % 4, i)), b"x");
        }

        let src = source_at(root);
        let state = FakeState::default();
        let ticks: Mutex<Vec<u64>> = Mutex::new(Vec::new());
        let sink = |visited: u64| -> futures::future::BoxFuture<'_, ()> {
            ticks.lock().unwrap().push(visited);
            Box::pin(async {})
        };

        let res = scan_with_progress(&src, &state, ScanMode::FastPath, None, Some(&sink))
            .await
            .unwrap();
        assert_eq!(res.new_or_changed.len(), files, "every file is new");

        let ticks = ticks.lock().unwrap().clone();
        assert!(
            ticks.len() >= 2,
            "expected an in-walk tick plus the final one, got {ticks:?}"
        );
        assert_eq!(
            *ticks.last().unwrap(),
            files as u64,
            "the final tick carries the true visited total"
        );
        assert!(
            ticks.windows(2).all(|w| w[0] <= w[1]),
            "the visited count is monotonic: {ticks:?}"
        );
        assert!(
            ticks[..ticks.len() - 1].iter().any(|&t| t < files as u64),
            "at least one tick lands BEFORE the walk finishes: {ticks:?}"
        );
    }

    /// A source with no files still settles the readout: exactly one tick, zero.
    #[tokio::test]
    async fn scan_ticks_zero_on_an_empty_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = source_at(dir.path());
        let state = FakeState::default();
        let ticks: Mutex<Vec<u64>> = Mutex::new(Vec::new());
        let sink = |visited: u64| -> futures::future::BoxFuture<'_, ()> {
            ticks.lock().unwrap().push(visited);
            Box::pin(async {})
        };

        scan_with_progress(&src, &state, ScanMode::FastPath, None, Some(&sink))
            .await
            .unwrap();
        assert_eq!(*ticks.lock().unwrap(), vec![0]);
    }

    /// Excluded files still move the counter - otherwise descending a big
    /// `node_modules` looks identical to a hung scan.
    #[tokio::test]
    async fn scan_progress_counts_excluded_files_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("keep.txt"), b"x");
        write(&root.join("skip/a.txt"), b"x");
        write(&root.join("skip/b.txt"), b"x");

        let mut src = source_at(root);
        src.exclude_patterns = vec!["skip/**".to_string()];
        let state = FakeState::default();
        let ticks: Mutex<Vec<u64>> = Mutex::new(Vec::new());
        let sink = |visited: u64| -> futures::future::BoxFuture<'_, ()> {
            ticks.lock().unwrap().push(visited);
            Box::pin(async {})
        };

        let res = scan_with_progress(&src, &state, ScanMode::FastPath, None, Some(&sink))
            .await
            .unwrap();
        assert_eq!(
            res.new_or_changed.len(),
            1,
            "only the included file uploads"
        );
        assert_eq!(
            *ticks.lock().unwrap().last().unwrap(),
            3,
            "all three visited files count toward scan progress"
        );
    }
}
