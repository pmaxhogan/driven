//! Streaming exclusion preview (SPEC s11.2; DESIGN s8.5 step 3).
//!
//! The one-shot [`crate::commands::sources::preview_exclusions`] walks the whole
//! source tree before it returns anything, so a large folder leaves the
//! exclusion editor showing "Loading..." for minutes with no sign of progress or
//! of what the rules are actually doing. This module adds the STREAMING flow the
//! editor uses instead:
//!
//! - `preview_exclusions_start(req)` validates the request (shared
//!   [`crate::commands::sources::resolve_preview_root`] - same glob validation,
//!   same exactly-one-selector rule, same dialog-token peek, same readable-dir
//!   check) and returns a fresh `preview_id` IMMEDIATELY;
//! - the classification pass emits `exclusion_preview:batch` events (every
//!   [`BATCH_MAX_NODES`] nodes or [`BATCH_MAX_INTERVAL`], whichever comes
//!   first), each carrying newly discovered nodes plus the running totals;
//! - it finishes with one `exclusion_preview:done` carrying the exact final
//!   totals, or one `exclusion_preview:error` if the matcher could not be built.
//!
//! Every event is tagged with the `preview_id`, so the webview discards batches
//! from a superseded pass instead of mixing two trees.
//!
//! ## Walking and classifying are separate jobs
//!
//! The pass is driven by [`crate::commands::preview_cache::PreviewTreeCache`]:
//! each directory is read from the cache when it is there and from DISK when it
//! is not (recording it on the way). A first preview of a root is therefore a
//! full disk walk exactly as before, and every LATER preview of the same root -
//! i.e. every rule edit - re-classifies from memory in milliseconds, touching
//! the disk only for subtrees the earlier passes pruned and the new rules now
//! reach into. Editing a glob no longer re-reads the folder.
//!
//! Nothing about the RESULT depends on which path a directory took: a cache miss
//! walks and caches, a cache hit replays the same entries, and the matcher's
//! verdict is computed fresh either way. The cache never stores a verdict.
//!
//! ## Generations, demotion and cancellation
//!
//! The editor re-previews on every rule edit, and a rule edit is exactly when a
//! minutes-long first walk is most likely still running. [`PreviewRegistry`]
//! keeps at most ONE emitting pass plus at most ONE demoted BUILDER:
//!
//! - starting a preview DEMOTES the pass it supersedes - the old pass stops
//!   emitting but keeps walking, because its remaining work is exactly the cache
//!   the new pass is about to want;
//! - if a builder is already running, the superseded pass is cancelled instead,
//!   so a user hammering the textarea can never stack more than two walks (the
//!   oldest builder is the one furthest along, so it is the one worth keeping);
//! - `preview_exclusions_cancel(id)` (fired when the editor closes) cancels
//!   BOTH and frees the cache - nobody is looking at the tree any more.
//!
//! The pass polls the cancel flag between directories and between entries, so it
//! stops within one directory's work rather than burning CPU on a tree nobody is
//! looking at.
//!
//! ## Breadth-first, parents before children
//!
//! The pass is BFS, which matters twice. The webview builds the tree
//! incrementally, so a node's parent MUST arrive first - BFS guarantees it (a
//! directory is emitted while scanning ITS parent, and only descended later).
//! And it makes the live preview useful: the root's own children appear in the
//! first batch and the tree fills downward, instead of DFS diving into one deep
//! subtree and streaming 50k nodes the user cannot see the top of.
//!
//! ## Bounded payload
//!
//! A 10M-file source must not OOM the webview, so node DETAILS stop streaming
//! after [`NODE_STREAM_CAP`] nodes ([`ExclusionPreviewBatch::truncated`] then
//! flips to `true` and the tree stops growing). The COUNTS and byte total keep
//! updating to the exact end of the pass - the summary line stays truthful even
//! when the tree is a partial view.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, State};

use driven_core::exclude::SourceMatcher;

use crate::app_state::AppState;
use crate::commands::dtos::{
    ExclusionPreviewBatch, ExclusionPreviewDone, ExclusionPreviewError, ExclusionPreviewNode,
    ExclusionPreviewRequest,
};
use crate::commands::preview_cache::{CachedEntry, PreviewTreeCache};
use crate::commands::sources::{build_preview_matcher, resolve_preview_root};
use crate::commands::{CommandError, CommandResult};
use crate::events;

/// Tracing target for the streaming preview.
const TARGET: &str = "driven::app::exclusion_stream";

/// Flush a batch once this many nodes have accumulated. Sized so a fast local
/// SSD walk emits a handful of events per second rather than one per file (each
/// event crosses the IPC boundary and wakes the webview), while still keeping a
/// single payload small enough to serialise cheaply.
const BATCH_MAX_NODES: usize = 400;

/// Flush a partial batch after this long, so a SLOW tree (network share, cold
/// spinning disk, permission-heavy directories) still shows movement instead of
/// waiting for [`BATCH_MAX_NODES`] to fill.
const BATCH_MAX_INTERVAL: Duration = Duration::from_millis(100);

/// Stop streaming node DETAILS after this many nodes. The counts stay exact past
/// the cap; only the rendered tree is bounded. 50k nodes is far more than a user
/// will ever expand by hand yet small enough that the webview's index stays a
/// few tens of MB in the worst case.
const NODE_STREAM_CAP: usize = 50_000;

/// Poll the cancel flag every this many directory entries, so a single
/// enormous directory (hundreds of thousands of siblings) still aborts promptly
/// instead of only at the next directory boundary.
const CANCEL_POLL_ENTRIES: usize = 256;

/// The control surface of one classification pass: what stops it, and whether
/// anyone is still listening to it.
#[derive(Clone)]
pub struct PreviewHandle {
    /// The generation id handed to the webview.
    id: String,
    /// Flipped to `true` to ask the pass to stop.
    cancel: Arc<AtomicBool>,
    /// Flipped to `false` when the pass is DEMOTED to a cache builder: it keeps
    /// walking (its work is the cache the live pass wants) but its events are no
    /// longer anyone's tree, so they are dropped rather than emitted.
    emitting: Arc<AtomicBool>,
}

impl PreviewHandle {
    fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            cancel: Arc::new(AtomicBool::new(false)),
            emitting: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Has this pass been asked to stop?
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Is anyone still showing this pass's tree?
    pub fn emitting(&self) -> bool {
        self.emitting.load(Ordering::SeqCst)
    }

    /// The cancel flag, for [`stream_classify_tree`].
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }
}

/// Tracks the live classification pass and the at-most-one demoted cache builder
/// (see the module docs).
#[derive(Default)]
pub struct PreviewRegistry {
    inner: Mutex<RegistryState>,
}

