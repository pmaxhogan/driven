//! Filesystem-watcher surface: per-source change detection that triggers
//! an early scan-tick (DESIGN s5.9).
//!
//! The watcher is a *latency win* layered on top of the authoritative
//! scheduled scan, never a replacement: a missed watcher event must only
//! make backup *slower*, never silently *lose* a file (DESIGN s5.9.5). So
//! a watcher event is a **scan-tick request**, not a per-file upload
//! trigger (DESIGN s5.9.1) - the scanner remains the diff-and-plan engine.
//!
//! This module carries BOTH the I/O-free contract ([`SourceWatcher`] and
//! its value types, consumed by the orchestrator) and the concrete M3
//! implementation [`NotifyWatcher`] backed by `notify` v8 (inotify /
//! FSEvents / ReadDirectoryChangesW). The same shape happened for the
//! executor: the M2 trait surface stays verbatim and M3 adds the real impl
//! beside it.
//!
//! ## Implementation shape (DESIGN s5.9.1, s5.9.2, s5.9.3, s5.9.4)
//!
//! - One [`notify::RecommendedWatcher`] per watched source, watching the
//!   source's `local_path` recursively.
//! - `notify`'s event callback runs on `notify`'s own backend thread and
//!   must never block, so it does the cheap path-prefix exclude filter
//!   (DESIGN s5.9.3) inline and forwards a unit "saw an edit" tick into a
//!   per-source `std`-channel; a per-source async debounce task owns the
//!   500 ms quiet window + 1-request/minute/source cap (DESIGN s5.9.2) and
//!   emits the debounced [`ScanTickRequest`] onto the orchestrator
//!   `tokio::sync::mpsc` (DESIGN s5.9.1).
//! - A backend error delivered in the event stream is mapped to an in-band
//!   [`ScanTickReason::Degraded`] request (inotify watch-limit, FSEvents
//!   coalescing, Windows handle invalidation) so the orchestrator falls
//!   back to the scheduled scan and re-watches on the next tick (DESIGN
//!   s5.9.4); we never panic on a watcher fault.
//!
//! `notify` v8 lives in `[workspace.dependencies]`; this crate needs
//! `notify.workspace = true` added to its own `[dependencies]` for the
//! concrete impl below to compile (see the module-level integration note in
//! the M3 report - left unedited here per the touch-only rule).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::exclude::{build_source_matcher, SourceMatcher};
use crate::state::SourceRow;
use crate::types::SourceId;

use serde::{Deserialize, Serialize};

static TARGET: &str = "driven::core::watcher";

/// The debounce quiet window: a scan-tick fires this long after the last
/// observed edit for a source (DESIGN s5.9.2).
const DEBOUNCE_QUIET: Duration = Duration::from_millis(500);

/// Hard cap: at most one watcher-driven scan-tick request per source per
/// this window (DESIGN s5.9.2). The scheduled scan runs independently of
/// this cap.
const RATE_CAP_WINDOW: Duration = Duration::from_secs(60);

/// Why the watcher emitted a [`ScanTickRequest`] (DESIGN s5.9).
///
/// The orchestrator treats `Edit` as "scan earlier than the next
/// scheduled tick" and `WatcherDied` / `Degraded` as "fall back to
/// scheduled-only for this source and try to re-establish the watch on
/// the next tick" (DESIGN s5.9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanTickReason {
    /// One or more debounced filesystem edits were observed under the
    /// source root (DESIGN s5.9.2: collapsed after a 500 ms quiet window,
    /// capped at one request/minute/source from the watcher).
    Edit,
    /// The watcher's backend reported a recoverable degradation (inotify
    /// watch-limit exhausted, FSEvents coalescing, a Windows
    /// directory-handle invalidation) and the source is now relying on the
    /// scheduled scan until the watch is re-established (DESIGN s5.9.4).
    Degraded,
    /// The watcher thread / channel closed unexpectedly; the orchestrator
    /// logs `WatcherDied`, falls back to scheduled-only, and attempts a
    /// restart on the next tick (DESIGN s5.9.4).
    WatcherDied,
}

