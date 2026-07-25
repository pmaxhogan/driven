//! Streaming exclusion preview (SPEC s11.2; DESIGN s8.5 step 3).
//!
//! The one-shot [`crate::commands::sources::preview_exclusions`] walks the whole
//! source tree before it returns anything, so a large folder leaves the
//! exclusion editor showing "Loading..." for minutes with no sign of progress or
//! of what the rules are actually doing. This module adds the STREAMING flow the
//! editor uses instead:
//!
//! - `preview_exclusions_start(req)` validates the request EXACTLY like
//!   `preview_exclusions` (shared
//!   [`crate::commands::sources::resolve_preview_root_and_matcher`] - same glob
//!   validation, same exactly-one-selector rule, same dialog-token peek, same
//!   readable-dir check), then spawns the blocking walk and returns a fresh
//!   `preview_id` immediately;
//! - the walk emits `exclusion_preview:batch` events (every
//!   [`BATCH_MAX_NODES`] nodes or [`BATCH_MAX_INTERVAL`], whichever comes
//!   first), each carrying newly discovered nodes plus the running totals;
//! - it finishes with one `exclusion_preview:done` carrying the exact final
//!   totals.
//!
//! Every event is tagged with the `preview_id`, so the webview discards batches
//! from a superseded walk instead of mixing two trees.
//!
//! ## Generations and cancellation
//!
//! The editor re-previews on every rule edit, and a rule edit is exactly when a
//! minutes-long walk is most likely still running. [`PreviewRegistry`] keeps at
//! most ONE live walk per app: starting a preview flips the previous walk's
//! cancel flag, and `preview_exclusions_cancel(id)` (fired when the editor
//! closes) flips the current one. The walk polls the flag between directories
//! and between entries, so it stops within one directory's work rather than
//! burning CPU on a tree nobody is looking at.
//!
//! ## Breadth-first, parents before children
//!
//! The walk is BFS, which matters twice. The webview builds the tree
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
//! updating to the exact end of the walk - the summary line stays truthful even
//! when the tree is a partial view.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, State};

use driven_core::exclude::{DirDecision, SourceMatcher};

use crate::app_state::AppState;
use crate::commands::dtos::{
    ExclusionPreviewBatch, ExclusionPreviewDone, ExclusionPreviewNode, ExclusionPreviewRequest,
};
use crate::commands::sources::resolve_preview_root_and_matcher;
use crate::commands::CommandResult;
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

/// The single live streaming preview, if any.
struct ActivePreview {
    /// The generation id handed to the webview.
    id: String,
    /// Flipped to `true` to ask the walk to stop.
    cancel: Arc<AtomicBool>,
}

/// Tracks the ONE in-flight streaming preview so a new one can supersede it.
///
/// The exclusion editor re-previews on every rule change, so without this a user
/// tweaking globs over a large folder would stack N concurrent full-tree walks,
/// each pushing events the webview then has to discard. Starting a preview
/// cancels whatever was running; a walk that finishes on its own deregisters
/// itself, but ONLY if it is still the current generation (a walk that was
/// already superseded must not clear its successor's registration).
#[derive(Default)]
pub struct PreviewRegistry {
    active: Mutex<Option<ActivePreview>>,
}

impl PreviewRegistry {
    /// Register `id` as the live preview, CANCELLING whatever was live before,
    /// and return the new walk's cancel flag.
    pub fn start(&self, id: &str) -> Arc<AtomicBool> {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut guard = self.lock();
        if let Some(prev) = guard.take() {
            prev.cancel.store(true, Ordering::SeqCst);
            tracing::debug!(target: TARGET, superseded = %prev.id, by = %id, "cancelling the previous exclusion preview");
        }
        *guard = Some(ActivePreview {
            id: id.to_string(),
            cancel: Arc::clone(&cancel),
        });
        cancel
    }

    /// Cancel the live preview if it is `id`. Returns `true` when a matching
    /// preview was cancelled; a stale id is a no-op (the webview may cancel a
    /// generation the backend has already superseded or finished).
    pub fn cancel(&self, id: &str) -> bool {
        let mut guard = self.lock();
        match guard.as_ref() {
            Some(active) if active.id == id => {
                active.cancel.store(true, Ordering::SeqCst);
                *guard = None;
                true
            }
            _ => false,
        }
    }

    /// Deregister `id` after its walk ended - but only if it is STILL the live
    /// generation, so a superseded walk finishing late cannot clear the
    /// registration of the preview that replaced it.
    pub fn finish(&self, id: &str) {
        let mut guard = self.lock();
        if guard.as_ref().is_some_and(|a| a.id == id) {
            *guard = None;
        }
    }

