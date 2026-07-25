//! The exclusion editor's in-memory folder-tree cache (SPEC s11.2).
//!
//! ## Why
//!
//! The streaming preview ([`crate::commands::exclusion_stream`]) used to couple
//! two jobs that have nothing to do with each other: WALKING the source folder
//! off disk, and CLASSIFYING each entry under the candidate include/exclude
//! rules. Every rule edit therefore paid for a fresh full-tree walk - minutes on
//! a large source, at a fraction of the disk's throughput, for a change that
//! cannot alter what is ON disk at all. Editing a glob felt like re-mounting the
//! drive.
//!
//! This cache decouples them. The FIRST walk of a root records what it found;
//! every later preview of the same root re-runs only the classification, from
//! memory, and touches the disk exclusively for subtrees the earlier walk never
//! descended (see "frontier" below). A rule edit then re-renders in
//! milliseconds instead of minutes.
//!
//! ## Shape
//!
//! One entry per DIRECTORY: `rel_dir -> [{ name, is_dir, size }]`, i.e. exactly
//! what one `read_dir` returned, in the order it returned it. A directory that
//! is absent from the map is one nobody has walked yet - either because the walk
//! has not reached it, or because the matcher of the walk that passed it PRUNED
//! it (an excluded directory no `!`-rule could reach into). Those two cases are
//! deliberately indistinguishable: a classification pass that wants a directory
//! the cache does not have simply walks it from disk and inserts it, so a
//! "frontier" directory a NEW rule now reaches into is filled in lazily and the
//! result is identical to a fresh full walk either way. Presence per directory
//! IS the completeness signal; no separate frontier set or global "complete"
//! flag is needed.
//!
//! ## Bounds and lifetime
//!
//! The cache is editor-scoped: [`PreviewTreeCache::begin`] drops it when the
//! previewed ROOT changes, and `preview_exclusions_cancel` (the editor closing)
//! clears it outright. Within a session it is bounded by
//! [`DEFAULT_MAX_ENTRIES`]; a source big enough to blow that budget marks the
//! cache OVERFLOWED, frees everything it held, and stops caching - every preview
//! of that root then walks from disk exactly as it did before this module
//! existed. Correctness is unaffected: the overflow path is the no-cache path.
//!
//! ## Staleness
//!
//! Sizes and directory contents are a snapshot taken when the entry was walked,
//! not a live view of the disk. That is the whole point - the editor is a
//! question about PATTERNS, and re-statting the tree to answer it is the cost
//! being removed. The snapshot lives only as long as the editor is open, and the
//! real backup always re-walks, so a file written while the user is editing
//! globs can affect the next preview session but never the backup itself.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// Tracing target for the preview tree cache.
const TARGET: &str = "driven::app::preview_cache";

/// Cache at most this many directory ENTRIES (files + subdirectories) before
/// giving up and falling back to walking from disk.
///
/// An entry is a name plus two scalars - roughly 40 bytes of struct plus the
/// name's own bytes, so call it 60-70 bytes each once the per-directory `Vec`s
/// are counted. 4M entries is therefore on the order of 250MB at the absolute
/// worst, held only while the exclusion editor is open and released the moment
/// it closes.
///
/// The ceiling is set from a measured tree rather than a guess: a real
/// `Documents` folder used to size this held ~3.45M files, and a budget that
/// stopped short of it would have dropped the cache for precisely the kind of
/// source that makes the preview slow enough to need one. Beyond the ceiling
/// nothing breaks - blowing the budget is not an error, it just returns the
/// preview to its pre-cache behaviour (verified on that same tree: the
/// classification came out identical, only slower).
pub const DEFAULT_MAX_ENTRIES: usize = 4_000_000;

/// One directory entry as the walk found it. Names are stored rather than full
/// paths because a directory's entries all share its prefix - storing the prefix
/// once per directory instead of once per entry is most of what keeps the budget
/// above realistic.
#[derive(Debug, Clone)]
pub struct CachedEntry {
    /// The entry's file name (one path component, OS-native).
    pub name: OsString,
    /// `true` for a directory. Symlinks are never recorded (the scanner's `Skip`
    /// policy), so this is always the entry's own type.
    pub is_dir: bool,
    /// File size in bytes at walk time; 0 for a directory.
    pub size: u64,
}

/// The directories cached for ONE root.
struct CacheState {
    /// The canonical source root these directories belong to.
    root: PathBuf,
    /// `rel_dir -> entries`, where `rel_dir` is relative to [`Self::root`] and
    /// the empty path is the root itself. `Arc` so a reader clones the pointer
    /// under the lock and classifies without holding it.
    dirs: HashMap<PathBuf, Arc<Vec<CachedEntry>>>,
    /// Total entries across [`Self::dirs`], tracked incrementally so the budget
    /// check is O(1).
    entries: usize,
    /// The budget was exceeded: everything was dropped and nothing more is
    /// cached until the next [`PreviewTreeCache::begin`] / [`PreviewTreeCache::clear`].
    overflowed: bool,
}