/// A debounced request from the watcher asking the orchestrator to scan a
/// source ahead of its scheduled tick (DESIGN s5.9.1, s5.9.2).
///
/// Carries no per-file path list by design: the watcher says only "scan
/// this source"; the scanner re-derives the actual diff (DESIGN s5.9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScanTickRequest {
    /// Source the early scan is requested for.
    pub source_id: SourceId,
    /// Why the request was emitted.
    pub reason: ScanTickReason,
}

/// The per-source filesystem-watcher contract (DESIGN s5.9).
///
/// One implementation watches one source's `local_path` recursively and
/// emits debounced [`ScanTickRequest`]s. The orchestrator consumes them
/// over a `tokio::sync::mpsc` channel (DESIGN s5.9.1); the trait exposes
/// that subscription plus lifecycle control so a source can be
/// re-watched after a `Degraded` / `WatcherDied` event.
pub trait SourceWatcher: Send + Sync {
    /// Begins watching the source root recursively. Idempotent: calling it
    /// on an already-watching source re-establishes the watch (the restart
    /// path after a degradation, DESIGN s5.9.4). Returns an error if the
    /// backend cannot establish the watch at all (e.g. the path no longer
    /// exists); a *recoverable* degradation is reported in-band as a
    /// [`ScanTickReason::Degraded`] request instead.
    fn watch(&self, source_id: SourceId) -> anyhow::Result<()>;

    /// Stops watching the source and releases the backend handle.
    fn unwatch(&self, source_id: SourceId);

    /// Returns the receive end of the debounced scan-tick stream
    /// (DESIGN s5.9.1: a single mpsc consumer - the orchestrator selects on
    /// it alongside its scheduled-tick timer and the power / network event
    /// channels).
    ///
    /// **Take-once semantics.** `mpsc::Receiver` is single-consumer and not
    /// `Clone`, so this hands out the one receiver and an implementation
    /// may return it at most once (a second call returns `None`). The
    /// implement phase may instead choose to hand the receiver to the
    /// orchestrator at construction and keep only the `Sender` behind the
    /// trait; that reshaping is in-bounds (see the M3 phase-1 finding).
    fn subscribe(&self) -> Option<mpsc::Receiver<ScanTickRequest>>;
}

/// The concrete `notify`-v8-backed [`SourceWatcher`] (DESIGN s5.9.1).
///
/// Holds one [`RecommendedWatcher`] per watched source plus the debounce
/// task that turns the bursty per-source raw-event stream into a single
/// debounced [`ScanTickRequest`]. Constructed with the set of [`SourceRow`]s
/// it may be asked to watch so [`SourceWatcher::watch`] can resolve a
/// `SourceId` to its `local_path` + exclude matcher (the trait passes only
/// the id; this reshaping is blessed by the trait doc's phase-1 finding).
///
/// The orchestrator-facing receiver is created at construction and handed
/// out once via [`SourceWatcher::subscribe`]; the watcher keeps the
/// [`mpsc::Sender`] and clones it per source for the debounce tasks.
pub struct NotifyWatcher {
    /// Source config by id, used to resolve `local_path` + build the
    /// exclude matcher at watch time.
    sources: HashMap<SourceId, SourceRow>,
    /// Live per-source state (backend handle + debounce-task shutdown). A
    /// `Mutex` because the trait methods take `&self` (the orchestrator may
    /// re-watch / unwatch concurrently) yet must mutate the live-watch map.
    live: Mutex<HashMap<SourceId, LiveWatch>>,
    /// Cloned per source for the debounce task -> orchestrator hop.
    tx: mpsc::Sender<ScanTickRequest>,
    /// Handed out at most once by [`SourceWatcher::subscribe`].
    rx: Mutex<Option<mpsc::Receiver<ScanTickRequest>>>,
}

