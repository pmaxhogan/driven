//! diff -> ops; SPEC s7.
//!
//! The planner turns a [`ScanResult`](crate::types::ScanResult) (the diff a
//! single scan of one source produced, SPEC s6) into a [`Plan`] of
//! [`Op`]s the executor (SPEC s8) runs. It is the second stage of the
//! per-source pipeline the orchestrator drives:
//! `scan -> plan -> execute -> verify` (SPEC s5).
//!
//! The planner performs no I/O of its own beyond the two `file_state`
//! lookups every deleted path needs; it is otherwise a pure fold over the
//! scan result. All persistence flows through the injected
//! [`StateRepo`] trait (SPEC s2) so this module is exercisable against an
//! in-memory fake without SQLite.
//!
//! ## Rename detection: not done in V1 (ROADMAP M2)
//!
//! V1 does NOT detect renames. A rename on disk surfaces in the scan as a
//! delete of the old relative path plus an add of the new one, so the
//! planner emits exactly **one upload** (for the new path) and **one
//! trash** (for the old path's remote object) - it never recognises that
//! the two paths share content and reuses the existing Drive object. This
//! is the documented V1 behaviour (ROADMAP M2 "Rename -> currently
//! produces one upload + one trash; we don't detect renames in V1").
//! Content-addressed rename detection is deferred to a later milestone.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::state::SourceRow;
use crate::state::StateRepo;
use crate::types::{BundleMemberPlan, LocalEntry, Op, Plan, RelativePath, ScanResult};

/// Module-level tracing target (SPEC s0 logging convention).
const TARGET: &str = "driven::core::planner";

// -----------------------------------------------------------------------------
// V2 small-file bundling (issue #35): planner grouping configuration.
// -----------------------------------------------------------------------------

/// Default per-member eligibility ceiling: only files at or below this size are
/// candidates for bundling. Larger files upload individually (a big file gets no
/// round-trip benefit from bundling and would blow the archive's memory bound).
pub const BUNDLE_MAX_FILE_SIZE: u64 = 256 * 1024;

/// Default minimum number of eligible small files in one directory before that
/// directory is bundled at all. Below this, the round-trip saving is not worth a
/// bundle, so the files upload individually. Set high (100) so bundling only
/// kicks in for genuinely dense cold folders (e.g. a `node_modules`-style tree),
/// where the round-trip saving dominates; sparser directories keep individual
/// per-file objects (simpler restore, cheaper re-bundle churn).
pub const BUNDLE_MIN_FILES: usize = 100;

/// Default minimum age (in days) a file's mtime must have before it is
/// bundle-eligible. Fresh files are churn-prone - a file edited yesterday is
/// likely to be edited again, and re-bundling on every edit is pure overhead -
/// so only files untouched for at least this long are packed. A file whose mtime
/// is unknown (scanner sentinel `0`) is treated as fresh (never bundled).
pub const BUNDLE_MIN_COLD_AGE_DAYS: u32 = 30;

/// Default maximum members packed into a single bundle object.
pub const BUNDLE_MAX_FILES: usize = 512;

/// Default maximum UNCOMPRESSED bytes packed into a single bundle. Kept below the
/// executor's `RESUMABLE_THRESHOLD` (5 MiB) and Drive's simple-upload limit so a
/// bundle is always ONE non-resumable simple create, and small enough that the
/// whole archive fits comfortably in memory during build + restore.
pub const BUNDLE_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Tunables controlling small-file bundling (issue #35). Disabled by default so
/// bundling is strictly opt-in (the frozen v1.0.0 behaviour is unchanged unless a
/// user turns it on); the orchestrator builds an enabled config from the
/// `bundle_small_files` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundleConfig {
    /// When false the planner behaves exactly as v1.0.0 (every new/changed file
    /// is an individual `HashThenUpload`).
    pub enabled: bool,
    /// Per-member size ceiling ([`BUNDLE_MAX_FILE_SIZE`]).
    pub max_file_size: u64,
    /// Minimum eligible files per directory to bundle ([`BUNDLE_MIN_FILES`]).
    pub min_files: usize,
    /// Maximum members per bundle ([`BUNDLE_MAX_FILES`]).
    pub max_files: usize,
    /// Maximum uncompressed bytes per bundle ([`BUNDLE_MAX_BYTES`]).
    pub max_bytes: u64,
    /// Minimum file age in days before a file is bundle-eligible
    /// ([`BUNDLE_MIN_COLD_AGE_DAYS`]). `0` disables the coldness gate (any file
    /// with a known mtime is eligible).
    pub min_cold_age_days: u32,
}

impl Default for BundleConfig {
    /// Disabled - the v1.0.0 behaviour.
    fn default() -> Self {
        Self {
            enabled: false,
            max_file_size: BUNDLE_MAX_FILE_SIZE,
            min_files: BUNDLE_MIN_FILES,
            max_files: BUNDLE_MAX_FILES,
            max_bytes: BUNDLE_MAX_BYTES,
            min_cold_age_days: BUNDLE_MIN_COLD_AGE_DAYS,
        }
    }
}

impl BundleConfig {
    /// An enabled config with the default thresholds.
    pub fn enabled_defaults() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }
}

/// Settings KV key (SPEC s22) for the user-facing on/off bundling toggle (issue
/// #35 item d). Surfaced in the settings UI; read by [`load_bundle_config`].
pub const SETTING_BUNDLE_ENABLED: &str = "bundle_small_files";

/// Backend-only settings KV keys for the bundling thresholds (issue #35 item c).
/// Not exposed in the UI; each is read with a fail-closed fallback to its
/// compile-time default so a missing or malformed value never breaks planning.
const SETTING_BUNDLE_MAX_FILE_SIZE: &str = "bundle_max_file_size";
const SETTING_BUNDLE_MIN_FILES: &str = "bundle_min_files";
const SETTING_BUNDLE_MAX_FILES: &str = "bundle_max_files";
const SETTING_BUNDLE_MAX_BYTES: &str = "bundle_max_bytes";
const SETTING_BUNDLE_MIN_COLD_AGE_DAYS: &str = "bundle_min_cold_age_days";