    /// The live generation id, for tests / diagnostics.
    #[cfg(test)]
    fn current_id(&self) -> Option<String> {
        self.lock().as_ref().map(|a| a.id.clone())
    }

    /// Lock the slot, recovering a poisoned lock (house rule: never panic on a
    /// poisoned lock).
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<ActivePreview>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The knobs [`stream_classify_tree`] walks under. Production uses
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

/// Walk `root` breadth-first, classifying every entry with `matcher` and handing
/// each accumulated batch to `emit`. Returns the terminal totals.
///
/// Synchronous and blocking - the command runs it under `spawn_blocking`. It
/// takes the sink as a closure rather than an `AppHandle` so the whole walk
/// (batching, truncation, cancellation, ordering) is unit-testable with a `Vec`
/// sink and no Tauri runtime.
///
/// Mirrors the scanner's walk policy exactly, because the preview's whole job is
/// to predict it: symlinks are never followed (DESIGN s5.2.1 `Skip`), an
/// excluded directory is descended anyway when a `!`-re-include could reach
/// under THAT directory (`SourceMatcher::negations_could_match_under` - the same
/// per-directory P1-1 lockstep rule `build_walker` applies), and an unreadable
/// directory is logged and skipped rather than failing the preview.
pub fn stream_classify_tree(
    root: &Path,
    matcher: &SourceMatcher,
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
    //
    // Each queue entry carries its directory's resolved [`DirDecision`] cursor
    // alongside the path. Because the BFS already visits a directory before its
    // children, the parent state is always in hand, so every entry is classified
    // in O(scopes) instead of `is_included`'s O(depth x scopes) - no lookup
    // structure and no re-walking of parent components per entry. On a source
    // with a deep nested `.gitignore` cascade that is the bulk of the preview's
    // CPU. The cursor is only a faster spelling of `is_included`: it returns the
    // same verdict, and hands back to the slow path if ever mis-threaded.
    let mut queue: VecDeque<(PathBuf, DirDecision)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), matcher.root_decision()));

    let cancelled = 'walk: loop {
        let Some((dir, dir_state)) = queue.pop_front() else {
            break false;
        };
        if cancel.load(Ordering::SeqCst) {
            break true;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(err) => {
                // A permission denial / transient error on a subdir: log + skip
                // that subtree rather than failing the whole preview.
                tracing::debug!(target: TARGET, dir = %dir.display(), %err, "streaming preview: skipping unreadable directory");
                continue;
            }
        };
        for (seen, entry) in entries.flatten().enumerate() {
            if seen % CANCEL_POLL_ENTRIES == 0 && cancel.load(Ordering::SeqCst) {
                break 'walk true;
            }
            let path = entry.path();
            // Do not follow symlinks (scanner `Skip` policy): use the entry's
            // own type, not the dereferenced target.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let is_dir = file_type.is_dir();
            if !is_dir && !file_type.is_file() {
                continue;
            }

            // A directory's verdict and the cursor to walk into it come from one
            // call, since both need the same per-scope match work.
            let (included, child_state) = if is_dir {
                let (inc, child) = matcher.descend(&dir_state, rel);
                (inc, Some(child))
            } else {
                (matcher.is_included_at(&dir_state, rel, false), None)
            };
            let size = if is_dir {
                0
            } else {
                entry.metadata().map(|m| m.len()).unwrap_or(0)
            };
            // Materialise the wire form now: the queue below takes ownership of
            // `path`, which `rel` borrows from.
            let rel_str = rel.to_string_lossy().replace('\\', "/");

            if is_dir {
                // Descend unless this dir is excluded AND pruning it is safe -
                // i.e. no `!`-rule anywhere could re-include something beneath
                // THIS directory (the scanner's own per-directory rule).
                if included || matcher.negations_could_match_under(rel) {
                    if let Some(child) = child_state {
                        queue.push_back((path, child));
                    }
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
                // depends on concrete: a cancelled walk stops within one batch.
                if cancel.load(Ordering::SeqCst) {
                    break 'walk true;
                }
            }
        }
    };

    // Final partial batch (also the ONLY batch for a small tree). Skipped when
    // empty so a finished walk does not emit a redundant no-op event.
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
/// Validates identically to
/// [`crate::commands::sources::preview_exclusions`] (shared
/// [`resolve_preview_root_and_matcher`]), so an invalid glob or a request
/// without exactly one trusted root selector fails FAST with the same stable
/// SPEC s24 code - before any walk starts and before the webview clears its
/// tree. On success the walk runs on a blocking thread and reports through
/// `exclusion_preview:batch` / `exclusion_preview:done`; any preview still
/// running is cancelled first.
#[tauri::command]
pub async fn preview_exclusions_start(
    app: AppHandle,
    state: State<'_, AppState>,
    req: ExclusionPreviewRequest,
) -> CommandResult<String> {
    let (canon, matcher) = resolve_preview_root_and_matcher(&state, &req).await?;

    let registry = state.exclusion_previews();
    let preview_id = uuid::Uuid::new_v4().to_string();
    let cancel = registry.start(&preview_id);

    let id_for_walk = preview_id.clone();
    let walk = tokio::task::spawn_blocking(move || {
        let cfg = StreamConfig::default();
        let done = stream_classify_tree(&canon, &matcher, &cancel, &cfg, &id_for_walk, |batch| {
            if let Err(err) = events::emit_exclusion_preview_batch(&app, &batch) {
                tracing::warn!(target: TARGET, %err, "emit exclusion_preview:batch failed");
            }
        });
        registry.finish(&id_for_walk);
        if let Err(err) = events::emit_exclusion_preview_done(&app, &done) {
            tracing::warn!(target: TARGET, %err, "emit exclusion_preview:done failed");
        }
    });
    // The walk owns its own reporting; nothing awaits it. Dropping the handle
    // detaches the task rather than cancelling it (tokio semantics), which is
    // what we want - the cancel FLAG is the stop signal, not handle drop.
    drop(walk);

    Ok(preview_id)
}