#[derive(Default)]
struct RegistryState {
    /// The pass whose events the webview is rendering.
    active: Option<PreviewHandle>,
    /// A superseded pass still filling the cache. At most one, and always the
    /// OLDEST survivor - it is the one furthest into the tree.
    builder: Option<PreviewHandle>,
}

impl PreviewRegistry {
    /// Register `id` as the live pass and return its handle.
    ///
    /// The pass it supersedes is demoted to the builder slot when that slot is
    /// free (it keeps walking, silently, so its remaining work becomes the cache
    /// the new pass wants) and cancelled outright when it is not - bounding the
    /// app to two concurrent walks no matter how fast the user edits.
    pub fn start(&self, id: &str) -> PreviewHandle {
        let handle = PreviewHandle::new(id);
        let mut guard = self.lock();
        if let Some(prev) = guard.active.take() {
            if guard.builder.is_none() {
                prev.emitting.store(false, Ordering::SeqCst);
                tracing::debug!(target: TARGET, demoted = %prev.id, by = %id, "demoting the superseded exclusion preview to a cache builder");
                guard.builder = Some(prev);
            } else {
                prev.cancel.store(true, Ordering::SeqCst);
                tracing::debug!(target: TARGET, cancelled = %prev.id, by = %id, "cancelling the superseded exclusion preview (a cache builder is already running)");
            }
        }
        guard.active = Some(handle.clone());
        handle
    }

    /// Cancel the live pass if it is `id`, along with any cache builder. Returns
    /// `true` when a matching preview was cancelled; a stale id is a no-op (the
    /// webview may cancel a generation the backend has already superseded or
    /// finished).
    pub fn cancel(&self, id: &str) -> bool {
        let mut guard = self.lock();
        if !guard.active.as_ref().is_some_and(|a| a.id == id) {
            return false;
        }
        if let Some(active) = guard.active.take() {
            active.cancel.store(true, Ordering::SeqCst);
        }
        if let Some(builder) = guard.builder.take() {
            builder.cancel.store(true, Ordering::SeqCst);
        }
        true
    }

    /// Cancel EVERY pass, live or building. Used when the previewed root changes
    /// under us: a walk of the old root has nothing left to contribute.
    pub fn cancel_all(&self) {
        let mut guard = self.lock();
        for handle in [guard.active.take(), guard.builder.take()]
            .into_iter()
            .flatten()
        {
            handle.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Deregister `id` after its pass ended - matching either slot, and only the
    /// slot it actually occupies, so a superseded pass finishing late cannot
    /// clear the registration of the preview that replaced it.
    pub fn finish(&self, id: &str) {
        let mut guard = self.lock();
        if guard.active.as_ref().is_some_and(|a| a.id == id) {
            guard.active = None;
        }
        if guard.builder.as_ref().is_some_and(|b| b.id == id) {
            guard.builder = None;
        }
    }

    /// The live generation id, for tests / diagnostics.
    #[cfg(test)]
    fn current_id(&self) -> Option<String> {
        self.lock().active.as_ref().map(|a| a.id.clone())
    }

    /// The demoted cache builder's id, for tests / diagnostics.
    #[cfg(test)]
    fn builder_id(&self) -> Option<String> {
        self.lock().builder.as_ref().map(|b| b.id.clone())
    }

    /// Lock the slots, recovering a poisoned lock (house rule: never panic on a
    /// poisoned lock).
    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The knobs [`stream_classify_tree`] runs under. Production uses
/// [`StreamConfig::default`]; tests shrink the thresholds so the batching,
/// truncation, and cancellation behaviours are observable on a tiny fixture
/// tree instead of needing 50k real files.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Flush a batch once this many nodes have accumulated.
    pub batch_max_nodes: usize,
    /// Flush a partial batch after this long.
    pub batch_max_interval: Duration,
    /// Stop streaming node details after this many nodes (counts continue).
    pub node_stream_cap: usize,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            batch_max_nodes: BATCH_MAX_NODES,
            batch_max_interval: BATCH_MAX_INTERVAL,
            node_stream_cap: NODE_STREAM_CAP,
        }
    }
}

/// A directory waiting to be classified, in BFS order.
///
/// Only the root-relative path today. When the per-directory decision cursor
/// lands it rides along here too, so descending re-uses the parent's cascade
/// state instead of re-deriving it from the root for every entry.
struct QueuedDir {
    /// Path relative to the source root; empty for the root itself.
    rel: PathBuf,
}

/// Read one directory off disk into the cache's entry form, applying the walk
/// policy the scanner uses: symlinks are never followed (DESIGN s5.2.1 `Skip`),
/// and anything that is neither a regular file nor a directory is ignored.
///
/// Returns `None` for an unreadable directory (a permission denial or transient
/// error), which the caller logs and skips rather than failing the preview.
fn read_dir_entries(abs: &Path) -> Option<Vec<CachedEntry>> {
    let read = match std::fs::read_dir(abs) {
        Ok(r) => r,
        Err(err) => {
            tracing::debug!(target: TARGET, dir = %abs.display(), %err, "streaming preview: skipping unreadable directory");
            return None;
        }
    };
    let mut out: Vec<CachedEntry> = Vec::new();
    for entry in read.flatten() {
        // Use the entry's OWN type, not the dereferenced target.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let is_dir = file_type.is_dir();
        if !is_dir && !file_type.is_file() {
            continue;
        }
        out.push(CachedEntry {
            name: entry.file_name(),
            is_dir,
            size: if is_dir {
                0
            } else {
                entry.metadata().map(|m| m.len()).unwrap_or(0)
            },
        });
    }
    // The vector is about to be cached for the life of the editor session, and
    // `push` growth leaves up to half its capacity unused - across hundreds of
    // thousands of directories that slack is the difference between fitting the
    // memory budget and overflowing it.
    out.shrink_to_fit();
    Some(out)
}