/// Per-source live-watch state: owns the backend handle (dropping it stops
/// the OS watch) and the debounce-task handle (aborting it stops the timer
/// loop). Dropping `LiveWatch` therefore fully tears a source down.
struct LiveWatch {
    /// The `notify` backend handle. Held to keep the watch alive; dropped on
    /// `unwatch` / re-watch.
    _watcher: RecommendedWatcher,
    /// Shutdown signal for the debounce task: dropping the sender closes the
    /// raw-event channel, which ends the debounce task's loop.
    debounce_task: tokio::task::JoinHandle<()>,
}

impl Drop for LiveWatch {
    fn drop(&mut self) {
        // A deliberate unwatch / re-watch is a SILENT, clean teardown: abort
        // the debounce task so it does not emit anything for an intentional
        // stop. (`WatcherDied` is reserved for an *unexpected* closure the
        // orchestrator should try to restart - see [`ScanTickReason`] - which
        // a user-driven unwatch is not.) Aborting also guarantees a re-watch
        // cannot leave two debounce loops racing for one source.
        self.debounce_task.abort();
    }
}

/// The root(s) an event path is stripped against before the exclude check.
///
/// `plain` is the path passed to `notify.watch()` (matches event paths on
/// Linux / Windows); `canonical` is its realpath when available (matches the
/// `/private`-prefixed paths macOS FSEvents reports). [`path_is_excluded`]
/// tries `plain` first (the common, zero-cost path) then `canonical`.
struct StripRoots {
    plain: PathBuf,
    canonical: Option<PathBuf>,
}

impl NotifyWatcher {
    /// Channel depth for the orchestrator-facing debounced stream. The
    /// debounce + 1/min cap already collapse bursts, so a small buffer is
    /// plenty; a full channel means the orchestrator is far behind and a
    /// dropped extra scan-tick is harmless (the scheduled scan still runs).
    const CHANNEL_DEPTH: usize = 64;

    /// Builds a [`NotifyWatcher`] over the sources it may watch.
    ///
    /// Nothing is watched yet; the orchestrator calls
    /// [`SourceWatcher::watch`] per source. `subscribe` must be called once
    /// to obtain the debounced [`ScanTickRequest`] stream.
    pub fn new(sources: impl IntoIterator<Item = SourceRow>) -> Self {
        let (tx, rx) = mpsc::channel(Self::CHANNEL_DEPTH);
        let sources = sources.into_iter().map(|s| (s.id, s)).collect();
        Self {
            sources,
            live: Mutex::new(HashMap::new()),
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// Spawns (or re-spawns) the watch for one resolved source: builds the
    /// exclude matcher, creates the `notify` backend with a non-blocking
    /// callback, and starts the debounce task. Returns the [`LiveWatch`] on
    /// success or an `Err` if the backend could not establish the watch at
    /// all (DESIGN s5.9.4 hard-failure path; recoverable faults arrive
    /// later in-band on the event stream).
    fn spawn_watch(&self, source: &SourceRow) -> anyhow::Result<LiveWatch> {
        let source_id = source.id;
        let root = PathBuf::from(&source.local_path);

        // Exclude matcher for the path-prefix filter (DESIGN s5.9.3). Built
        // once per watch and moved into the backend callback. A build failure
        // is fatal for THIS watch attempt (we cannot filter safely), surfaced
        // as the `watch()` Err so the orchestrator falls back to scheduled.
        let matcher = build_source_matcher(source)?;

        // Strip-roots for the exclude filter. notify v8 derives event paths by
        // joining onto the watched root on Linux/Windows, so the plain `root`
        // strips correctly there; macOS FSEvents reports realpaths (e.g.
        // `/var` -> `/private/var`), so we also keep a canonicalized form and
        // try it as a fallback. Computed once per watch; `None` when the path
        // cannot be canonicalized (e.g. it just vanished) - the plain root
        // still covers the common platforms.
        let canonical_root = std::fs::canonicalize(&root).ok();
        let strip_roots = StripRoots {
            plain: root.clone(),
            canonical: canonical_root,
        };

        // Raw per-source edit ticks: the notify callback (on notify's thread)
        // sends a `RawTick` per qualifying event; the debounce task drains
        // them. A std mpsc keeps the callback free of any async runtime
        // dependency.
        let (raw_tx, raw_rx) = std::sync::mpsc::channel::<RawTick>();

        let out_tx = self.tx.clone();
        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                handle_backend_event(res, &strip_roots, &matcher, &raw_tx);
            },
            notify::Config::default(),
        )
        .map_err(|e| anyhow::anyhow!("creating notify watcher for {source_id}: {e}"))?;

        // The one call that can legitimately fail when the path is gone /
        // unwatchable -> hard `watch()` error (DESIGN s5.9.4).
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|e| anyhow::anyhow!("watching {} for {source_id}: {e}", root.display()))?;