/// `preview_exclusions_cancel(previewId)` - stop the streaming preview with this
/// generation id (SPEC s11.2).
///
/// Called when the exclusion editor closes (wizard cancelled / step left / the
/// inline panel collapsed) so a minutes-long walk over a huge source does not
/// keep a CPU busy for a view nobody is looking at. A start of a NEW preview
/// already cancels the previous one, so this is only needed for the
/// nothing-replaces-it case. Cancelling an unknown / already-finished generation
/// is a deliberate no-op, not an error: the webview cannot know whether the walk
/// beat it to the finish.
#[tauri::command]
pub async fn preview_exclusions_cancel(
    state: State<'_, AppState>,
    preview_id: String,
) -> CommandResult<()> {
    let cancelled = state.exclusion_previews().cancel(&preview_id);
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
    /// `resolve_preview_root_and_matcher` builds for a preview.
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

    /// Run a walk to completion, collecting every emitted batch.
    fn run(
        root: &Path,
        source: &SourceRow,
        cfg: &StreamConfig,
    ) -> (Vec<ExclusionPreviewBatch>, ExclusionPreviewDone) {
        let matcher = build_source_matcher(source).expect("matcher");
        let cancel = AtomicBool::new(false);
        let mut batches = Vec::new();
        let done = stream_classify_tree(root, &matcher, &cancel, cfg, "gen-1", |b| batches.push(b));
        (batches, done)
    }

    /// Every node from every batch, in emission order.
    fn all_nodes(batches: &[ExclusionPreviewBatch]) -> Vec<ExclusionPreviewNode> {
        batches.iter().flat_map(|b| b.nodes.clone()).collect()
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

        let mut paths: Vec<(String, bool, bool)> = nodes
            .iter()
            .map(|n| (n.path.clone(), n.is_dir, n.included))
            .collect();
        paths.sort();
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
    fn every_streamed_verdict_equals_the_matchers_own_answer() {
        // The lockstep guarantee for the DirDecision cursor: the preview now
        // classifies each entry from its parent directory's cached state instead
        // of calling `is_included` per path. That is only ever a faster spelling,
        // so EVERY streamed node must carry exactly the verdict `is_included`
        // would have given - checked here against a tree with a real nested
        // cascade (root rules, a deeper .gitignore overriding them, a directory
        // holding its own rules, an excluded subtree, and source-level
        // include/exclude overrides), which is where a per-scope inheritance bug
        // would show up.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        write(&root.join(".gitignore"), "*.log\nbuild/\n/anchored.txt\n");
        write(&root.join("anchored.txt"), "x");
        write(&root.join("sub/anchored.txt"), "x");
        write(&root.join("top.log"), "x");
        write(&root.join("keep.txt"), "x");
        write(&root.join("build/out/app.js"), "x");

        // A deeper .gitignore that re-includes what the root rule dropped.
        write(&root.join("a/.gitignore"), "!special.log\n*.dat\n");
        write(&root.join("a/special.log"), "x");
        write(&root.join("a/other.log"), "x");
        write(&root.join("a/thing.dat"), "x");
        write(&root.join("a/deep/special.log"), "x");
        write(&root.join("a/deep/plain.txt"), "x");

        // Source-level overrides on top of the whole cascade.
        write(&root.join("stale.bak"), "x");
        write(&root.join("secret/.env"), "x");
        let source = source_at(root, &["/secret/.env"], &["*.bak"]);

        let matcher = build_source_matcher(&source).expect("matcher");
        let (batches, done) = run(root, &source, &StreamConfig::default());
        let nodes = all_nodes(&batches);

        assert!(
            nodes.len() >= 15,
            "the fixture must actually stream a real tree, got {} nodes",
            nodes.len()
        );
        for node in &nodes {
            let expected = matcher.is_included(Path::new(&node.path), node.is_dir);
            assert_eq!(
                node.included, expected,
                "streamed verdict for {} (is_dir={}) disagreed with the matcher",
                node.path, node.is_dir
            );
        }

        // The counts must line up with the per-node verdicts too, so a cursor bug
        // cannot hide inside the totals.
        let included_files = nodes.iter().filter(|n| !n.is_dir && n.included).count() as u64;
        let excluded_files = nodes.iter().filter(|n| !n.is_dir && !n.included).count() as u64;
        assert_eq!(done.included_count, included_files);
        assert_eq!(done.excluded_count, excluded_files);
        assert!(!done.truncated, "the fixture is well under the node cap");
    }

    #[test]
    fn every_node_arrives_after_its_parent() {
        // The webview builds the tree incrementally, so a child whose parent has
        // not been streamed yet would have nowhere to attach. BFS guarantees the
        // ordering; this pins it against a deep + wide tree.
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
        let (batches, done) = run(root, &source, &cfg);
        let nodes = all_nodes(&batches);
        assert_eq!(done.included_count, 12);

        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for node in &nodes {
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
    fn cancellation_stops_the_walk_and_marks_the_done_event() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..50 {
            write(&root.join(format!("f{i}.txt")), "x");
        }

        let matcher = build_source_matcher(&source_at(root, &[], &[])).expect("matcher");
        let cancel = AtomicBool::new(false);
        let cfg = StreamConfig {
            batch_max_nodes: 2,
            ..StreamConfig::default()
        };
        let mut batches = Vec::new();
        // Cancel from inside the sink, the moment the first batch lands - the
        // real trigger is the same shape (a new preview / the editor closing
        // while the walk is mid-flight).
        let done = stream_classify_tree(root, &matcher, &cancel, &cfg, "gen-1", |b| {
            batches.push(b);
            cancel.store(true, Ordering::SeqCst);
        });

        assert!(done.cancelled, "the done event reports the cancellation");
        assert!(
            done.included_count < 50,
            "the walk stopped early, it did not run to completion ({} of 50)",
            done.included_count
        );
        assert!(
            !batches.is_empty(),
            "batches emitted before the cancel stand"
        );
    }

    #[test]
    fn a_pre_cancelled_walk_does_no_work() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("a.txt"), "x");

        let matcher = build_source_matcher(&source_at(root, &[], &[])).expect("matcher");
        let cancel = AtomicBool::new(true);
        let mut batches = Vec::new();
        let done = stream_classify_tree(
            root,
            &matcher,
            &cancel,
            &StreamConfig::default(),
            "gen-1",
            |b| batches.push(b),
        );

        assert!(done.cancelled);
        assert!(batches.is_empty(), "nothing streamed");
        assert_eq!(done.included_count, 0);
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

    #[test]
    fn starting_a_preview_cancels_the_one_it_supersedes() {
        let registry = PreviewRegistry::default();
        let first = registry.start("gen-1");
        assert_eq!(registry.current_id().as_deref(), Some("gen-1"));
        assert!(!first.load(Ordering::SeqCst));

        let second = registry.start("gen-2");
        assert!(
            first.load(Ordering::SeqCst),
            "the superseded walk is asked to stop"
        );
        assert!(!second.load(Ordering::SeqCst));
        assert_eq!(registry.current_id().as_deref(), Some("gen-2"));
    }

    #[test]
    fn cancel_only_matches_the_live_generation() {
        let registry = PreviewRegistry::default();
        let live = registry.start("gen-1");

        assert!(
            !registry.cancel("gen-other"),
            "a stale id is a no-op, not an error"
        );
        assert!(!live.load(Ordering::SeqCst));

        assert!(registry.cancel("gen-1"));
        assert!(live.load(Ordering::SeqCst));
        assert_eq!(registry.current_id(), None);
        assert!(!registry.cancel("gen-1"), "cancelling twice is a no-op");
    }

    #[test]
    fn a_superseded_walk_finishing_late_does_not_deregister_its_successor() {
        // The stale-generation hazard: gen-1's blocking walk notices the cancel
        // flag and calls finish() AFTER gen-2 has already registered. It must not
        // clear gen-2, or gen-2 would become uncancellable.
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
        assert!(second.load(Ordering::SeqCst));
    }

    #[test]
    fn finish_clears_the_live_generation() {
        let registry = PreviewRegistry::default();
        registry.start("gen-1");
        registry.finish("gen-1");
        assert_eq!(registry.current_id(), None);
    }
}