/// Classify every entry under `root` with `matcher`, breadth-first, handing each
/// accumulated batch to `emit`. Returns the terminal totals.
///
/// Directories come from `cache` when it has them and from DISK when it does not
/// (recording them on the way), so the FIRST pass over a root is a full walk and
/// every later pass over the same root is near-instant - see the module docs.
/// The cache holds only what is on disk; the include/exclude verdict is computed
/// fresh on every pass, which is what makes re-running it after a rule edit
/// meaningful.
///
/// Synchronous and blocking - the command runs it under `spawn_blocking`. It
/// takes the sink as a closure rather than an `AppHandle` so the whole pass
/// (batching, truncation, cancellation, ordering, cache reuse) is unit-testable
/// with a `Vec` sink and no Tauri runtime.
///
/// Mirrors the scanner's walk policy exactly, because the preview's whole job is
/// to predict it: symlinks are never followed, an excluded directory is
/// descended anyway when a `!`-re-include could reach under THAT directory
/// (`SourceMatcher::negations_could_match_under` - the same per-directory P1-1
/// lockstep rule `build_walker` applies), and an unreadable directory is logged
/// and skipped rather than failing the preview.
pub fn stream_classify_tree(
    root: &Path,
    matcher: &SourceMatcher,
    cache: &PreviewTreeCache,
    cancel: &AtomicBool,
    cfg: &StreamConfig,
    preview_id: &str,
    mut emit: impl FnMut(ExclusionPreviewBatch),
) -> ExclusionPreviewDone {
    let mut included_count: u64 = 0;
    let mut excluded_count: u64 = 0;
    let mut included_bytes: u64 = 0;
    let mut streamed_nodes: usize = 0;
    let mut truncated = false;

    let mut pending: Vec<ExclusionPreviewNode> = Vec::with_capacity(cfg.batch_max_nodes);
    let mut last_flush = Instant::now();

    // BFS: a directory is emitted while scanning its PARENT and only descended
    // when it comes off the front of the queue, so every node's parent has
    // already been streamed by the time the node itself is.
    let mut queue: VecDeque<QueuedDir> = VecDeque::new();
    queue.push_back(QueuedDir {
        rel: PathBuf::new(),
    });

    let cancelled = 'walk: loop {
        let Some(dir) = queue.pop_front() else {
            break false;
        };
        if cancel.load(Ordering::SeqCst) {
            break true;
        }
        // The cache-or-disk decision, and the ONLY place this pass can touch the
        // disk. A hit costs one hash lookup and an `Arc` clone.
        let entries = match cache.get(root, &dir.rel) {
            Some(entries) => entries,
            None => {
                let Some(fresh) = read_dir_entries(&root.join(&dir.rel)) else {
                    continue;
                };
                let entries = Arc::new(fresh);
                cache.insert(root, &dir.rel, Arc::clone(&entries));
                entries
            }
        };

        for (seen, entry) in entries.iter().enumerate() {
            if seen % CANCEL_POLL_ENTRIES == 0 && cancel.load(Ordering::SeqCst) {
                break 'walk true;
            }
            let rel = dir.rel.join(&entry.name);
            let is_dir = entry.is_dir;

            let included = matcher.is_included(&rel, is_dir);
            let size = entry.size;
            let rel_str = rel.to_string_lossy().replace('\\', "/");

            if is_dir {
                // Descend unless this dir is excluded AND pruning it is safe -
                // i.e. no `!`-rule anywhere could re-include something beneath
                // THIS directory (the scanner's own per-directory rule).
                if included || matcher.negations_could_match_under(&rel) {
                    queue.push_back(QueuedDir { rel });
                }
            } else if included {
                included_count += 1;
                included_bytes = included_bytes.saturating_add(size);
            } else {
                excluded_count += 1;
            }

            // Past the cap the tree stops growing but the counts above keep
            // going, so the summary line stays exact on a source far larger than
            // the webview could ever render.
            if streamed_nodes < cfg.node_stream_cap {
                streamed_nodes += 1;
                pending.push(ExclusionPreviewNode {
                    path: rel_str,
                    is_dir,
                    included,
                    size,
                });
            } else if !truncated {
                truncated = true;
                tracing::debug!(target: TARGET, cap = cfg.node_stream_cap, "streaming preview: node cap reached; counts continue");
            }

            if pending.len() >= cfg.batch_max_nodes
                || last_flush.elapsed() >= cfg.batch_max_interval
            {
                emit(ExclusionPreviewBatch {
                    preview_id: preview_id.to_string(),
                    nodes: std::mem::take(&mut pending),
                    included_count,
                    excluded_count,
                    included_bytes,
                    truncated,
                });
                pending.reserve(cfg.batch_max_nodes);
                last_flush = Instant::now();
                // A batch boundary is the other natural cancel checkpoint. The
                // per-entry poll above only fires every CANCEL_POLL_ENTRIES, so
                // without this a cancel arriving mid-directory would not be seen
                // until the next multiple - and a single directory can hold every
                // entry there is. Checking here makes the guarantee the UI
                // depends on concrete: a cancelled pass stops within one batch.
                if cancel.load(Ordering::SeqCst) {
                    break 'walk true;
                }
            }
        }
    };

    // Final partial batch (also the ONLY batch for a small tree). Skipped when
    // empty so a finished pass does not emit a redundant no-op event.
    if !pending.is_empty() {
        emit(ExclusionPreviewBatch {
            preview_id: preview_id.to_string(),
            nodes: std::mem::take(&mut pending),
            included_count,
            excluded_count,
            included_bytes,
            truncated,
        });
    }

    ExclusionPreviewDone {
        preview_id: preview_id.to_string(),
        included_count,
        excluded_count,
        included_bytes,
        truncated,
        cancelled,
    }
}

/// `preview_exclusions_start(req)` - begin a STREAMING exclusion preview and
/// return its generation id (SPEC s11.2; DESIGN s8.5 step 3).
///
/// Validates the request exactly as [`crate::commands::sources::preview_exclusions`]
/// does (shared [`resolve_preview_root`]), so an invalid glob or a request
/// without exactly one trusted root selector fails FAST with the same stable
/// SPEC s24 code - before any classification starts and before the webview
/// touches its tree.
///
/// Everything that can be slow happens AFTER the id is returned, on a blocking
/// thread: building the matcher collects the source's `.gitignore` cascade off
/// disk, which on a big repo-of-repos is itself seconds of I/O, and the
/// classification pass may have to walk. Holding the id back until either was
/// done left the editor with nothing to render and no way to tell "starting"
/// from "hung". A matcher that fails to build now reports through
/// `exclusion_preview:error` rather than as a rejected call.
#[tauri::command]
pub async fn preview_exclusions_start(
    app: AppHandle,
    state: State<'_, AppState>,
    req: ExclusionPreviewRequest,
) -> CommandResult<String> {
    let canon = resolve_preview_root(&state, &req).await?;

    let registry = state.exclusion_previews();
    let cache = state.preview_tree_cache();
    // A different folder invalidates everything cached, and any walk still
    // filling the old root's cache has nothing left to contribute.
    if cache.begin(&canon) {
        registry.cancel_all();
    }

    let preview_id = uuid::Uuid::new_v4().to_string();
    let handle = registry.start(&preview_id);

    let id_for_pass = preview_id.clone();
    let pass = tokio::task::spawn_blocking(move || {
        let matcher = match build_preview_matcher(&canon, &req) {
            Ok(matcher) => matcher,
            Err(err) => {
                let err = CommandError::from(err);
                tracing::warn!(target: TARGET, %err, "exclusion preview: matcher build failed");
                registry.finish(&id_for_pass);
                if handle.emitting() {
                    let payload = ExclusionPreviewError {
                        preview_id: id_for_pass,
                        code: err.code.to_string(),
                        message: err.message,
                    };
                    if let Err(err) = events::emit_exclusion_preview_error(&app, &payload) {
                        tracing::warn!(target: TARGET, %err, "emit exclusion_preview:error failed");
                    }
                }
                return;
            }
        };

        let cfg = StreamConfig::default();
        let done = stream_classify_tree(
            &canon,
            &matcher,
            cache.as_ref(),
            handle.cancel_flag(),
            &cfg,
            &id_for_pass,
            |batch| {
                // A demoted pass is still walking - to fill the cache - but its
                // tree is nobody's tree any more, so its batches are dropped
                // rather than crossing the IPC boundary.
                if !handle.emitting() {
                    return;
                }
                if let Err(err) = events::emit_exclusion_preview_batch(&app, &batch) {
                    tracing::warn!(target: TARGET, %err, "emit exclusion_preview:batch failed");
                }
            },
        );
        registry.finish(&id_for_pass);
        if handle.emitting() {
            if let Err(err) = events::emit_exclusion_preview_done(&app, &done) {
                tracing::warn!(target: TARGET, %err, "emit exclusion_preview:done failed");
            }
        }
    });
    // The pass owns its own reporting; nothing awaits it. Dropping the handle
    // detaches the task rather than cancelling it (tokio semantics), which is
    // what we want - the cancel FLAG is the stop signal, not handle drop.
    drop(pass);

    Ok(preview_id)
}

