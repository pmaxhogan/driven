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
//! Blocking-I/O caveat (deferred past phase 2A): the walk and the
//! deep-verify hashing are synchronous, blocking calls run inline on the
//! async task. Ideally they would move to `spawn_blocking`, but the
//! `&dyn StateRepo` / `&SourceRow` borrows make that awkward at this phase;
//! left inline and noted for a later pass.

use std::collections::HashSet;
use std::fs::Metadata;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Context;

use crate::exclude::{build_source_matcher, build_walker};
use crate::state::{SourceRow, StateRepo};
use crate::types::{LocalEntry, RelativePath, ScanMode, ScanResult};

static TARGET: &str = "driven::core::scanner";

/// `FILE_ATTRIBUTE_RECALL_ON_OPEN` (DESIGN s5.2.1): set by OneDrive Files-On-
/// Demand (and similar cloud providers) on a placeholder whose bytes are not
/// resident on disk. Reading such a file forces a network hydration, so the
/// scanner skips these by default rather than pulling down TBs of cloud-only
/// data. Matches the Win32 SDK constant `0x00400000`.
#[cfg(windows)]
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0040_0000;

/// Walks one source and returns the new-or-changed / deleted diff (SPEC s6).
///
/// `mode` selects the change-detection predicate (see the module docs).
/// Pure aside from local filesystem reads and the `state` load; emits no
/// ops and mutates no state - the planner (SPEC s7) and executor (SPEC s8)
/// own those side effects.
pub async fn scan(
    source: &SourceRow,
    state: &dyn StateRepo,
    mode: ScanMode,
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

    // The include/exclude decision matcher (DESIGN s5.2). The walker itself
    // does NO ignore logic now; we ask the matcher per entry here AND reuse it
    // below to split not-seen known paths into genuine deletions vs
    // excluded-orphans (DESIGN s5.5).
    let matcher = build_source_matcher(source)?;

    let walker = build_walker(source)?.build();

    let mut seen: HashSet<RelativePath> = HashSet::new();
    let mut new_or_changed: Vec<LocalEntry> = Vec::new();
    // NFC collision keys: a second distinct raw path that normalises onto an
    // already-seen `RelativePath` (DESIGN s5.2.3, SPEC s24
    // `local.unicode_collision`). We keep the first file, drop the later one,
    // and record the key here instead of emitting a duplicate op.
    let mut collisions: Vec<RelativePath> = Vec::new();
    // Count of cloud-only (OneDrive Files-On-Demand) placeholders skipped
    // before stat/hash (DESIGN s5.2.1). Mutated only on Windows; the
    // attribute (and thus the increment) never exists on other targets.
    #[cfg_attr(not(windows), allow(unused_mut))]
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

    for result in walker {
        let entry = match result {
            Ok(e) => e,
            Err(err) => {
                // DESIGN s5.2 step 3: never swallow walker errors. Record
                // the errored path's relative prefix (when one is
                // recoverable) so deletions under it are suppressed; if no
                // attributable prefix, suppress ALL deletions this cycle.
                match error_path(&err)
                    .and_then(|p| p.strip_prefix(root).ok())
                    .and_then(|p| RelativePath::try_from(p).ok())
                {
                    Some(rel) => errored_prefixes.push(rel),
                    None => unattributed_error = true,
                }
                tracing::warn!(target: TARGET, source_id = %source.id, %err, "walk error; suppressing deletes under this subtree");
                continue;
            }
        };

        // Symlink check BEFORE the is_file filter: a link to a file can read
        // as a file on some platforms, and we must skip + count it either
        // way (DESIGN s5.2.1 SymlinkPolicy::Skip).
        if entry.file_type().is_some_and(|t| t.is_symlink()) {
            skipped_symlinks += 1;
            tracing::trace!(target: TARGET, source_id = %source.id, path = %entry.path().display(), "skipping symlink");
            continue;
        }
        // Files only; directories and other non-files are not backed up.
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }

        let abs = entry.path();
        let rel = match abs.strip_prefix(root).ok().and_then(|p| {
            // RelativePath::try_from enforces the NFC / no-`..` invariants.
            RelativePath::try_from(p).ok()
        }) {
            Some(r) => r,
            None => {
                tracing::warn!(target: TARGET, source_id = %source.id, path = %abs.display(), "skipping path not representable as a relative path");
                continue;
            }
        };

        // Include/exclude decision (DESIGN s5.2): consult the same matcher the
        // orphan split below uses, on the NFC `RelativePath` so both agree. We
        // are past the is_file filter, so `is_dir = false`. An excluded file is
        // simply not backed up - and, crucially, never lands in `seen`, so if a
        // stored `file_state` row exists for it the orphan split classifies it
        // as an excluded-orphan (no trash), not a deletion.
        // TODO(perf): prune excluded directories via WalkBuilder::filter_entry
        //   so we do not descend e.g. node_modules just to discard each file.
        if !matcher.is_included(Path::new(rel.as_str()), false) {
            tracing::trace!(target: TARGET, source_id = %source.id, path = %rel, "excluded by ignore rules");
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                // A stat failure on a single file is a per-file error, not a
                // subtree walk error; suppress this path from deletion (its
                // row, if any, should not be trashed) and move on.
                errored_prefixes.push(rel.clone());
                tracing::warn!(target: TARGET, source_id = %source.id, path = %abs.display(), %err, "metadata read failed; skipping file");
                continue;
            }
        };

        // OneDrive Files-On-Demand skip (DESIGN s5.2.1): a cloud-only
        // placeholder has FILE_ATTRIBUTE_RECALL_ON_OPEN set; opening it to
        // stat/hash would force a network hydration. Skip it, but FIRST mark
        // it `seen` so the deletion sweep below does NOT treat its existing
        // `file_state` row as "known but missing" and trash the Drive backup
        // - a placeholder is still present locally, just dehydrated.
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & FILE_ATTRIBUTE_RECALL_ON_OPEN != 0 {
                // TODO(M6): per-source "include cloud-only OneDrive files" toggle (default skip)
                seen.insert(rel.clone());
                skipped_cloud_only += 1;
                tracing::trace!(target: TARGET, source_id = %source.id, path = %rel, "skipping cloud-only (OneDrive Files-On-Demand) placeholder");
                continue;
            }
        }

        let size = meta.len();
        let mtime_ns = mtime_ns(&meta);
        // NFC collision detection (DESIGN s5.2.3, SPEC s24
        // local.unicode_collision): `RelativePath::try_from` NFC-normalised
        // `rel`, so two byte-distinct raw paths can collapse to one key.
        // `HashSet::insert` returns false when the key was already present;
        // keep the first file, record + drop the later collider rather than
        // emitting a duplicate upload op for the same `file_state` key.
        if !seen.insert(rel.clone()) {
            tracing::warn!(target: TARGET, source_id = %source.id, path = %rel, "local.unicode_collision: distinct raw paths normalise to one NFC key; dropping later duplicate");
            collisions.push(rel.clone());
            continue;
        }

        let stored = known.get(&rel);
        // TODO(M3): FS-granularity probe + ctime/birthtime fallback + hash-within-last-scan-window per DESIGN s5.2 step 2
        let stat_match =
            matches!(stored, Some(row) if row.size == size && row.mtime_ns == mtime_ns);

        if stat_match {
            // FastPath: stat match => unchanged. DeepVerify: re-hash and
            // treat a hash mismatch against the stored blake3 as changed.
            if mode == ScanMode::DeepVerify {
                let stored_hash = stored.map(|r| r.hash_blake3);
                match hash_file(abs) {
                    Ok(hash) => {
                        if stored_hash != Some(hash) {
                            tracing::info!(target: TARGET, source_id = %source.id, path = %rel, "deep-verify hash mismatch; marking changed");
                            new_or_changed.push(LocalEntry {
                                rel,
                                size,
                                mtime_ns,
                            });
                        }
                    }
                    Err(err) => {
                        // Could not read to verify: don't claim changed (no
                        // evidence) but suppress this path from deletion.
                        errored_prefixes.push(rel.clone());
                        tracing::warn!(target: TARGET, source_id = %source.id, path = %rel, %err, "deep-verify read failed; skipping file");
                    }
                }
            }
            // FastPath stat-match: unchanged, nothing to emit.
        } else {
            // New (no row) or changed (stat differs).
            new_or_changed.push(LocalEntry {
                rel,
                size,
                mtime_ns,
            });
        }
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
    let mut deleted: Vec<RelativePath> = Vec::new();
    let mut excluded_orphans: Vec<RelativePath> = Vec::new();
    for path in known.keys().filter(|p| !seen.contains(*p)) {
        if !matcher.is_included(Path::new(path.as_str()), false) {
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

    tracing::debug!(
        target: TARGET,
        source_id = %source.id,
        ?mode,
        new_or_changed = new_or_changed.len(),
        deleted = deleted.len(),
        excluded_orphans = excluded_orphans.len(),
        skipped_symlinks,
        skipped_cloud_only,
        collisions = collisions.len(),
        errored_prefixes = errored_prefixes.len(),
        unattributed_error,
        "scan complete"
    );

    Ok(ScanResult {
        new_or_changed,
        deleted,
        collisions,
        excluded_orphans,
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
fn hash_file(path: &Path) -> anyhow::Result<[u8; 32]> {
    // TODO(M3): use FILE_SHARE_DELETE platform-open helper per DESIGN s5.3 (executor open path)
    let mut file = std::fs::File::open(path)
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
            drive_folder_path: "/f".into(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
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
        async fn get_setting(&self, _key: &str) -> anyhow::Result<Option<serde_json::Value>> {
            unimplemented!()
        }
        async fn set_setting(&self, _key: &str, _value: &serde_json::Value) -> anyhow::Result<()> {
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
    async fn nested_negation_under_excluded_dir_not_false_deleted() {
        // P1-1 (data-loss guard): a nested negation (`vendor/.gitignore:
        // !keep.txt`) re-includes `vendor/keep.txt` even though a parent rule
        // excludes `vendor/`. The flattened matcher classifies it INCLUDED, so
        // if dir-pruning skipped `vendor/` the file would never be walked, land
        // un-`seen`, and the orphan split would false-classify its stored
        // file_state row as `deleted` - trashing a file that still exists.
        // With the fix, build_walker disables pruning whenever the matcher has
        // negations, so `vendor/` IS walked, `keep.txt` is seen, and the row is
        // neither deleted nor an excluded_orphan.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), b"vendor/\n");
        // Nested negation re-including a file inside the excluded dir. A
        // no-slash `!keep.txt` matches the path itself before the excluded
        // parent is consulted (the exact P1-1 mechanism).
        write(&root.join("vendor/.gitignore"), b"!keep.txt\n");
        let keep = root.join("vendor/keep.txt");
        write(&keep, b"x");
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
            "a nested-negation re-included file under an excluded dir must NOT be deleted: {:?}",
            res.deleted
        );
        assert!(
            !res.excluded_orphans.contains(&rel("vendor/keep.txt")),
            "the re-included file is INCLUDED, so it is not an excluded_orphan either: {:?}",
            res.excluded_orphans
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
    #[cfg(windows)]
    #[test]
    fn recall_on_open_bit_is_correct() {
        assert_eq!(super::FILE_ATTRIBUTE_RECALL_ON_OPEN, 0x0040_0000);
    }
}