        let debounce_task = tokio::spawn(debounce_loop(source_id, raw_rx, out_tx));

        tracing::debug!(
            target: TARGET,
            %source_id,
            path = %root.display(),
            "established filesystem watch"
        );
        Ok(LiveWatch {
            _watcher: watcher,
            debounce_task,
        })
    }
}

impl SourceWatcher for NotifyWatcher {
    fn watch(&self, source_id: SourceId) -> anyhow::Result<()> {
        let source = self
            .sources
            .get(&source_id)
            .ok_or_else(|| anyhow::anyhow!("unknown source {source_id}: cannot watch"))?
            .clone();

        // Build the new watch BEFORE swapping out any existing one so a failed
        // re-watch leaves the old watch untouched rather than tearing it down.
        let live = self.spawn_watch(&source)?;

        // Idempotent re-watch (DESIGN s5.9.4): replacing the entry drops the
        // previous LiveWatch, which stops the old OS watch + debounce task.
        match self.live.lock() {
            Ok(mut map) => {
                map.insert(source_id, live);
                Ok(())
            }
            // A poisoned lock means a prior holder panicked; do not propagate a
            // panic. The new `live` drops here, tearing its own watch down.
            Err(_) => Err(anyhow::anyhow!(
                "watcher live-map lock poisoned; refusing to watch {source_id}"
            )),
        }
    }

    fn unwatch(&self, source_id: SourceId) {
        // Dropping the removed LiveWatch stops the OS watch and the debounce
        // task. A poisoned lock is logged, not panicked (house rule).
        match self.live.lock() {
            Ok(mut map) => {
                if map.remove(&source_id).is_some() {
                    tracing::debug!(target: TARGET, %source_id, "stopped filesystem watch");
                }
            }
            Err(_) => {
                tracing::warn!(
                    target: TARGET,
                    %source_id,
                    "watcher live-map lock poisoned; unwatch skipped"
                );
            }
        }
    }

    fn subscribe(&self) -> Option<mpsc::Receiver<ScanTickRequest>> {
        // Take-once: a poisoned lock yields None rather than panicking.
        self.rx.lock().ok().and_then(|mut g| g.take())
    }
}

/// What the `notify` callback forwards to the debounce task: either a real
/// edit under the (non-excluded) source root, or a recoverable backend
/// degradation that should surface as a [`ScanTickReason::Degraded`]
/// request (DESIGN s5.9.4).
enum RawTick {
    /// A qualifying filesystem edit was observed.
    Edit,
    /// The backend reported a recoverable error in the event stream.
    Degraded,
}

/// Handles one event delivered by the `notify` backend callback (runs on
/// `notify`'s thread - must not block).
///
/// On `Ok(event)` it applies the path-prefix exclude filter (DESIGN s5.9.3):
/// if EVERY path on the event lies under an excluded prefix the event is
/// dropped (git-noise suppression); otherwise a [`RawTick::Edit`] is
/// forwarded. On `Err` it forwards a [`RawTick::Degraded`] so the source
/// degrades to scheduled-only without panicking (DESIGN s5.9.4). A closed
/// raw channel (debounce task gone) is ignored - teardown is in progress.
fn handle_backend_event(
    res: notify::Result<Event>,
    roots: &StripRoots,
    matcher: &SourceMatcher,
    raw_tx: &std::sync::mpsc::Sender<RawTick>,
) {
    match res {
        Ok(event) => {
            if event_is_all_excluded(&event, roots, matcher) {
                // Pure git-noise / excluded-prefix burst: drop it so the
                // debounce buffer never fills on a dev folder (DESIGN s5.9.3).
                return;
            }
            // Ignore send errors: a closed channel means the debounce task
            // (and this watch) is being torn down.
            let _ = raw_tx.send(RawTick::Edit);
        }
        Err(err) => {
            tracing::warn!(
                target: TARGET,
                %err,
                "watcher backend reported a recoverable error; degrading to scheduled scan"
            );
            let _ = raw_tx.send(RawTick::Degraded);
        }
    }
}