/// The plaintext-bytes ceiling a configured `max_bytes` is clamped to. Kept a
/// 512 KiB margin below the executor's [`crate::executor::RESUMABLE_THRESHOLD`]
/// (5 MiB) so the UPLOADED object - the gzip of the tar, then for an encrypted
/// source the ciphertext plus its per-chunk framing - always stays inside the
/// single simple-create band and never needs a resumable session. A bundle that
/// crossed the threshold would demand the resumable path the executor
/// deliberately never takes for bundles. Also the executor's hard accumulated-
/// plaintext ceiling passed to `build_bundle` (issue #35), so a member that grew
/// after the scan can never push the object past the simple-create band.
pub const BUNDLE_MAX_BYTES_CEILING: u64 = crate::executor::RESUMABLE_THRESHOLD - 512 * 1024;

/// Read one `u64`-valued setting, falling back to `default` when the key is
/// absent, unreadable, or not a JSON number (fail-closed, issue #35 item c).
async fn read_u64_setting(state: &dyn StateRepo, key: &str, default: u64) -> u64 {
    state
        .get_setting(key)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_u64())
        .unwrap_or(default)
}

/// Build a [`BundleConfig`] from the settings KV (issue #35 item c).
///
/// [`SETTING_BUNDLE_ENABLED`] (bool) gates the whole feature: when it is absent,
/// `false`, or unreadable the returned config is DISABLED (byte-for-byte the
/// v1.0.0 behaviour). When enabled, each threshold is read from its backend-only
/// KV key and fails closed to the compile-time default on a missing/malformed
/// value. `max_bytes` is additionally CLAMPED to [`BUNDLE_MAX_BYTES_CEILING`] so
/// a bundle always remains a single simple create.
pub async fn load_bundle_config(state: &dyn StateRepo) -> BundleConfig {
    let enabled = state
        .get_setting(SETTING_BUNDLE_ENABLED)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !enabled {
        return BundleConfig::default();
    }
    let d = BundleConfig::enabled_defaults();
    let max_bytes = read_u64_setting(state, SETTING_BUNDLE_MAX_BYTES, d.max_bytes)
        .await
        .min(BUNDLE_MAX_BYTES_CEILING);
    // Narrowing casts fail CLOSED to the compile-time default when a stored value
    // is out of the target type's range (issue #35 finding 5): a raw `as u32`
    // WRAPS (e.g. 2^32 -> 0, which would silently disable the coldness gate), so
    // use a checked conversion. `usize` is narrower than `u64` only on 32-bit
    // hosts, but a checked conversion is correct there too.
    let min_files = usize::try_from(
        read_u64_setting(state, SETTING_BUNDLE_MIN_FILES, d.min_files as u64).await,
    )
    .unwrap_or(d.min_files);
    let max_files = usize::try_from(
        read_u64_setting(state, SETTING_BUNDLE_MAX_FILES, d.max_files as u64).await,
    )
    .unwrap_or(d.max_files);
    let min_cold_age_days = u32::try_from(
        read_u64_setting(
            state,
            SETTING_BUNDLE_MIN_COLD_AGE_DAYS,
            u64::from(d.min_cold_age_days),
        )
        .await,
    )
    .unwrap_or(d.min_cold_age_days);
    BundleConfig {
        enabled: true,
        max_file_size: read_u64_setting(state, SETTING_BUNDLE_MAX_FILE_SIZE, d.max_file_size).await,
        min_files,
        max_files,
        max_bytes,
        min_cold_age_days,
    }
}

/// Turn one source's [`ScanResult`] into a [`Plan`] (SPEC s7).
///
/// For each entry in `scan.new_or_changed` emit one
/// [`Op::HashThenUpload`] carrying the pre-open size captured by the
/// scanner. For each path in `scan.deleted` consult the stored
/// `file_state` row:
///
/// - If the row has a `drive_file_id`, the file reached Drive, so emit one
///   [`Op::Trash`] targeting that remote object. The executor trashes
///   rather than hard-deletes so the user can recover from a mistaken
///   delete via the Drive web UI (SPEC s7).
/// - If the row has no `drive_file_id`, the file never made it to Drive,
///   so there is nothing to trash: delete the `file_state` row directly
///   and emit no op (SPEC s7).
///
/// A deleted path always traces back to a `file_state` row (the scanner
/// derives `deleted` from the loaded `file_state` keys, SPEC s6), so a
/// missing row is unreachable in practice. House rules forbid `expect()`
/// in non-test code, so rather than panic on the `None` the SPEC s7
/// pseudocode glosses with `.expect("must exist")`, this treats it as a
/// graceful no-op (nothing on disk, nothing on Drive) and logs a warning -
/// "anything ambiguous is a bug" (SPEC s0).
///
/// Ops are emitted uploads-first then trashes, in scan-iteration order.
/// The executor is free to reorder for concurrency but must preserve
/// happens-before semantics per `(source_id, relative_path)` (see
/// [`Plan::ops`]).
///
/// The scanner's NFC collisions are copied verbatim into [`Plan::collisions`]
/// (no op emitted); the M3 orchestrator surfaces them as
/// `local.unicode_collision` activity errors and owns the fail-closed policy.
///
/// See the module docs for why renames yield one upload + one trash in V1.
pub async fn plan(
    source: &SourceRow,
    scan: &ScanResult,
    state: &dyn StateRepo,
    now: crate::types::UnixMs,
    bundle: &BundleConfig,
) -> Result<Plan> {
    let mut ops = Vec::with_capacity(scan.new_or_changed.len() + scan.deleted.len());

    if bundle.enabled {
        // V2 small-file bundling (issue #35). Only GENUINELY-NEW files (no
        // existing `file_state` row) are bundle-eligible: a changed or
        // previously-uploaded file always stays an individual `HashThenUpload`, so
        // bundling never strands an old standalone Drive object and a bundled
        // member that later changes simply re-uploads standalone (clearing its
        // membership). Load the source's `file_state` keys once to tell "new" from
        // "changed"; the scanner already loaded this map, so this is one extra
        // bulk indexed read, incurred only when bundling is enabled.
        let existing = state.load_source_file_state(source.id).await?;
        let (bundles, individual) = group_bundles(&scan.new_or_changed, &existing, now, bundle);
        for entry in individual {
            ops.push(Op::HashThenUpload {
                source_id: source.id,
                relative_path: entry.rel.clone(),
                size: entry.size,
            });
        }
        for members in bundles {
            ops.push(Op::UploadBundle {
                source_id: source.id,
                members,
            });
        }
    } else {
        for entry in &scan.new_or_changed {
            ops.push(Op::HashThenUpload {
                source_id: source.id,
                relative_path: entry.rel.clone(),
                size: entry.size,
            });
        }
    }

    for path in &scan.deleted {
        match state.get_file_state(source.id, path).await? {
            Some(row) => match row.drive_file_id {
                Some(file_id) => ops.push(Op::Trash {
                    source_id: source.id,
                    relative_path: path.clone(),
                    drive_file_id: file_id,
                }),
                // Never reached Drive: drop the row, emit no op (SPEC s7).
                None => state.delete_file_state(source.id, path).await?,
            },
            // Unreachable in practice (see fn docs); treat as a no-op.
            None => {
                tracing::warn!(
                    target: TARGET,
                    source_id = %source.id,
                    relative_path = %path,
                    "deleted path had no file_state row; skipping (SPEC s7 invariant violated)"
                );
            }
        }
    }

    // P1-3: thread the scanner's NFC collisions through to the Plan untouched.
    // The M3 orchestrator surfaces these as `local.unicode_collision` activity
    // errors and decides fail-closed (block the source) vs skip-the-colliding-
    // file-with-an-error policy; the planner itself emits no op for them.
    Ok(Plan {
        ops,
        collisions: scan.collisions.clone(),
    })
}