/// The app-wide, editor-scoped folder-tree cache (see the module docs).
pub struct PreviewTreeCache {
    state: Mutex<Option<CacheState>>,
    /// Entry budget; a test seam ([`PreviewTreeCache::with_max_entries`]) so the
    /// overflow path is exercised on a fixture of a dozen files rather than two
    /// million.
    max_entries: usize,
}

impl Default for PreviewTreeCache {
    fn default() -> Self {
        Self::with_max_entries(DEFAULT_MAX_ENTRIES)
    }
}

impl PreviewTreeCache {
    /// An empty cache with a custom entry budget (tests; production uses
    /// [`DEFAULT_MAX_ENTRIES`] via [`Default`]).
    pub fn with_max_entries(max_entries: usize) -> Self {
        Self {
            state: Mutex::new(None),
            max_entries,
        }
    }

    /// Point the cache at `root`, DROPPING everything it held when that is a
    /// different root than last time (including a root that had overflowed -
    /// a new folder gets a fresh budget).
    ///
    /// Returns `true` when contents were dropped, which the caller uses to also
    /// stop any walk still filling the cache for the old root: such a walk would
    /// otherwise keep inserting the old root's directories under the new root's
    /// key space. Inserts are additionally root-checked ([`Self::insert`]), so a
    /// walk that races past the cancel flag still cannot corrupt the new root.
    pub fn begin(&self, root: &Path) -> bool {
        let mut guard = self.lock();
        match guard.as_ref() {
            Some(state) if state.root == root => false,
            Some(_) => {
                *guard = Some(CacheState::new(root));
                tracing::debug!(target: TARGET, root = %root.display(), "preview tree cache: root changed, cache dropped");
                true
            }
            None => {
                *guard = Some(CacheState::new(root));
                false
            }
        }
    }

    /// The cached entries of `rel_dir` under `root`, or `None` when that
    /// directory has never been walked (or the cache has overflowed, or belongs
    /// to a different root - both of which make every lookup a miss, i.e. the
    /// pre-cache behaviour).
    pub fn get(&self, root: &Path, rel_dir: &Path) -> Option<Arc<Vec<CachedEntry>>> {
        let guard = self.lock();
        let state = guard.as_ref()?;
        if state.root != root || state.overflowed {
            return None;
        }
        state.dirs.get(rel_dir).map(Arc::clone)
    }

    /// Record `rel_dir`'s entries, unless doing so would exceed the budget - in
    /// which case the cache OVERFLOWS: it frees everything it holds and stops
    /// caching for this root (see the module docs).
    ///
    /// A no-op when the cache belongs to a different root, so a walk that
    /// outlives its root change cannot write into its successor's cache.
    pub fn insert(&self, root: &Path, rel_dir: &Path, entries: Arc<Vec<CachedEntry>>) {
        let mut guard = self.lock();
        let Some(state) = guard.as_mut() else {
            return;
        };
        if state.root != root || state.overflowed {
            return;
        }
        // Re-inserting a directory two walks raced on is idempotent - same
        // directory, same contents - but it must not double-count the budget.
        let previous = state.dirs.get(rel_dir).map_or(0, |e| e.len());
        let next = state.entries - previous + entries.len();
        if next > self.max_entries {
            tracing::info!(
                target: TARGET,
                max_entries = self.max_entries,
                root = %root.display(),
                "preview tree cache: entry budget exceeded, falling back to walking from disk"
            );
            state.dirs = HashMap::new();
            state.entries = 0;
            state.overflowed = true;
            return;
        }
        state.entries = next;
        state.dirs.insert(rel_dir.to_path_buf(), entries);
    }

    /// Drop everything (the editor closed). The next [`Self::begin`] starts a
    /// fresh cache with a fresh budget.
    pub fn clear(&self) {
        *self.lock() = None;
    }

    /// Entries currently cached - 0 when empty or overflowed. Diagnostics/tests.
    /// Named for what it counts rather than `len`, because "how much is in the
    /// cache" is a memory-budget question, not a collection length.
    pub fn entry_count(&self) -> usize {
        self.lock().as_ref().map_or(0, |s| s.entries)
    }

    /// The budget was exceeded for the current root (see the module docs).
    pub fn overflowed(&self) -> bool {
        self.lock().as_ref().is_some_and(|s| s.overflowed)
    }