/// True iff the event carries at least one path AND every path is excluded
/// by the source matcher (DESIGN s5.9.3 path-prefix filtering).
///
/// An event with NO paths (some backends emit pathless meta-events) is
/// treated as NOT all-excluded so we conservatively keep it - dropping a
/// meta-event could lose a real change, and the scanner is the authority
/// anyway (a spurious scan-tick is merely a wasted cheap scan).
fn event_is_all_excluded(event: &Event, roots: &StripRoots, matcher: &SourceMatcher) -> bool {
    if event.paths.is_empty() {
        return false;
    }
    event
        .paths
        .iter()
        .all(|p| path_is_excluded(p, roots, matcher))
}

/// True iff `path` strips under one of `roots` and the matcher excludes it
/// (or an ancestor of it). A path under no known root, or one that strips to
/// the empty (root itself) form, is treated as NOT excluded (keep it -
/// conservative; a spurious scan-tick is merely a wasted cheap scan, whereas
/// wrongly dropping a real edit would cost latency).
fn path_is_excluded(path: &Path, roots: &StripRoots, matcher: &SourceMatcher) -> bool {
    // Try the plain root first (matches event paths on Linux / Windows), then
    // the canonical root (matches macOS realpaths). First successful strip
    // decides; only fall through to the next root when the strip fails.
    let rel = path.strip_prefix(&roots.plain).ok().or_else(|| {
        roots
            .canonical
            .as_deref()
            .and_then(|c| path.strip_prefix(c).ok())
    });
    match rel {
        Some(rel) if !rel.as_os_str().is_empty() => {
            // The event carries no reliable file-vs-dir flag (a delete event's
            // path may already be gone, and inotify reports modifications to a
            // watched subdirectory as a bare event ON that directory). Treat
            // the path as excluded if it is excluded under EITHER
            // interpretation: querying as a file catches an excluded ancestor
            // (e.g. `.git/objects/x`), and querying as a directory catches a
            // bare event on a directory-scoped exclude itself (e.g. the `noise`
            // entry against a `noise/` rule, which `is_included(_, false)`
            // alone would leak because the trailing-slash rule is dir-only).
            // `is_included` walks ancestors in both cases, so a child under an
            // excluded dir is still filtered. This is the git-noise / excluded-
            // prefix buffer-pressure relief s5.9.3 targets; a kept file or dir
            // matches no exclude under either query and is never dropped.
            !matcher.is_included(rel, false) || !matcher.is_included(rel, true)
        }
        _ => false,
    }
}