/// Whether a file is "cold" enough to bundle: its mtime is at least
/// `min_cold_age_days` older than `now` (issue #35 coldness gate, item b).
///
/// `mtime_ns` is signed nanoseconds since the Unix epoch (the scanner's
/// convention); `now` is Unix-epoch MILLISECONDS. A file whose mtime is the
/// scanner's `0` sentinel (the platform could not report an mtime) is treated as
/// NOT cold - we cannot prove it is old, and bundling a possibly-churning file is
/// exactly what this gate exists to avoid. `min_cold_age_days == 0` disables the
/// gate for every file with a KNOWN (non-zero) mtime. Arithmetic is done in
/// `i128` so no realistic timestamp overflows.
fn is_cold(mtime_ns: i64, now: crate::types::UnixMs, min_cold_age_days: u32) -> bool {
    if mtime_ns == 0 {
        return false;
    }
    let now_ns = i128::from(now) * 1_000_000;
    let min_age_ns = i128::from(min_cold_age_days) * 86_400 * 1_000_000_000;
    let cutoff_ns = now_ns - min_age_ns;
    i128::from(mtime_ns) <= cutoff_ns
}

/// The parent directory of a relative path, as the bundle grouping key. A
/// root-level file (`"a.txt"`) groups under the empty string; `"a/b/c.txt"`
/// groups under `"a/b"`. Paths are already `/`-separated and NFC-canonical.
fn parent_dir(rel: &RelativePath) -> &str {
    match rel.as_str().rfind('/') {
        Some(i) => &rel.as_str()[..i],
        None => "",
    }
}

/// Partition the scan's new/changed files into bundle groups plus the leftover
/// individual uploads (V2 small-file bundling, issue #35).
///
/// A file is bundle-eligible only if it is GENUINELY NEW (no `existing`
/// `file_state` row), at or below `cfg.max_file_size`, AND COLD - its mtime is at
/// least `cfg.min_cold_age_days` old relative to `now` (see [`is_cold`]).
/// Eligible files are grouped by parent directory; a directory with fewer than
/// `cfg.min_files` eligible files is not bundled (its files upload individually).
/// Within a bundled directory, files are packed in deterministic path order into
/// bundles capped at `cfg.max_files` members and `cfg.max_bytes` bytes; a
/// leftover group of a single file falls back to an individual upload (a 1-file
/// "bundle" is pure overhead). Everything not bundled is returned as an
/// individual entry, so no file is ever dropped.
fn group_bundles<'a>(
    entries: &'a [LocalEntry],
    existing: &std::collections::HashMap<RelativePath, crate::state::FileStateRow>,
    now: crate::types::UnixMs,
    cfg: &BundleConfig,
) -> (Vec<Vec<BundleMemberPlan>>, Vec<&'a LocalEntry>) {
    let mut by_dir: BTreeMap<&str, Vec<&'a LocalEntry>> = BTreeMap::new();
    let mut individual: Vec<&'a LocalEntry> = Vec::new();

    for e in entries {
        let eligible = e.size <= cfg.max_file_size
            && !existing.contains_key(&e.rel)
            && is_cold(e.mtime_ns, now, cfg.min_cold_age_days);
        if eligible {
            by_dir.entry(parent_dir(&e.rel)).or_default().push(e);
        } else {
            individual.push(e);
        }
    }

    let mut bundles: Vec<Vec<BundleMemberPlan>> = Vec::new();
    for (_dir, mut files) in by_dir {
        if files.len() < cfg.min_files {
            individual.extend(files);
            continue;
        }
        // Deterministic order so bundle membership is reproducible.
        files.sort_by(|a, b| a.rel.cmp(&b.rel));

        let mut cur: Vec<&'a LocalEntry> = Vec::new();
        let mut cur_bytes: u64 = 0;
        for e in files {
            let would_overflow = !cur.is_empty()
                && (cur.len() >= cfg.max_files || cur_bytes.saturating_add(e.size) > cfg.max_bytes);
            if would_overflow {
                flush_group(std::mem::take(&mut cur), &mut bundles, &mut individual);
                cur_bytes = 0;
            }
            cur_bytes = cur_bytes.saturating_add(e.size);
            cur.push(e);
        }
        flush_group(cur, &mut bundles, &mut individual);
    }

    (bundles, individual)
}