    /// Lock the state, recovering a poisoned lock (house rule: never panic on a
    /// poisoned lock).
    fn lock(&self) -> MutexGuard<'_, Option<CacheState>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl CacheState {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            dirs: HashMap::new(),
            entries: 0,
            overflowed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(names: &[&str]) -> Arc<Vec<CachedEntry>> {
        Arc::new(
            names
                .iter()
                .map(|n| CachedEntry {
                    name: OsString::from(*n),
                    is_dir: false,
                    size: 1,
                })
                .collect(),
        )
    }

    fn root() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\src" } else { "/src" })
    }

    #[test]
    fn a_directory_round_trips_and_an_unknown_one_misses() {
        let cache = PreviewTreeCache::default();
        assert!(!cache.begin(&root()), "the first begin drops nothing");

        assert!(cache.get(&root(), Path::new("")).is_none());
        cache.insert(&root(), Path::new(""), entries(&["a.txt", "b.txt"]));

        let got = cache.get(&root(), Path::new("")).expect("cached");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, OsString::from("a.txt"));
        assert_eq!(cache.entry_count(), 2);
        assert!(cache.get(&root(), Path::new("docs")).is_none());
    }

    #[test]
    fn changing_the_root_drops_the_cache_and_locks_out_the_old_roots_walk() {
        let cache = PreviewTreeCache::default();
        cache.begin(&root());
        cache.insert(&root(), Path::new(""), entries(&["a.txt"]));

        let other = root().join("other");
        assert!(cache.begin(&other), "a new root drops the old contents");
        assert_eq!(cache.entry_count(), 0);
        assert!(cache.get(&root(), Path::new("")).is_none());

        // The old root's walk may not have noticed its cancel flag yet; its
        // inserts must not land in the new root's cache.
        cache.insert(&root(), Path::new("stale"), entries(&["x", "y"]));
        assert_eq!(cache.entry_count(), 0, "a stale root's insert is ignored");
        assert!(cache.get(&other, Path::new("stale")).is_none());

        // ...while the new root's own walk is cached normally.
        cache.insert(&other, Path::new("live"), entries(&["z"]));
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn re_inserting_the_same_directory_does_not_double_count_the_budget() {
        // Two generations racing on the same uncached directory both walk it and
        // both insert. The contents are identical; the accounting must be too,
        // or a few rule edits would "overflow" a cache that holds one directory.
        let cache = PreviewTreeCache::default();
        cache.begin(&root());
        cache.insert(&root(), Path::new("docs"), entries(&["a", "b", "c"]));
        assert_eq!(cache.entry_count(), 3);
        cache.insert(&root(), Path::new("docs"), entries(&["a", "b", "c"]));
        assert_eq!(
            cache.entry_count(),
            3,
            "the re-insert replaces, it does not add"
        );
    }

    #[test]
    fn exceeding_the_budget_frees_everything_and_stops_caching() {
        let cache = PreviewTreeCache::with_max_entries(4);
        cache.begin(&root());
        cache.insert(&root(), Path::new("a"), entries(&["1", "2"]));
        cache.insert(&root(), Path::new("b"), entries(&["3", "4"]));
        assert_eq!(cache.entry_count(), 4, "exactly at the budget still fits");
        assert!(!cache.overflowed());

        cache.insert(&root(), Path::new("c"), entries(&["5"]));
        assert!(cache.overflowed(), "one entry over the budget overflows");
        assert_eq!(
            cache.entry_count(),
            0,
            "and the memory is actually released"
        );
        // Every lookup now misses, so callers walk from disk - the pre-cache
        // behaviour, which is always correct, just slower.
        assert!(cache.get(&root(), Path::new("a")).is_none());
        cache.insert(&root(), Path::new("d"), entries(&["6"]));
        assert_eq!(cache.entry_count(), 0, "an overflowed cache stays empty");

        // A fresh root gets a fresh budget.
        let other = root().join("other");
        cache.begin(&other);
        assert!(!cache.overflowed());
        cache.insert(&other, Path::new("a"), entries(&["1"]));
        assert_eq!(cache.entry_count(), 1);
    }

    #[test]
    fn clear_drops_everything_including_the_overflow_flag() {
        let cache = PreviewTreeCache::with_max_entries(1);
        cache.begin(&root());
        cache.insert(&root(), Path::new("a"), entries(&["1", "2"]));
        assert!(cache.overflowed());

        cache.clear();
        assert_eq!(cache.entry_count(), 0);
        assert!(!cache.overflowed(), "a cleared cache has no root at all");
        // ...and an insert with no `begin` is simply dropped.
        cache.insert(&root(), Path::new("a"), entries(&["1"]));
        assert_eq!(cache.entry_count(), 0);
    }
}