/// The per-source debounce loop (DESIGN s5.9.2).
///
/// Drains the raw per-source tick channel, collapses bursts with a 500 ms
/// quiet window, enforces the 1-request/minute/source cap, and emits the
/// debounced [`ScanTickRequest`] onto the orchestrator channel. A
/// [`RawTick::Degraded`] is forwarded immediately (no debounce) as a
/// `Degraded` request so the orchestrator can fall back promptly (DESIGN
/// s5.9.4).
///
/// When the raw channel closes the loop flushes any pending edit (cap
/// permitting) and then exits **silently**: channel closure here only ever
/// results from the backend handle being dropped on a deliberate `unwatch`
/// / re-watch (and the task is `abort()`ed by [`LiveWatch::drop`] in that
/// path anyway). It does NOT emit [`ScanTickReason::WatcherDied`] - that
/// reason is reserved for an *unexpected* death the orchestrator should try
/// to restart, whereas a real backend fault surfaces earlier as an `Err`
/// event mapped to [`ScanTickReason::Degraded`].
///
/// Runs the blocking `std::sync::mpsc::Receiver::recv` on a dedicated
/// blocking thread (so it never starves the tokio reactor) and uses
/// `tokio::time` for the quiet window + cap so the timing is driven by the
/// async runtime the tests run on.
async fn debounce_loop(
    source_id: SourceId,
    raw_rx: std::sync::mpsc::Receiver<RawTick>,
    out_tx: mpsc::Sender<ScanTickRequest>,
) {
    // Bridge the blocking std channel into an async one on a blocking thread
    // so `recv().await` here never blocks the reactor.
    let (bridge_tx, mut bridge_rx) = mpsc::channel::<RawTick>(NotifyWatcher::CHANNEL_DEPTH);
    let bridge = tokio::task::spawn_blocking(move || {
        while let Ok(tick) = raw_rx.recv() {
            // The async side is bounded; blocking_send applies backpressure
            // rather than dropping ticks. A closed receiver means the debounce
            // task exited - stop bridging.
            if bridge_tx.blocking_send(tick).is_err() {
                break;
            }
        }
        // raw_rx closed (backend dropped on unwatch / death): dropping
        // bridge_tx closes bridge_rx, which the loop below reads as "died".
    });

    // Monotonic instant of the last EMITTED edit request, for the 1/min cap.
    let mut last_emit: Option<tokio::time::Instant> = None;
    // Whether at least one edit is pending in the current quiet window.
    let mut pending_edit = false;

    loop {
        let next = if pending_edit {
            // We have a pending edit: wait for either a new tick (which
            // restarts the quiet window) or the quiet window to elapse.
            match tokio::time::timeout(DEBOUNCE_QUIET, bridge_rx.recv()).await {
                Ok(Some(tick)) => Some(tick),
                // Quiet window elapsed with no new tick -> fire the debounced
                // edit (subject to the 1/min cap).
                Err(_elapsed) => {
                    pending_edit = false;
                    if rate_allows(&mut last_emit) {
                        send_request(&out_tx, source_id, ScanTickReason::Edit).await;
                    } else {
                        tracing::trace!(
                            target: TARGET,
                            %source_id,
                            "edit scan-tick suppressed by 1/min cap"
                        );
                    }
                    continue;
                }
                // Channel closed while a pending edit was buffered: flush it
                // (cap permitting) so an edit racing a teardown is not lost,
                // then fall through to the `None` arm to exit silently.
                Ok(None) => {
                    if rate_allows(&mut last_emit) {
                        send_request(&out_tx, source_id, ScanTickReason::Edit).await;
                    }
                    None
                }
            }
        } else {
            // Idle: block until the next tick or channel close.
            bridge_rx.recv().await
        };

        match next {
            Some(RawTick::Edit) => {
                // (Re)start the quiet window.
                pending_edit = true;
            }
            Some(RawTick::Degraded) => {
                // Degradation is surfaced immediately, bypassing the debounce,
                // so the orchestrator can fall back without waiting (DESIGN
                // s5.9.4). Not subject to the edit cap.
                send_request(&out_tx, source_id, ScanTickReason::Degraded).await;
            }
            None => {
                // Raw channel closed: the backend handle was dropped on a
                // deliberate unwatch / re-watch. Exit silently - no WatcherDied
                // for an intentional stop (see this fn's docs and
                // [`ScanTickReason::WatcherDied`]).
                break;
            }
        }
    }

    // Ensure the bridge task is not left dangling.
    bridge.abort();
}

/// Returns whether an `Edit` request may be emitted now under the
/// 1-request/minute/source cap, updating `last_emit` to now when it returns
/// `true` (DESIGN s5.9.2). `Degraded` / `WatcherDied` are NOT rate-limited.
fn rate_allows(last_emit: &mut Option<tokio::time::Instant>) -> bool {
    let now = tokio::time::Instant::now();
    match last_emit {
        Some(prev) if now.duration_since(*prev) < RATE_CAP_WINDOW => false,
        _ => {
            *last_emit = Some(now);
            true
        }
    }
}