/// Stop the preview with this generation id and free everything the editor was
/// holding. The testable half of [`preview_exclusions_cancel`]: returns whether
/// `preview_id` matched the live generation.
fn cancel_preview(registry: &PreviewRegistry, cache: &PreviewTreeCache, preview_id: &str) -> bool {
    let cancelled = registry.cancel(preview_id);
    if cancelled {
        // The editor is gone, so the tree it was showing is dead weight - and a
        // cache is exactly the thing that quietly keeps hundreds of MB alive
        // long after the view that wanted it closed.
        cache.clear();
    }
    cancelled
}

/// `preview_exclusions_cancel(previewId)` - stop the streaming preview with this
/// generation id (SPEC s11.2).
///
/// Called when the exclusion editor closes (wizard cancelled / step left / the
/// inline panel collapsed). It stops the live pass AND any demoted cache
/// builder, and frees the folder-tree cache: a minutes-long pass over a huge
/// source must not keep a CPU busy - nor hold the tree in memory - for a view
/// nobody is looking at. A start of a NEW preview already supersedes the
/// previous one, so this is only needed for the nothing-replaces-it case.
/// Cancelling an unknown / already-finished generation is a deliberate no-op,
/// not an error: the webview cannot know whether the pass beat it to the finish.
#[tauri::command]
pub async fn preview_exclusions_cancel(
    state: State<'_, AppState>,
    preview_id: String,
) -> CommandResult<()> {
    let cancelled = cancel_preview(
        state.exclusion_previews().as_ref(),
        state.preview_tree_cache().as_ref(),
        &preview_id,
    );
    tracing::debug!(target: TARGET, preview_id = %preview_id, cancelled, "exclusion preview cancel requested");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use driven_core::exclude::build_source_matcher;
    use driven_core::state::{PlaceholderPolicy, SourceRow};
    use driven_core::types::{AccountId, SourceId};

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, contents).expect("write");
    }

    /// A synthetic `SourceRow` rooted at `root`, matching the one
    /// `build_preview_matcher` builds for a preview.
    fn source_at(root: &Path, include: &[&str], exclude: &[&str]) -> SourceRow {
        SourceRow {
            id: SourceId::new_v4(),
            account_id: AccountId::new_v4(),
            display_name: String::new(),
            enabled: true,
            local_path: root.to_string_lossy().into_owned(),
            drive_folder_id: String::new(),
            drive_id: None,
            drive_folder_path: String::new(),
            encryption_enabled: false,
            wrapped_source_key: None,
            // The gitignore tier is off so these fixtures depend only on the
            // candidate globs (and the DESIGN s5.2 defaults).
            respect_gitignore: false,
            include_patterns: include.iter().map(|s| s.to_string()).collect(),
            exclude_patterns: exclude.iter().map(|s| s.to_string()).collect(),
            placeholder_policy: PlaceholderPolicy::Skip,
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            mtime_granularity_ns: None,
            created_at: 0,
        }
    }

    /// Run a pass to completion against a FRESH cache, collecting every batch.
    fn run(
        root: &Path,
        source: &SourceRow,
        cfg: &StreamConfig,
    ) -> (Vec<ExclusionPreviewBatch>, ExclusionPreviewDone) {
        let cache = PreviewTreeCache::default();
        cache.begin(root);
        run_with_cache(root, source, cfg, &cache)
    }

    /// Run a pass to completion against `cache`, collecting every batch.
    fn run_with_cache(
        root: &Path,
        source: &SourceRow,
        cfg: &StreamConfig,
        cache: &PreviewTreeCache,
    ) -> (Vec<ExclusionPreviewBatch>, ExclusionPreviewDone) {
        let matcher = build_source_matcher(source).expect("matcher");
        let cancel = AtomicBool::new(false);
        let mut batches = Vec::new();
        let done = stream_classify_tree(root, &matcher, cache, &cancel, cfg, "gen-1", |b| {
            batches.push(b)
        });
        (batches, done)
    }

    /// Every node from every batch, in emission order.
    fn all_nodes(batches: &[ExclusionPreviewBatch]) -> Vec<ExclusionPreviewNode> {
        batches.iter().flat_map(|b| b.nodes.clone()).collect()
    }

    /// The `(path, is_dir, included, size)` tuples of a pass, sorted.
    ///
    /// Sorted rather than in emission order because two passes over the same
    /// tree can legitimately visit one directory's entries in different orders
    /// (`read_dir` guarantees no ordering) - pinning the sequence would be a
    /// flaky test, not a stronger one. Emission ORDER is pinned separately, and
    /// far more meaningfully, by `every_node_arrives_after_its_parent`.
    fn verdicts(batches: &[ExclusionPreviewBatch]) -> Vec<(String, bool, bool, u64)> {
        let mut out: Vec<(String, bool, bool, u64)> = all_nodes(batches)
            .iter()
            .map(|n| (n.path.clone(), n.is_dir, n.included, n.size))
            .collect();
        out.sort();
        out
    }

    /// Assert `nodes` never mentions a child before its parent - the invariant
    /// the webview's incremental tree depends on.
    fn assert_parents_first(nodes: &[ExclusionPreviewNode]) {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for node in nodes {
            if let Some((parent, _)) = node.path.rsplit_once('/') {
                assert!(
                    seen.contains(parent),
                    "{} streamed before its parent {parent}",
                    node.path
                );
            }
            seen.insert(&node.path);
        }
    }

    #[test]
    fn streams_every_node_with_the_matchers_verdict_and_exact_totals() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("keep.txt"), "abcde");
        write(&root.join("drop.log"), "xy");
        write(&root.join("docs/inner.txt"), "abc");
        write(&root.join("docs/inner.log"), "z");

        let source = source_at(root, &[], &["*.log"]);
        let (batches, done) = run(root, &source, &StreamConfig::default());
        let nodes = all_nodes(&batches);

        let paths: Vec<(String, bool, bool)> = verdicts(&batches)
            .into_iter()
            .map(|(p, d, i, _)| (p, d, i))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("docs".to_string(), true, true),
                ("docs/inner.log".to_string(), false, false),
                ("docs/inner.txt".to_string(), false, true),
                ("drop.log".to_string(), false, false),
                ("keep.txt".to_string(), false, true),
            ]
        );

        assert_eq!(done.included_count, 2, "keep.txt + docs/inner.txt");
        assert_eq!(done.excluded_count, 2, "drop.log + docs/inner.log");
        assert_eq!(done.included_bytes, 8, "5 + 3 bytes of included files");
        assert!(!done.truncated);
        assert!(!done.cancelled);
        assert_eq!(done.preview_id, "gen-1");
        // Directories are streamed for the tree but never counted as files.
        assert_eq!(
            nodes.iter().filter(|n| n.is_dir).count(),
            1,
            "the one directory is streamed"
        );
        assert!(
            nodes.iter().filter(|n| n.is_dir).all(|n| n.size == 0),
            "a directory carries no size"
        );
    }

    #[test]
    fn every_node_arrives_after_its_parent() {
        // The webview builds the tree incrementally, so a child whose parent has
        // not been streamed yet would have nowhere to attach. BFS guarantees the
        // ordering; this pins it against a deep + wide tree, on BOTH the
        // walked-from-disk pass and the replayed-from-cache one (the cache is
        // per directory, so a bug that queued a subtree before its parent would
        // survive into the replay).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..6 {
            write(&root.join(format!("top{i}/mid/deep/leaf.txt")), "x");
            write(&root.join(format!("top{i}/sibling.txt")), "x");
        }

        let source = source_at(root, &[], &[]);
        // A tiny batch size so the ordering is exercised across many batches.
        let cfg = StreamConfig {
            batch_max_nodes: 3,
            ..StreamConfig::default()
        };
        let cache = PreviewTreeCache::default();
        cache.begin(root);

        let (batches, done) = run_with_cache(root, &source, &cfg, &cache);
        assert_eq!(done.included_count, 12);
        assert_parents_first(&all_nodes(&batches));

        let (cached_batches, cached_done) = run_with_cache(root, &source, &cfg, &cache);
        assert_eq!(cached_done.included_count, 12);
        assert_parents_first(&all_nodes(&cached_batches));
    }

    #[test]
    fn batches_are_flushed_at_the_node_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..10 {
            write(&root.join(format!("f{i}.txt")), "x");
        }

        let source = source_at(root, &[], &[]);
        let cfg = StreamConfig {
            batch_max_nodes: 4,
            // An interval long enough that only the node threshold can fire.
            batch_max_interval: Duration::from_secs(3600),
            ..StreamConfig::default()
        };
        let (batches, done) = run(root, &source, &cfg);

        assert_eq!(done.included_count, 10);
        assert_eq!(all_nodes(&batches).len(), 10, "no node is lost to batching");
        assert_eq!(batches.len(), 3, "4 + 4 + a final partial 2");
        assert_eq!(batches[0].nodes.len(), 4);
        assert_eq!(batches[1].nodes.len(), 4);
        assert_eq!(batches[2].nodes.len(), 2);
        // Running totals climb with each batch and match the final totals.
        assert!(batches[0].included_count <= batches[1].included_count);
        assert_eq!(
            batches.last().unwrap().included_count,
            done.included_count,
            "the last batch already carries the final count"
        );
        assert!(batches.iter().all(|b| b.preview_id == "gen-1"));
    }

    #[test]
    fn a_slow_tree_still_flushes_on_the_interval() {
        // With a zero interval EVERY entry flushes, proving the time-based path
        // fires independently of the node threshold (which is set high enough
        // here that it can never trigger).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..5 {
            write(&root.join(format!("f{i}.txt")), "x");
        }

        let source = source_at(root, &[], &[]);
        let cfg = StreamConfig {
            batch_max_nodes: 10_000,
            batch_max_interval: Duration::ZERO,
            ..StreamConfig::default()
        };
        let (batches, done) = run(root, &source, &cfg);

        assert_eq!(done.included_count, 5);
        assert_eq!(batches.len(), 5, "one batch per entry at a zero interval");
        assert!(batches.iter().all(|b| b.nodes.len() == 1));
    }

    #[test]
    fn the_node_cap_truncates_the_tree_but_never_the_counts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..20 {
            write(&root.join(format!("inc{i}.txt")), "ab");
            write(&root.join(format!("exc{i}.log")), "c");
        }

        let source = source_at(root, &[], &["*.log"]);
        let cfg = StreamConfig {
            batch_max_nodes: 3,
            node_stream_cap: 7,
            ..StreamConfig::default()
        };
        let (batches, done) = run(root, &source, &cfg);

        assert_eq!(all_nodes(&batches).len(), 7, "node details stop at the cap");
        assert!(done.truncated, "the done event reports the truncation");
        assert!(
            batches.last().unwrap().truncated,
            "and so does the last batch, so the notice can show mid-scan"
        );
        // The counts are the whole point: they must be exact despite the cap.
        assert_eq!(done.included_count, 20);
        assert_eq!(done.excluded_count, 20);
        assert_eq!(done.included_bytes, 40, "20 included files x 2 bytes");
    }

    #[test]
    fn cancellation_stops_the_pass_and_marks_the_done_event() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..50 {
            write(&root.join(format!("f{i}.txt")), "x");
        }

        let matcher = build_source_matcher(&source_at(root, &[], &[])).expect("matcher");
        let cancel = AtomicBool::new(false);
        let cache = PreviewTreeCache::default();
        cache.begin(root);
        let cfg = StreamConfig {
            batch_max_nodes: 2,
            ..StreamConfig::default()
        };
        let mut batches = Vec::new();
        // Cancel from inside the sink, the moment the first batch lands - the
        // real trigger is the same shape (a new preview / the editor closing
        // while the pass is mid-flight).
        let done = stream_classify_tree(root, &matcher, &cache, &cancel, &cfg, "gen-1", |b| {
            batches.push(b);
            cancel.store(true, Ordering::SeqCst);
        });

        assert!(done.cancelled, "the done event reports the cancellation");
        assert!(
            done.included_count < 50,
            "the pass stopped early, it did not run to completion ({} of 50)",
            done.included_count
        );
        assert!(
            !batches.is_empty(),
            "batches emitted before the cancel stand"
        );
    }

    #[test]
    fn a_pre_cancelled_pass_does_no_work() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.txt"), "x");

        let matcher = build_source_matcher(&source_at(root, &[], &[])).expect("matcher");
        let cancel = AtomicBool::new(true);
        let cache = PreviewTreeCache::default();
        cache.begin(root);
        let mut batches = Vec::new();
        let done = stream_classify_tree(
            root,
            &matcher,
            &cache,
            &cancel,
            &StreamConfig::default(),
            "gen-1",
            |b| batches.push(b),
        );

        assert!(done.cancelled);
        assert!(batches.is_empty(), "nothing streamed");
        assert_eq!(done.included_count, 0);
        assert_eq!(
            cache.entry_count(),
            0,
            "and nothing was walked, so nothing cached"
        );
    }

    #[test]
    fn an_excluded_directory_is_pruned_unless_a_negation_could_reach_into_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("vendor/lib.js"), "x");
        write(&root.join("vendor/keep.js"), "x");
        write(&root.join("src/app.js"), "x");

        // No negations: the excluded directory is streamed as one excluded node
        // and never descended, so its contents cost nothing.
        let (batches, done) = run(
            root,
            &source_at(root, &[], &["/vendor/"]),
            &StreamConfig::default(),
        );
        let nodes = all_nodes(&batches);
        assert!(nodes.iter().any(|n| n.path == "vendor" && !n.included));
        assert!(
            !nodes.iter().any(|n| n.path.starts_with("vendor/")),
            "a pruned directory streams no children"
        );
        assert_eq!(done.included_count, 1, "only src/app.js");
        assert_eq!(done.excluded_count, 0, "vendor's files were never visited");

        // With a re-include reaching inside, the same directory IS descended, so
        // the re-included file is visible and correctly classified - matching the
        // scanner's own lockstep rule.
        let (batches, done) = run(
            root,
            &source_at(root, &["/vendor/keep.js"], &["/vendor/"]),
            &StreamConfig::default(),
        );
        let nodes = all_nodes(&batches);
        let keep = nodes
            .iter()
            .find(|n| n.path == "vendor/keep.js")
            .expect("the re-included file is streamed");
        assert!(keep.included, "and is classified included");
        let lib = nodes
            .iter()
            .find(|n| n.path == "vendor/lib.js")
            .expect("its sibling is streamed too");
        assert!(!lib.included, "but stays excluded");
        assert_eq!(done.included_count, 2, "src/app.js + vendor/keep.js");
        assert_eq!(done.excluded_count, 1, "vendor/lib.js");

        // And the per-DIRECTORY rule: a re-include that exists but cannot reach
        // into this directory prunes it just the same. Before the per-directory
        // check any `!`-rule anywhere disabled pruning for the whole tree, so a
        // user re-including one file elsewhere paid a full descent of every
        // excluded folder - the exact behaviour that made the preview crawl on
        // a source with a `node_modules`.
        let (batches, done) = run(
            root,
            &source_at(root, &["/src/app.js"], &["/vendor/"]),
            &StreamConfig::default(),
        );
        let nodes = all_nodes(&batches);
        assert!(nodes.iter().any(|n| n.path == "vendor" && !n.included));
        assert!(
            !nodes.iter().any(|n| n.path.starts_with("vendor/")),
            "an unreachable negation must not defeat the prune"
        );
        assert_eq!(done.included_count, 1, "only src/app.js");
        assert_eq!(done.excluded_count, 0, "vendor's files were never visited");
    }

    #[test]
    fn an_empty_tree_emits_no_batch_and_a_zeroed_done() {
        let dir = tempfile::tempdir().unwrap();
        let (batches, done) = run(
            dir.path(),
            &source_at(dir.path(), &[], &[]),
            &StreamConfig::default(),
        );
        assert!(batches.is_empty());
        assert_eq!(done.included_count, 0);
        assert_eq!(done.excluded_count, 0);
        assert_eq!(done.included_bytes, 0);
        assert!(!done.truncated);
        assert!(!done.cancelled);
    }

    // ---------------------------------------------------------------------
    // Cache reuse
    // ---------------------------------------------------------------------

    #[test]
    fn a_second_pass_classifies_entirely_from_memory() {
        // The headline claim: after one pass, re-previewing the same root does
        // not touch the disk. Proved the only way that cannot be faked - by
        // DELETING the fixture between the two passes. A pass that still read
        // `read_dir` would come back empty.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write(&root.join("keep.txt"), "abcde");
        write(&root.join("docs/inner.txt"), "abc");
        write(&root.join("docs/notes.log"), "zz");

        let cache = PreviewTreeCache::default();
        cache.begin(&root);
        let (first, first_done) = run_with_cache(
            &root,
            &source_at(&root, &[], &[]),
            &StreamConfig::default(),
            &cache,
        );
        assert_eq!(first_done.included_count, 3);
        assert_eq!(cache.entry_count(), 4, "root's 2 entries + docs' 2");

        fs::remove_dir_all(&root).expect("remove the fixture");
        assert!(!root.exists());

        let (second, second_done) = run_with_cache(
            &root,
            &source_at(&root, &[], &[]),
            &StreamConfig::default(),
            &cache,
        );
        assert_eq!(
            verdicts(&second),
            verdicts(&first),
            "the same tree, from memory, with the disk gone"
        );
        assert_eq!(second_done.included_count, 3);
        assert_eq!(second_done.included_bytes, first_done.included_bytes);
        assert_parents_first(&all_nodes(&second));
    }

    #[test]
    fn a_rule_edit_reclassifies_from_the_cache_with_no_disk_access() {
        // The user's actual workflow: type a glob, see the tree re-colour. The
        // verdicts must follow the NEW rules (the cache stores what is on disk,
        // never a verdict) while the disk stays untouched.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write(&root.join("keep.txt"), "abcde");
        write(&root.join("notes.log"), "zz");

        let cache = PreviewTreeCache::default();
        cache.begin(&root);
        run_with_cache(
            &root,
            &source_at(&root, &[], &[]),
            &StreamConfig::default(),
            &cache,
        );

        fs::remove_dir_all(&root).expect("remove the fixture");

        let (batches, done) = run_with_cache(
            &root,
            &source_at(&root, &[], &["*.log"]),
            &StreamConfig::default(),
            &cache,
        );
        assert_eq!(
            verdicts(&batches),
            vec![
                ("keep.txt".to_string(), false, true, 5),
                ("notes.log".to_string(), false, false, 2),
            ],
            "the new rule re-classified the cached tree"
        );
        assert_eq!(done.included_count, 1);
        assert_eq!(done.excluded_count, 1);
        assert_eq!(done.included_bytes, 5);
    }

    #[test]
    fn a_frontier_directory_the_new_rules_reach_into_is_walked_lazily() {
        // The interesting half of the cache: the first pass PRUNED `vendor`, so
        // it is not cached at all. A later rule that reaches inside must descend
        // it from disk and produce exactly what a fresh full walk would - the
        // cache must not make a pruned subtree invisible.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("vendor/lib.js"), "x");
        write(&root.join("vendor/keep.js"), "yy");
        write(&root.join("src/app.js"), "zzz");

        let cache = PreviewTreeCache::default();
        cache.begin(root);
        let (_, pruned_done) = run_with_cache(
            root,
            &source_at(root, &[], &["/vendor/"]),
            &StreamConfig::default(),
            &cache,
        );
        assert_eq!(pruned_done.included_count, 1, "only src/app.js");
        assert!(
            cache.get(root, Path::new("vendor")).is_none(),
            "a pruned directory is not cached - it was never read"
        );

        // Now re-include one file inside it. `vendor` is a cache MISS, so it is
        // walked from disk on the fly and appended to the cache.
        let reached = source_at(root, &["/vendor/keep.js"], &["/vendor/"]);
        let (batches, done) = run_with_cache(root, &reached, &StreamConfig::default(), &cache);
        let (fresh_batches, fresh_done) = run(root, &reached, &StreamConfig::default());

        assert_eq!(
            verdicts(&batches),
            verdicts(&fresh_batches),
            "the lazily-walked frontier matches a fresh full walk exactly"
        );
        assert_eq!(done.included_count, fresh_done.included_count);
        assert_eq!(done.excluded_count, fresh_done.excluded_count);
        assert_eq!(done.included_bytes, fresh_done.included_bytes);
        assert_parents_first(&all_nodes(&batches));
        assert!(
            cache.get(root, Path::new("vendor")).is_some(),
            "and the frontier directory is now cached for the next edit"
        );
    }

    #[test]
    fn every_rule_edit_over_a_shared_cache_matches_a_fresh_walk() {
        // The property that makes the cache safe to ship: for a MATRIX of rule
        // edits applied one after another to the same long-lived cache, each
        // pass's nodes and totals equal what a pass with an empty cache produces
        // for the same rules. Covers pruning then un-pruning, re-pruning, and
        // rules that reach into a directory an earlier edit had pruned.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("src/app.js"), "aaaa");
        write(&root.join("src/app.test.js"), "bb");
        write(&root.join("src/deep/util.js"), "ccc");
        write(&root.join("vendor/lib.js"), "d");
        write(&root.join("vendor/keep.js"), "ee");
        write(&root.join("vendor/nested/deep.js"), "fff");
        write(&root.join("build/out.min.js"), "g");
        write(&root.join("notes.md"), "hh");
        write(&root.join("notes.log"), "i");

        let edits: Vec<(Vec<&str>, Vec<&str>)> = vec![
            (vec![], vec![]),
            (vec![], vec!["*.log"]),
            (vec![], vec!["*.log", "/vendor/"]),
            (vec!["/vendor/keep.js"], vec!["*.log", "/vendor/"]),
            (
                vec!["/vendor/keep.js"],
                vec!["*.log", "/vendor/", "/build/"],
            ),
            (vec![], vec!["/vendor/", "/build/", "/src/deep/"]),
            (vec!["/src/deep/util.js"], vec!["/vendor/", "/src/deep/"]),
            (vec![], vec!["*.js"]),
            (vec!["/src/app.js"], vec!["*.js"]),
            (vec![], vec![]),
        ];

        let shared = PreviewTreeCache::default();
        shared.begin(root);
        for (include, exclude) in edits {
            let source = source_at(root, &include, &exclude);
            let (cached_batches, cached_done) =
                run_with_cache(root, &source, &StreamConfig::default(), &shared);
            let (fresh_batches, fresh_done) = run(root, &source, &StreamConfig::default());

            let label = format!("include={include:?} exclude={exclude:?}");
            assert_eq!(
                verdicts(&cached_batches),
                verdicts(&fresh_batches),
                "cached classification diverged from a fresh walk for {label}"
            );
            assert_eq!(
                cached_done.included_count, fresh_done.included_count,
                "{label}"
            );
            assert_eq!(
                cached_done.excluded_count, fresh_done.excluded_count,
                "{label}"
            );
            assert_eq!(
                cached_done.included_bytes, fresh_done.included_bytes,
                "{label}"
            );
            assert!(!cached_done.truncated, "{label}");
            assert!(!cached_done.cancelled, "{label}");
            assert_parents_first(&all_nodes(&cached_batches));
        }
    }

    #[test]
    fn an_overflowed_cache_falls_back_to_walking_and_stays_correct() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.txt"), "x");
        write(&root.join("b.txt"), "yy");
        write(&root.join("docs/c.txt"), "zzz");
        write(&root.join("docs/d.log"), "w");

        let source = source_at(root, &[], &["*.log"]);
        let (expected, expected_done) = run(root, &source, &StreamConfig::default());

        // A budget of 2 cannot even hold the root's 3 entries.
        let tiny = PreviewTreeCache::with_max_entries(2);
        tiny.begin(root);
        let (batches, done) = run_with_cache(root, &source, &StreamConfig::default(), &tiny);

        assert!(tiny.overflowed(), "the budget was blown");
        assert_eq!(tiny.entry_count(), 0, "and the memory released");
        assert_eq!(
            verdicts(&batches),
            verdicts(&expected),
            "an overflowed cache is just the pre-cache walk, which is still exact"
        );
        assert_eq!(done.included_count, expected_done.included_count);
        assert_eq!(done.excluded_count, expected_done.excluded_count);
        assert_eq!(done.included_bytes, expected_done.included_bytes);

        // ...and it stays that way for the NEXT edit rather than half-caching.
        let (again, _) = run_with_cache(root, &source, &StreamConfig::default(), &tiny);
        assert_eq!(verdicts(&again), verdicts(&expected));
        assert_eq!(tiny.entry_count(), 0);
    }

    #[test]
    fn a_cache_built_under_one_matcher_serves_a_completely_different_one() {
        // A pass that saw the tree through a permissive matcher caches the same
        // directories a restrictive one needs; the restrictive pass must reach
        // the identical verdicts without re-reading anything.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        write(&root.join("a/b/c/leaf.txt"), "xx");
        write(&root.join("a/b/other.md"), "y");
        write(&root.join("top.txt"), "zzz");

        let cache = PreviewTreeCache::default();
        cache.begin(&root);
        run_with_cache(
            &root,
            &source_at(&root, &[], &[]),
            &StreamConfig::default(),
            &cache,
        );
        let cached_entries = cache.entry_count();

        fs::remove_dir_all(&root).expect("remove the fixture");

        let (batches, done) = run_with_cache(
            &root,
            &source_at(&root, &[], &["*.md", "/a/b/c/"]),
            &StreamConfig::default(),
            &cache,
        );
        assert_eq!(
            verdicts(&batches),
            vec![
                ("a".to_string(), true, true, 0),
                ("a/b".to_string(), true, true, 0),
                // `/a/b/c/` is excluded with no negation reaching inside, so it
                // is pruned - exactly as a disk walk would have pruned it.
                ("a/b/c".to_string(), true, false, 0),
                ("a/b/other.md".to_string(), false, false, 1),
                ("top.txt".to_string(), false, true, 3),
            ]
        );
        assert_eq!(done.included_count, 1, "only top.txt");
        assert_eq!(done.excluded_count, 1, "only a/b/other.md - c/ was pruned");
        assert_eq!(
            cache.entry_count(),
            cached_entries,
            "a pruning pass adds nothing to the cache and drops nothing from it"
        );
    }

    // ---------------------------------------------------------------------
    // Registry: generations, demotion, cancellation
    // ---------------------------------------------------------------------

    #[test]
    fn starting_a_preview_demotes_the_one_it_supersedes_to_a_cache_builder() {
        // The superseded pass is exactly the walk whose results the new pass
        // wants, so it keeps going - silently. Cancelling it would throw away
        // the disk work already in flight and make the new pass redo it.
        let registry = PreviewRegistry::default();
        let first = registry.start("gen-1");
        assert_eq!(registry.current_id().as_deref(), Some("gen-1"));
        assert!(first.emitting());
        assert!(!first.cancelled());

        let second = registry.start("gen-2");
        assert!(
            !first.cancelled(),
            "the superseded pass keeps walking to fill the cache"
        );
        assert!(
            !first.emitting(),
            "but its events stop reaching the webview"
        );
        assert!(second.emitting());
        assert_eq!(registry.current_id().as_deref(), Some("gen-2"));
        assert_eq!(registry.builder_id().as_deref(), Some("gen-1"));
    }

    #[test]
    fn a_third_preview_cancels_rather_than_stacking_a_second_builder() {
        // Typing in the textarea fires a preview per keystroke. Demoting every
        // one of them would leave N threads walking the same tree, so only the
        // OLDEST survivor (furthest into the tree, therefore worth the most)
        // keeps building; the rest are cancelled.
        let registry = PreviewRegistry::default();
        let first = registry.start("gen-1");
        let second = registry.start("gen-2");
        let third = registry.start("gen-3");

        assert!(!first.cancelled(), "the builder is untouched");
        assert!(second.cancelled(), "the middle pass is dropped outright");
        assert!(!third.cancelled());
        assert_eq!(registry.builder_id().as_deref(), Some("gen-1"));
        assert_eq!(registry.current_id().as_deref(), Some("gen-3"));

        // ...and it stays bounded at two however long the user types.
        let fourth = registry.start("gen-4");
        assert!(!first.cancelled());
        assert!(third.cancelled());
        assert!(!fourth.cancelled());
        assert_eq!(registry.builder_id().as_deref(), Some("gen-1"));
    }

    #[test]
    fn a_finished_builder_frees_the_slot_for_the_next_demotion() {
        let registry = PreviewRegistry::default();
        registry.start("gen-1");
        registry.start("gen-2");
        assert_eq!(registry.builder_id().as_deref(), Some("gen-1"));

        registry.finish("gen-1");
        assert_eq!(registry.builder_id(), None);
        assert_eq!(
            registry.current_id().as_deref(),
            Some("gen-2"),
            "finishing the builder must not disturb the live pass"
        );

        let second = registry.start("gen-3");
        assert_eq!(registry.builder_id().as_deref(), Some("gen-2"));
        assert!(second.emitting());
    }

    #[test]
    fn cancel_only_matches_the_live_generation_and_takes_the_builder_with_it() {
        let registry = PreviewRegistry::default();
        let builder = registry.start("gen-1");
        let live = registry.start("gen-2");

        assert!(
            !registry.cancel("gen-other"),
            "a stale id is a no-op, not an error"
        );
        assert!(!live.cancelled());
        assert!(!builder.cancelled());

        assert!(registry.cancel("gen-2"));
        assert!(live.cancelled());
        assert!(
            builder.cancelled(),
            "the editor closed, so the cache builder has nothing to build for"
        );
        assert_eq!(registry.current_id(), None);
        assert_eq!(registry.builder_id(), None);
        assert!(!registry.cancel("gen-2"), "cancelling twice is a no-op");
    }

    #[test]
    fn cancel_all_stops_both_slots() {
        let registry = PreviewRegistry::default();
        let builder = registry.start("gen-1");
        let live = registry.start("gen-2");

        registry.cancel_all();
        assert!(builder.cancelled());
        assert!(live.cancelled());
        assert_eq!(registry.current_id(), None);
        assert_eq!(registry.builder_id(), None);
    }

    #[test]
    fn a_superseded_pass_finishing_late_does_not_deregister_its_successor() {
        // The stale-generation hazard: gen-1's blocking pass ends AFTER gen-2
        // has already registered. It must clear only its own slot, or gen-2
        // would become uncancellable.
        let registry = PreviewRegistry::default();
        registry.start("gen-1");
        let second = registry.start("gen-2");

        registry.finish("gen-1");
        assert_eq!(
            registry.current_id().as_deref(),
            Some("gen-2"),
            "the live generation survives its predecessor's late finish"
        );
        assert!(registry.cancel("gen-2"));
        assert!(second.cancelled());
    }

    #[test]
    fn finish_clears_the_live_generation() {
        let registry = PreviewRegistry::default();
        registry.start("gen-1");
        registry.finish("gen-1");
        assert_eq!(registry.current_id(), None);
    }

    #[test]
    fn cancelling_the_live_preview_frees_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.txt"), "x");

        let registry = PreviewRegistry::default();
        let cache = PreviewTreeCache::default();
        cache.begin(root);
        let live = registry.start("gen-1");
        run_with_cache(
            root,
            &source_at(root, &[], &[]),
            &StreamConfig::default(),
            &cache,
        );
        assert!(cache.entry_count() > 0, "the pass populated the cache");

        assert!(
            !cancel_preview(&registry, &cache, "gen-other"),
            "a stale id changes nothing"
        );
        assert!(
            cache.entry_count() > 0,
            "and must not free a live editor's tree"
        );
        assert!(!live.cancelled());

        assert!(cancel_preview(&registry, &cache, "gen-1"));
        assert!(live.cancelled());
        assert_eq!(
            cache.entry_count(),
            0,
            "the editor closed, so the tree is released"
        );
    }
}