/// Commit one packed group: a group of >= 2 members becomes a bundle; a lone
/// member falls back to an individual upload (a single-file bundle is pure tar +
/// gzip overhead versus a direct upload).
fn flush_group<'a>(
    group: Vec<&'a LocalEntry>,
    bundles: &mut Vec<Vec<BundleMemberPlan>>,
    individual: &mut Vec<&'a LocalEntry>,
) {
    if group.len() >= 2 {
        bundles.push(
            group
                .into_iter()
                .map(|e| BundleMemberPlan {
                    relative_path: e.rel.clone(),
                    size: e.size,
                })
                .collect(),
        );
    } else {
        individual.extend(group);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use anyhow::Result;
    use async_trait::async_trait;

    use super::*;
    use crate::state::{
        AccountRow, ActivityFilter, ActivityPage, FileSearchHit, FileStateRow, NewActivity,
        NewPendingOp, PageRequest, PendingOpRow, SourceRow, StateRepo,
    };
    use crate::types::{
        AccountId, AccountState, ActivityId, FileStateStatus, LocalEntry, Op, PendingOpId,
        RelativePath, ScanResult, SourceId, UnixMs,
    };

    /// In-memory `StateRepo` fake. Only the two methods `plan()` exercises
    /// (`get_file_state`, `delete_file_state`) have real bodies; every other
    /// method is `unimplemented!()` (allowed under `#[cfg(test)]`).
    ///
    /// State lives behind `std::sync::Mutex` (not `RefCell`) because
    /// `StateRepo: Send + Sync` and the `#[async_trait]` futures must be
    /// `Send` - `RefCell` is `!Sync` and would fail the bound.
    #[derive(Default)]
    struct FakeStateRepo {
        rows: Mutex<HashMap<(SourceId, RelativePath), FileStateRow>>,
        /// Paths passed to `delete_file_state`, for assertions.
        deleted: Mutex<Vec<(SourceId, RelativePath)>>,
    }

    impl FakeStateRepo {
        fn with_rows(rows: Vec<FileStateRow>) -> Self {
            let map = rows
                .into_iter()
                .map(|r| ((r.source_id, r.relative_path.clone()), r))
                .collect();
            Self {
                rows: Mutex::new(map),
                deleted: Mutex::new(Vec::new()),
            }
        }

        fn deleted_paths(&self) -> Vec<(SourceId, RelativePath)> {
            self.deleted.lock().expect("lock").clone()
        }

        fn contains(&self, source: SourceId, path: &RelativePath) -> bool {
            self.rows
                .lock()
                .expect("lock")
                .contains_key(&(source, path.clone()))
        }
    }

    #[async_trait]
    impl StateRepo for FakeStateRepo {
        async fn get_file_state(
            &self,
            source: SourceId,
            path: &RelativePath,
        ) -> Result<Option<FileStateRow>> {
            Ok(self
                .rows
                .lock()
                .expect("lock")
                .get(&(source, path.clone()))
                .cloned())
        }

        async fn delete_file_state(&self, source: SourceId, path: &RelativePath) -> Result<()> {
            self.rows
                .lock()
                .expect("lock")
                .remove(&(source, path.clone()));
            self.deleted
                .lock()
                .expect("lock")
                .push((source, path.clone()));
            Ok(())
        }

        async fn clear_file_state_drive_file_id(
            &self,
            source: SourceId,
            path: &RelativePath,
        ) -> Result<()> {
            if let Some(row) = self
                .rows
                .lock()
                .expect("lock")
                .get_mut(&(source, path.clone()))
            {
                row.drive_file_id = None;
            }
            Ok(())
        }

        async fn mark_excluded_orphans(
            &self,
            _source: SourceId,
            _paths: &[RelativePath],
        ) -> Result<u64> {
            unimplemented!()
        }

        async fn bump_checksum_mismatch_count(
            &self,
            _source: SourceId,
            _path: &RelativePath,
        ) -> Result<u32> {
            unimplemented!("not used by planner tests")
        }
        async fn clear_checksum_mismatch_count(
            &self,
            _source: SourceId,
            _path: &RelativePath,
        ) -> Result<()> {
            unimplemented!("not used by planner tests")
        }

        // --- everything below is untouched by plan(); test-only stubs. -------

        async fn list_accounts(&self) -> Result<Vec<AccountRow>> {
            unimplemented!()
        }
        async fn upsert_account(&self, _row: &AccountRow) -> Result<()> {
            unimplemented!()
        }
        async fn mark_account_state(&self, _id: AccountId, _state: AccountState) -> Result<()> {
            unimplemented!()
        }
        async fn account_state(&self, _id: AccountId) -> Result<Option<AccountState>> {
            unimplemented!()
        }
        async fn mark_account_synced(&self, _id: AccountId, _at: UnixMs) -> Result<()> {
            unimplemented!()
        }
        async fn delete_account(&self, _id: AccountId) -> Result<()> {
            unimplemented!()
        }
        async fn list_sources(&self) -> Result<Vec<SourceRow>> {
            unimplemented!()
        }
        async fn list_enabled_sources_for(&self, _account: AccountId) -> Result<Vec<SourceRow>> {
            unimplemented!()
        }
        async fn upsert_source(&self, _row: &SourceRow) -> Result<()> {
            unimplemented!()
        }
        async fn mark_source_scanned(
            &self,
            _id: SourceId,
            _full_scan_at: UnixMs,
            _deep_verify_at: Option<UnixMs>,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn set_source_mtime_granularity(
            &self,
            _id: SourceId,
            _granularity_ns: i64,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn delete_source(&self, _id: SourceId) -> Result<()> {
            unimplemented!()
        }
        async fn load_source_file_state(
            &self,
            source: SourceId,
        ) -> Result<HashMap<RelativePath, FileStateRow>> {
            Ok(self
                .rows
                .lock()
                .expect("lock")
                .iter()
                .filter(|((s, _), _)| *s == source)
                .map(|((_, p), r)| (p.clone(), r.clone()))
                .collect())
        }
        async fn upsert_file_state(&self, _row: &FileStateRow) -> Result<()> {
            unimplemented!()
        }
        async fn enqueue_pending_op(&self, _row: NewPendingOp) -> Result<PendingOpId> {
            unimplemented!()
        }
        async fn get_pending_ops_due(
            &self,
            _now_ms: UnixMs,
            _limit: u32,
        ) -> Result<Vec<PendingOpRow>> {
            unimplemented!()
        }
        async fn get_pending_ops_for_source(&self, _source: SourceId) -> Result<Vec<PendingOpRow>> {
            unimplemented!()
        }
        async fn mark_pending_op_attempted(
            &self,
            _id: PendingOpId,
            _error: Option<&str>,
            _next_attempt_ms: UnixMs,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn delete_pending_op(&self, _id: PendingOpId) -> Result<()> {
            unimplemented!()
        }
        async fn update_pending_op_payload(
            &self,
            _id: PendingOpId,
            _payload_json: &serde_json::Value,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn commit_create_result(
            &self,
            _op_id: PendingOpId,
            _file_state: &FileStateRow,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn commit_update_result(
            &self,
            _op_id: PendingOpId,
            _file_state: &FileStateRow,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn write_activity(&self, _row: NewActivity) -> Result<ActivityId> {
            unimplemented!()
        }
        async fn query_activity(
            &self,
            _filter: ActivityFilter,
            _page: PageRequest,
        ) -> Result<ActivityPage> {
            unimplemented!()
        }
        async fn prune_activity_older_than(
            &self,
            _before_ms: UnixMs,
            _hard_cap: u64,
            _batch_size: Option<u32>,
        ) -> Result<u64> {
            unimplemented!()
        }
        async fn delete_activity_by_source(&self, _source: SourceId) -> Result<u64> {
            unimplemented!()
        }
        async fn schema_version(&self) -> Result<i64> {
            unimplemented!()
        }
        async fn table_row_count(&self, _table: &str) -> Result<i64> {
            unimplemented!()
        }
        async fn get_setting(&self, _key: &str) -> Result<Option<serde_json::Value>> {
            unimplemented!()
        }
        async fn set_setting(&self, _key: &str, _value: &serde_json::Value) -> Result<()> {
            unimplemented!()
        }
        async fn patch_setting_field(
            &self,
            _key: &str,
            _field: &str,
            _value: &serde_json::Value,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn search_files(
            &self,
            _source: Option<SourceId>,
            _query: &str,
            _limit: u32,
        ) -> Result<Vec<FileSearchHit>> {
            unimplemented!()
        }
    }

    // --- test helpers --------------------------------------------------------

    fn rel(s: &str) -> RelativePath {
        RelativePath::try_from(s.to_string()).expect("valid relative path")
    }

    fn test_source(id: SourceId) -> SourceRow {
        SourceRow {
            id,
            account_id: AccountId::new_v4(),
            display_name: "test".into(),
            enabled: true,
            local_path: "/tmp/src".into(),
            drive_folder_id: "folder-1".into(),
            drive_id: None,
            drive_folder_path: "/Backups/test".into(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore: true,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            mtime_granularity_ns: None,
            created_at: 0,
        }
    }

    /// A fixed "now" for planner tests (Unix ms), comfortably in the future of
    /// [`COLD_MTIME_NS`] so the coldness gate (issue #35 item b) passes for every
    /// [`local_entry`]; the dedicated coldness tests override the mtime.
    const TEST_NOW_MS: crate::types::UnixMs = 2_000_000_000_000;

    /// A fixed mtime (ns since epoch) decades before [`TEST_NOW_MS`], so a
    /// [`local_entry`] is always "cold" under the default 30-day gate.
    const COLD_MTIME_NS: i64 = 1_000_000_000_000_000;

    fn local_entry(path: &str, size: u64) -> LocalEntry {
        LocalEntry {
            rel: rel(path),
            size,
            mtime_ns: COLD_MTIME_NS,
        }
    }

    /// An enabled bundle config with a LOW `min_files` so the compact fixtures
    /// (~8-10 files) below actually bundle - the production default is 100, far
    /// above any unit fixture. Coldness uses the real 30-day default (fixtures are
    /// [`COLD_MTIME_NS`]-old, so they pass it).
    fn enabled_test_cfg() -> BundleConfig {
        BundleConfig {
            min_files: 3,
            ..BundleConfig::enabled_defaults()
        }
    }

    /// A `file_state` row that HAS reached Drive (has a `drive_file_id`).
    fn synced_row(source: SourceId, path: &str, file_id: &str) -> FileStateRow {
        FileStateRow {
            source_id: source,
            relative_path: rel(path),
            size: 10,
            mtime_ns: 0,
            hash_blake3: [0u8; 32],
            drive_file_id: Some(file_id.into()),
            drive_md5: Some([0u8; 16]),
            encrypted_remote_path: None,
            status: FileStateStatus::Synced,
            last_uploaded_at: Some(1),
            last_verified_at: None,
        }
    }

    /// A `file_state` row that NEVER reached Drive (`drive_file_id` is None).
    fn pending_row(source: SourceId, path: &str) -> FileStateRow {
        FileStateRow {
            source_id: source,
            relative_path: rel(path),
            size: 10,
            mtime_ns: 0,
            hash_blake3: [0u8; 32],
            drive_file_id: None,
            drive_md5: None,
            encrypted_remote_path: None,
            status: FileStateStatus::Pending,
            last_uploaded_at: None,
            last_verified_at: None,
        }
    }

    /// Empty remote, first scan: every observed file is new -> all uploads,
    /// no trashes (ROADMAP M2 "empty remote first scan").
    #[tokio::test]
    async fn empty_remote_first_scan_all_uploads() {
        let source = test_source(SourceId::new_v4());
        let scan = ScanResult {
            new_or_changed: vec![local_entry("a.txt", 1), local_entry("b/c.txt", 2)],
            deleted: Vec::new(),
            collisions: Vec::new(),
            excluded_orphans: Vec::new(),
            ads_skipped: Vec::new(),
            invalid_filenames: Vec::new(),
            probed_granularity_ns: None,
        };
        let state = FakeStateRepo::default();

        let plan = plan(
            &source,
            &scan,
            &state,
            TEST_NOW_MS,
            &BundleConfig::default(),
        )
        .await
        .unwrap();

        let summary = plan.summary();
        assert_eq!(summary.uploads, 2);
        assert_eq!(summary.trashes, 0);
        assert_eq!(summary.bytes, 3);
        assert!(plan
            .ops
            .iter()
            .all(|op| matches!(op, Op::HashThenUpload { .. })));
    }

    /// Nothing changed: the scan is empty -> the plan is empty.
    #[tokio::test]
    async fn unchanged_yields_empty_plan() {
        let source = test_source(SourceId::new_v4());
        let scan = ScanResult::default();
        let state = FakeStateRepo::default();

        let plan = plan(
            &source,
            &scan,
            &state,
            TEST_NOW_MS,
            &BundleConfig::default(),
        )
        .await
        .unwrap();

        assert!(plan.ops.is_empty());
        let summary = plan.summary();
        assert_eq!(summary.uploads, 0);
        assert_eq!(summary.trashes, 0);
        assert_eq!(summary.bytes, 0);
    }

    /// A single changed file -> exactly one upload op carrying its size.
    #[tokio::test]
    async fn single_change_one_upload() {
        let source = test_source(SourceId::new_v4());
        let scan = ScanResult {
            new_or_changed: vec![local_entry("doc.txt", 42)],
            deleted: Vec::new(),
            collisions: Vec::new(),
            excluded_orphans: Vec::new(),
            ads_skipped: Vec::new(),
            invalid_filenames: Vec::new(),
            probed_granularity_ns: None,
        };
        let state = FakeStateRepo::default();

        let plan = plan(
            &source,
            &scan,
            &state,
            TEST_NOW_MS,
            &BundleConfig::default(),
        )
        .await
        .unwrap();

        assert_eq!(plan.ops.len(), 1);
        match &plan.ops[0] {
            Op::HashThenUpload {
                source_id,
                relative_path,
                size,
            } => {
                assert_eq!(*source_id, source.id);
                assert_eq!(relative_path, &rel("doc.txt"));
                assert_eq!(*size, 42);
            }
            other => panic!("expected HashThenUpload, got {other:?}"),
        }
    }

    /// A single deleted file whose row HAS a `drive_file_id` -> one trash
    /// op targeting that remote object (ROADMAP M2 "single file deleted").
    #[tokio::test]
    async fn single_delete_with_drive_id_one_trash() {
        let source = test_source(SourceId::new_v4());
        let row = synced_row(source.id, "gone.txt", "drive-file-99");
        let scan = ScanResult {
            new_or_changed: Vec::new(),
            deleted: vec![rel("gone.txt")],
            collisions: Vec::new(),
            excluded_orphans: Vec::new(),
            ads_skipped: Vec::new(),
            invalid_filenames: Vec::new(),
            probed_granularity_ns: None,
        };
        let state = FakeStateRepo::with_rows(vec![row]);

        let plan = plan(
            &source,
            &scan,
            &state,
            TEST_NOW_MS,
            &BundleConfig::default(),
        )
        .await
        .unwrap();

        assert_eq!(plan.ops.len(), 1);
        match &plan.ops[0] {
            Op::Trash {
                source_id,
                relative_path,
                drive_file_id,
            } => {
                assert_eq!(*source_id, source.id);
                assert_eq!(relative_path, &rel("gone.txt"));
                assert_eq!(drive_file_id, "drive-file-99");
            }
            other => panic!("expected Trash, got {other:?}"),
        }
        let summary = plan.summary();
        assert_eq!(summary.trashes, 1);
        assert_eq!(summary.uploads, 0);
        // The row is NOT deleted here; the executor removes it after the
        // trash op succeeds (SPEC s8). The planner only emits the op.
        assert!(state.deleted_paths().is_empty());
    }

    /// A deleted path that never reached Drive (row has no `drive_file_id`)
    /// -> zero ops and the `file_state` row is removed directly (SPEC s7).
    #[tokio::test]
    async fn delete_never_uploaded_zero_ops_row_removed() {
        let source = test_source(SourceId::new_v4());
        let row = pending_row(source.id, "never-up.txt");
        let scan = ScanResult {
            new_or_changed: Vec::new(),
            deleted: vec![rel("never-up.txt")],
            collisions: Vec::new(),
            excluded_orphans: Vec::new(),
            ads_skipped: Vec::new(),
            invalid_filenames: Vec::new(),
            probed_granularity_ns: None,
        };
        let state = FakeStateRepo::with_rows(vec![row]);

        let plan = plan(
            &source,
            &scan,
            &state,
            TEST_NOW_MS,
            &BundleConfig::default(),
        )
        .await
        .unwrap();

        assert!(plan.ops.is_empty());
        // delete_file_state was called for the never-uploaded path...
        assert_eq!(
            state.deleted_paths(),
            vec![(source.id, rel("never-up.txt"))]
        );
        // ...and the row is actually gone.
        assert!(!state.contains(source.id, &rel("never-up.txt")));
    }

    /// A rename surfaces as delete(old) + add(new). V1 does NOT detect
    /// renames, so the plan is exactly one upload (new path) + one trash
    /// (old path's remote object) - see module docs / ROADMAP M2.
    #[tokio::test]
    async fn rename_yields_one_upload_and_one_trash() {
        let source = test_source(SourceId::new_v4());
        let old_row = synced_row(source.id, "old-name.txt", "drive-old");
        let scan = ScanResult {
            new_or_changed: vec![local_entry("new-name.txt", 7)],
            deleted: vec![rel("old-name.txt")],
            collisions: Vec::new(),
            excluded_orphans: Vec::new(),
            ads_skipped: Vec::new(),
            invalid_filenames: Vec::new(),
            probed_granularity_ns: None,
        };
        let state = FakeStateRepo::with_rows(vec![old_row]);

        let plan = plan(
            &source,
            &scan,
            &state,
            TEST_NOW_MS,
            &BundleConfig::default(),
        )
        .await
        .unwrap();

        let summary = plan.summary();
        assert_eq!(summary.uploads, 1, "rename uploads the new path");
        assert_eq!(summary.trashes, 1, "rename trashes the old remote object");
        assert_eq!(summary.bytes, 7);

        // Uploads are emitted before trashes.
        assert!(matches!(plan.ops[0], Op::HashThenUpload { .. }));
        match &plan.ops[1] {
            Op::Trash {
                relative_path,
                drive_file_id,
                ..
            } => {
                assert_eq!(relative_path, &rel("old-name.txt"));
                assert_eq!(drive_file_id, "drive-old");
            }
            other => panic!("expected Trash, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------------
    // V2 small-file bundling (issue #35).
    // -------------------------------------------------------------------------

    /// Build a `ScanResult` whose `new_or_changed` is exactly `entries`.
    fn scan_new(entries: Vec<LocalEntry>) -> ScanResult {
        ScanResult {
            new_or_changed: entries,
            deleted: Vec::new(),
            collisions: Vec::new(),
            excluded_orphans: Vec::new(),
            ads_skipped: Vec::new(),
            invalid_filenames: Vec::new(),
            probed_granularity_ns: None,
        }
    }

    fn bundle_ops(plan: &Plan) -> Vec<&Vec<BundleMemberPlan>> {
        plan.ops
            .iter()
            .filter_map(|op| match op {
                Op::UploadBundle { members, .. } => Some(members),
                _ => None,
            })
            .collect()
    }

    fn upload_count(plan: &Plan) -> usize {
        plan.ops
            .iter()
            .filter(|op| matches!(op, Op::HashThenUpload { .. }))
            .count()
    }

    /// Many small new files in one directory pack into ONE bundle; no individual
    /// uploads. The plan summary counts each member.
    #[tokio::test]
    async fn bundling_groups_many_new_small_files_into_one_bundle() {
        let source = test_source(SourceId::new_v4());
        let entries: Vec<LocalEntry> = (0..10)
            .map(|i| local_entry(&format!("logs/f{i}.log"), 100 + i as u64))
            .collect();
        let state = FakeStateRepo::default();

        let plan = plan(
            &source,
            &scan_new(entries),
            &state,
            TEST_NOW_MS,
            &enabled_test_cfg(),
        )
        .await
        .unwrap();

        let bundles = bundle_ops(&plan);
        assert_eq!(bundles.len(), 1, "one directory of 10 files => one bundle");
        assert_eq!(bundles[0].len(), 10);
        assert_eq!(upload_count(&plan), 0, "no individual uploads");
        let summary = plan.summary();
        assert_eq!(
            summary.uploads, 10,
            "summary counts each member as an upload"
        );
    }

    /// A directory with fewer than `min_files` eligible files is NOT bundled -
    /// each file uploads individually.
    #[tokio::test]
    async fn bundling_leaves_small_groups_as_individual_uploads() {
        let source = test_source(SourceId::new_v4());
        let entries = vec![
            local_entry("a.txt", 10),
            local_entry("b.txt", 20),
            local_entry("c.txt", 30),
        ];
        let state = FakeStateRepo::default();

        // The real default `min_files` (100) far exceeds these 3 files, so none
        // bundle - they upload individually.
        let plan = plan(
            &source,
            &scan_new(entries),
            &state,
            TEST_NOW_MS,
            &BundleConfig::enabled_defaults(),
        )
        .await
        .unwrap();

        assert!(bundle_ops(&plan).is_empty(), "too few to bundle");
        assert_eq!(upload_count(&plan), 3);
    }

    /// Files at or above the per-member ceiling never bundle; small siblings still
    /// do. A big file is an individual upload alongside the bundle.
    #[tokio::test]
    async fn bundling_excludes_large_files() {
        let source = test_source(SourceId::new_v4());
        let mut entries: Vec<LocalEntry> = (0..8)
            .map(|i| local_entry(&format!("d/small{i}.bin"), 1000))
            .collect();
        entries.push(local_entry("d/big.bin", BUNDLE_MAX_FILE_SIZE + 1));
        let state = FakeStateRepo::default();

        let plan = plan(
            &source,
            &scan_new(entries),
            &state,
            TEST_NOW_MS,
            &enabled_test_cfg(),
        )
        .await
        .unwrap();

        let bundles = bundle_ops(&plan);
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].len(), 8, "only the small files are bundled");
        assert_eq!(upload_count(&plan), 1, "the big file uploads individually");
    }

    /// A CHANGED file (one with an existing `file_state` row) is never bundled -
    /// only genuinely-new files are. This is what keeps bundling from stranding an
    /// old standalone object and avoids re-bundling.
    #[tokio::test]
    async fn bundling_only_new_files_not_changed_ones() {
        let source = test_source(SourceId::new_v4());
        // One previously-uploaded file (has a drive_file_id) that changed.
        let existing = synced_row(source.id, "logs/existing.log", "drive-existing");
        let state = FakeStateRepo::with_rows(vec![existing]);

        let mut entries: Vec<LocalEntry> = (0..8)
            .map(|i| local_entry(&format!("logs/new{i}.log"), 50))
            .collect();
        entries.push(local_entry("logs/existing.log", 999)); // changed

        let plan = plan(
            &source,
            &scan_new(entries),
            &state,
            TEST_NOW_MS,
            &enabled_test_cfg(),
        )
        .await
        .unwrap();

        let bundles = bundle_ops(&plan);
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].len(), 8, "only the 8 NEW files are bundled");
        assert!(
            bundles[0]
                .iter()
                .all(|m| m.relative_path.as_str() != "logs/existing.log"),
            "the changed file must NOT be a bundle member"
        );
        assert_eq!(
            upload_count(&plan),
            1,
            "the changed file uploads standalone"
        );
    }

    /// Disabled config is byte-for-byte the v1.0.0 behaviour: every new file is an
    /// individual upload, never a bundle.
    #[tokio::test]
    async fn bundling_disabled_is_identity() {
        let source = test_source(SourceId::new_v4());
        let entries: Vec<LocalEntry> = (0..20)
            .map(|i| local_entry(&format!("logs/f{i}.log"), 10))
            .collect();
        let state = FakeStateRepo::default();

        let plan = plan(
            &source,
            &scan_new(entries),
            &state,
            TEST_NOW_MS,
            &BundleConfig::default(),
        )
        .await
        .unwrap();

        assert!(bundle_ops(&plan).is_empty());
        assert_eq!(upload_count(&plan), 20);
    }

    /// The byte cap splits a large directory into multiple bundles, each within
    /// the cap; every file is still accounted for.
    #[tokio::test]
    async fn bundling_splits_on_byte_cap() {
        let source = test_source(SourceId::new_v4());
        // 10 files of 1000 bytes each; a 2500-byte cap => bundles of <= 2 files
        // (a 3rd file at 3000 bytes would overflow), so 5 two-file bundles.
        let entries: Vec<LocalEntry> = (0..10)
            .map(|i| local_entry(&format!("d/f{i}.bin"), 1000))
            .collect();
        let cfg = BundleConfig {
            enabled: true,
            max_file_size: 10_000,
            min_files: 2,
            max_files: 512,
            max_bytes: 2500,
            min_cold_age_days: 0,
        };
        let state = FakeStateRepo::default();

        let plan = plan(&source, &scan_new(entries), &state, TEST_NOW_MS, &cfg)
            .await
            .unwrap();

        let bundles = bundle_ops(&plan);
        assert!(bundles.len() >= 4, "10 files / 2 per bundle => 5 bundles");
        let total_members: usize =
            bundles.iter().map(|b| b.len()).sum::<usize>() + upload_count(&plan);
        assert_eq!(total_members, 10, "no file is dropped");
        for b in &bundles {
            let bytes: u64 = b.iter().map(|m| m.size).sum();
            assert!(bytes <= 2500, "each bundle respects the byte cap");
        }
    }

    /// Coldness gate (issue #35 item b): fresh files - mtime younger than
    /// `min_cold_age_days` - are NOT bundled even when everything else qualifies;
    /// they upload individually (they are churn-prone). Old files in the same
    /// directory still bundle.
    #[tokio::test]
    async fn bundling_excludes_fresh_files() {
        let source = test_source(SourceId::new_v4());
        // now - 31 days, expressed in ns, is still "cold"; now - 1 day is fresh.
        let day_ns: i64 = 86_400 * 1_000_000_000;
        let now_ns: i64 = TEST_NOW_MS * 1_000_000;
        let cold_mtime = now_ns - 31 * day_ns;
        let fresh_mtime = now_ns - day_ns;

        // 8 cold files (bundle) + 4 fresh files (individual), all in one dir.
        let mut entries: Vec<LocalEntry> = (0..8)
            .map(|i| LocalEntry {
                rel: rel(&format!("d/cold{i}.log")),
                size: 100,
                mtime_ns: cold_mtime,
            })
            .collect();
        entries.extend((0..4).map(|i| LocalEntry {
            rel: rel(&format!("d/fresh{i}.log")),
            size: 100,
            mtime_ns: fresh_mtime,
        }));
        let state = FakeStateRepo::default();

        let plan = plan(
            &source,
            &scan_new(entries),
            &state,
            TEST_NOW_MS,
            &enabled_test_cfg(),
        )
        .await
        .unwrap();

        let bundles = bundle_ops(&plan);
        assert_eq!(bundles.len(), 1, "the 8 cold files pack into one bundle");
        assert_eq!(bundles[0].len(), 8, "only cold files are bundled");
        assert!(
            bundles[0]
                .iter()
                .all(|m| !m.relative_path.as_str().contains("fresh")),
            "no fresh file is a bundle member"
        );
        assert_eq!(
            upload_count(&plan),
            4,
            "the 4 fresh files upload individually"
        );
    }

    /// A file whose mtime is the scanner's `0` "unknown" sentinel is treated as
    /// NOT cold - we cannot prove it is old, so it never bundles (issue #35).
    #[tokio::test]
    async fn bundling_excludes_unknown_mtime_files() {
        let source = test_source(SourceId::new_v4());
        let entries: Vec<LocalEntry> = (0..8)
            .map(|i| LocalEntry {
                rel: rel(&format!("d/u{i}.log")),
                size: 100,
                mtime_ns: 0,
            })
            .collect();
        let state = FakeStateRepo::default();

        let plan = plan(
            &source,
            &scan_new(entries),
            &state,
            TEST_NOW_MS,
            &enabled_test_cfg(),
        )
        .await
        .unwrap();

        assert!(
            bundle_ops(&plan).is_empty(),
            "unknown-mtime files never bundle"
        );
        assert_eq!(upload_count(&plan), 8, "they upload individually");
    }

    /// `min_cold_age_days == 0` disables the coldness gate: any file with a KNOWN
    /// (non-zero) mtime is eligible regardless of age.
    #[tokio::test]
    async fn bundling_cold_age_zero_disables_gate() {
        let source = test_source(SourceId::new_v4());
        let now_ns: i64 = TEST_NOW_MS * 1_000_000;
        // A file modified "now" - fresh - still bundles when the gate is off.
        let entries: Vec<LocalEntry> = (0..8)
            .map(|i| LocalEntry {
                rel: rel(&format!("d/f{i}.log")),
                size: 100,
                mtime_ns: now_ns,
            })
            .collect();
        let cfg = BundleConfig {
            min_cold_age_days: 0,
            ..enabled_test_cfg()
        };
        let state = FakeStateRepo::default();

        let plan = plan(&source, &scan_new(entries), &state, TEST_NOW_MS, &cfg)
            .await
            .unwrap();

        let bundles = bundle_ops(&plan);
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].len(), 8);
    }
}