/// Sends one [`ScanTickRequest`] to the orchestrator, logging (never
/// panicking) if the channel is closed or full. A dropped request is safe:
/// the scheduled scan is the authoritative fallback (DESIGN s5.9.5).
async fn send_request(
    out_tx: &mpsc::Sender<ScanTickRequest>,
    source_id: SourceId,
    reason: ScanTickReason,
) {
    let req = ScanTickRequest { source_id, reason };
    if let Err(err) = out_tx.send(req).await {
        tracing::debug!(
            target: TARGET,
            %source_id,
            ?reason,
            %err,
            "orchestrator scan-tick channel closed; dropping request"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::types::AccountId;

    /// A `SourceRow` rooted at `root`; fields the watcher path never reads are
    /// cheap dummies. Mirrors the exclude-test fixture.
    fn source_at(
        root: &Path,
        respect_gitignore: bool,
        include: &[&str],
        exclude: &[&str],
    ) -> SourceRow {
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
            respect_gitignore,
            include_patterns: include.iter().map(|s| s.to_string()).collect(),
            exclude_patterns: exclude.iter().map(|s| s.to_string()).collect(),
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 0,
        }
    }

    /// Receives the next scan-tick within `dur`, or `None` on timeout. Skips
    /// nothing - returns the first request so a test can assert its reason.
    async fn recv_within(
        rx: &mut mpsc::Receiver<ScanTickRequest>,
        dur: Duration,
    ) -> Option<ScanTickRequest> {
        tokio::time::timeout(dur, rx.recv()).await.ok().flatten()
    }

    #[tokio::test]
    async fn file_drop_fires_scan_tick_within_one_second() {
        // Core DESIGN s5.9 latency guarantee: an edit under a watched source
        // produces a debounced Edit scan-tick promptly (500 ms quiet window +
        // notify latency, comfortably under ~1.5 s here).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let src = source_at(root, true, &[], &[]);
        let source_id = src.id;
        let watcher = NotifyWatcher::new([src]);
        let mut rx = watcher.subscribe().expect("subscribe once");
        watcher.watch(source_id).expect("watch establishes");

        // Give the backend a beat to arm before the edit (avoids a race where
        // the create predates the watch on slow CI).
        tokio::time::sleep(Duration::from_millis(150)).await;
        fs::write(root.join("new.txt"), b"hello").unwrap();

        let req = recv_within(&mut rx, Duration::from_millis(1500))
            .await
            .expect("a scan-tick must fire within ~1s of a file drop");
        assert_eq!(req.source_id, source_id);
        assert_eq!(req.reason, ScanTickReason::Edit, "file drop -> Edit tick");
    }

    #[tokio::test]
    async fn excluded_path_events_are_filtered() {
        // DESIGN s5.9.3: edits confined to an excluded prefix (here a
        // gitignored `.git/`-style dir) must NOT produce a scan-tick, while an
        // edit to a non-excluded file still does. We first prove silence for
        // the excluded edit, then prove the watch is live by editing a kept
        // file and seeing exactly one Edit tick.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // `.gitignore` excludes everything under `noise/`.
        fs::write(root.join(".gitignore"), "noise/\n").unwrap();
        fs::create_dir(root.join("noise")).unwrap();

        let src = source_at(root, true, &[], &[]);
        let source_id = src.id;
        let watcher = NotifyWatcher::new([src]);
        let mut rx = watcher.subscribe().expect("subscribe once");
        watcher.watch(source_id).expect("watch establishes");
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Drain any setup/arming noise queued before the watch fully armed
        // (Linux inotify can still deliver the `.gitignore` create + `noise/`
        // mkdir from above as non-excluded events). Draining BEFORE the
        // excluded write below means this only swallows pre-write arming
        // noise; if the excluded write itself leaked the filter, the assert
        // would still fail (not masked).
        while recv_within(&mut rx, Duration::from_millis(200))
            .await
            .is_some()
        {}

        // An edit confined to the excluded dir: expect NO scan-tick.
        fs::write(root.join("noise").join("junk.tmp"), b"x").unwrap();
        let excluded = recv_within(&mut rx, Duration::from_millis(900)).await;
        assert!(
            excluded.is_none(),
            "edits under an excluded prefix must be filtered, got {excluded:?}"
        );

        // A kept file proves the watch is live and the prior silence was the
        // filter, not a dead watcher.
        fs::write(root.join("keep.txt"), b"y").unwrap();
        let kept = recv_within(&mut rx, Duration::from_millis(1500))
            .await
            .expect("a non-excluded edit must still fire a scan-tick");
        assert_eq!(kept.reason, ScanTickReason::Edit);
    }

    #[tokio::test]
    async fn watch_on_missing_path_errors_without_panic() {
        // DESIGN s5.9.4 hard-failure path: a source whose root does not exist
        // cannot establish a watch, so `watch()` returns Err (the orchestrator
        // falls back to scheduled) - and crucially does NOT panic.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let src = source_at(&missing, true, &[], &[]);
        let source_id = src.id;

        let watcher = NotifyWatcher::new([src]);
        let _rx = watcher.subscribe();
        let res = watcher.watch(source_id);
        assert!(
            res.is_err(),
            "watching a nonexistent path must fail in-band, not panic"
        );
    }

    #[tokio::test]
    async fn unwatch_is_silent_stops_ticks_and_is_idempotent() {
        // A deliberate unwatch is a clean, SILENT teardown (DESIGN s5.9.4):
        // it must NOT emit a WatcherDied tick (that reason is for an
        // unexpected death the orchestrator should restart, not a user stop),
        // it must stop further ticks (a later edit produces nothing), and a
        // second unwatch must be a no-op that does not panic.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let src = source_at(root, true, &[], &[]);
        let source_id = src.id;

        let watcher = NotifyWatcher::new([src]);
        let mut rx = watcher.subscribe().expect("subscribe once");
        watcher.watch(source_id).expect("watch establishes");
        tokio::time::sleep(Duration::from_millis(150)).await;

        watcher.unwatch(source_id);

        // No request of any kind should arrive after a clean unwatch.
        let after_unwatch = recv_within(&mut rx, Duration::from_millis(800)).await;
        assert!(
            after_unwatch.is_none(),
            "a deliberate unwatch must be silent, got {after_unwatch:?}"
        );

        // An edit after unwatch must NOT fire a tick - the watch is gone.
        fs::write(root.join("post.txt"), b"x").unwrap();
        let after_edit = recv_within(&mut rx, Duration::from_millis(1000)).await;
        assert!(
            after_edit.is_none(),
            "edits after unwatch must produce no scan-tick, got {after_edit:?}"
        );

        // Idempotent: a second unwatch must not panic.
        watcher.unwatch(source_id);
    }

    #[tokio::test]
    async fn rewatch_is_idempotent_and_re_establishes() {
        // DESIGN s5.9.4 restart path: calling watch() on an already-watching
        // source re-establishes the watch (replacing the old LiveWatch, whose
        // silent teardown emits nothing) and a subsequent edit still fires a
        // tick. We tolerate any stray request and require a live Edit from the
        // re-established watch.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let src = source_at(root, true, &[], &[]);
        let source_id = src.id;

        let watcher = NotifyWatcher::new([src]);
        let mut rx = watcher.subscribe().expect("subscribe once");
        watcher.watch(source_id).expect("first watch");
        tokio::time::sleep(Duration::from_millis(100)).await;
        watcher.watch(source_id).expect("re-watch is idempotent");
        tokio::time::sleep(Duration::from_millis(150)).await;

        fs::write(root.join("after.txt"), b"z").unwrap();

        // Drain up to a couple requests tolerantly (an editor-style write can
        // produce more than one underlying event); a live Edit from the
        // re-established watch must appear within the window.
        let mut saw_edit = false;
        for _ in 0..3 {
            match recv_within(&mut rx, Duration::from_millis(1500)).await {
                Some(req) if req.reason == ScanTickReason::Edit => {
                    assert_eq!(req.source_id, source_id);
                    saw_edit = true;
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        assert!(
            saw_edit,
            "re-established watch must still deliver Edit ticks"
        );
    }
}
