//! Persistent state contract: the [`StateRepo`] trait and its row types.
//!
//! Mirrors the SQLite schema in SPEC s2 row-for-row. The M1 phase 2
//! implementation (`sqlx` + the migrations under `src/migrations/`) lives
//! behind this trait so the orchestrator, scanner, planner, and executor
//! are exercisable against an in-memory [`StateRepo`] fake without ever
//! touching SQLite. The reconciliation queries DESIGN s5.6 specifies
//! ride on the same trait methods.
//!
//! Result type: [`anyhow::Result`] on every method, matching the sibling
//! [`crate::types`] / [`driven_drive::remote_store`] surface and the
//! orchestrator that consumes both. The library-crate house rule
//! preferring `thiserror` is intentionally overridden at this seam so the
//! orchestrator can `?`-bubble both kinds of error without an adaptor.
//! Inner SQLite-specific errors stay typed inside the M1 phase 2 impl.

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::types::{
    AccountId, AccountState, ActivityId, FileStateStatus, PendingOpId, RelativePath, SourceId,
    UnixMs,
};

pub mod sqlite;
pub use sqlite::SqliteStateRepo;

// -----------------------------------------------------------------------------
// Row types - mirror SPEC s2 column shapes.
// -----------------------------------------------------------------------------

/// One row of `accounts` (SPEC s2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRow {
    /// `accounts.id`.
    pub id: AccountId,
    /// `accounts.email`.
    pub email: String,
    /// `accounts.display_name` (NULLable in the schema).
    pub display_name: Option<String>,
    /// `accounts.state`.
    pub state: AccountState,
    /// `accounts.encryption_master_key_id` - keychain handle; the master
    /// key itself never lives in SQLite (SPEC s2).
    pub encryption_master_key_id: Option<String>,
    /// `accounts.created_at`.
    pub created_at: UnixMs,
    /// `accounts.last_synced_at` (NULL until first successful sync).
    pub last_synced_at: Option<UnixMs>,
}

/// One row of `backup_sources` (SPEC s2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRow {
    /// `backup_sources.id`.
    pub id: SourceId,
    /// FK to `accounts.id`.
    pub account_id: AccountId,
    /// `backup_sources.display_name`.
    pub display_name: String,
    /// `backup_sources.enabled` (`INTEGER` 0/1 in SQL).
    pub enabled: bool,
    /// Absolute local path to the source root.
    pub local_path: String,
    /// Drive `folder_id` the source uploads into.
    pub drive_folder_id: String,
    /// Cached display path of the Drive folder for UI rendering.
    pub drive_folder_path: String,
    /// Whether per-source encryption is on.
    pub encryption_enabled: bool,
    /// Per-source key wrapped by the account's master key (raw bytes;
    /// `BLOB` in SQL). `None` when encryption is off.
    pub wrapped_source_key: Option<Vec<u8>>,
    /// Whether `.gitignore` files are honoured during scan.
    pub respect_gitignore: bool,
    /// User-supplied include globs (JSON array in SQL).
    pub include_patterns: Vec<String>,
    /// User-supplied exclude globs (JSON array in SQL).
    pub exclude_patterns: Vec<String>,
    /// V2 reserved schedule JSON; V1 code never reads this column.
    pub schedule_json_v2_reserved: Option<String>,
    /// Deep-verify cadence in seconds (default `604800` = 7 days).
    pub deep_verify_interval_secs: u32,
    /// Wall-time of last completed full scan; `None` until the first scan
    /// finishes.
    pub last_full_scan_at: Option<UnixMs>,
    /// Wall-time of last completed deep-verify.
    pub last_deep_verify_at: Option<UnixMs>,
    /// `backup_sources.created_at`.
    pub created_at: UnixMs,
}

/// One row of `file_state` (SPEC s2).
///
/// Primary key is the `(source_id, relative_path)` pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStateRow {
    /// FK to `backup_sources.id`.
    pub source_id: SourceId,
    /// Path under the source's `local_path`.
    pub relative_path: RelativePath,
    /// Local file size in bytes.
    pub size: u64,
    /// Local mtime in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Plaintext BLAKE3 hash of the file (32 bytes). For encrypted
    /// sources this is still the plaintext hash so identity survives a
    /// key rotation (SPEC s2 column comment).
    pub hash_blake3: [u8; 32],
    /// Drive `file_id` once the file has been uploaded; `None` until then.
    pub drive_file_id: Option<String>,
    /// MD5 (16 bytes) of the bytes actually stored on Drive; ciphertext
    /// md5 for encrypted sources.
    pub drive_md5: Option<[u8; 16]>,
    /// Cached encrypted remote path for encrypted sources.
    pub encrypted_remote_path: Option<String>,
    /// Sync status.
    pub status: FileStateStatus,
    /// Wall-time of the last successful upload of this file's current
    /// bytes.
    pub last_uploaded_at: Option<UnixMs>,
    /// Wall-time of the last successful deep-verify.
    pub last_verified_at: Option<UnixMs>,
}

/// Op-type discriminant stored in `pending_ops.op_type` (SPEC s2).
///
/// Held as a plain string rather than the [`crate::types::Op`] enum to
/// keep the row type stable while the op enum grows (M1 phase 1 ships a
/// stub of `Op`; new variants land in M2+ without a schema migration of
/// the row).
pub type PendingOpType = String;

/// A row to insert into `pending_ops` (SPEC s2). Excludes `id` and
/// `attempts` and `last_error` (set by the storage layer on enqueue).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPendingOp {
    /// FK to `backup_sources.id`.
    pub source_id: SourceId,
    /// `'upload' | 'trash' | 'resume' | 'verify'`.
    pub op_type: PendingOpType,
    /// Path the op operates on.
    pub relative_path: RelativePath,
    /// Op-specific payload (resumable session URL, etc.); JSON-encoded.
    pub payload_json: serde_json::Value,
    /// When the op becomes due (Unix epoch ms). Use the current time for
    /// "run me now".
    pub scheduled_for: UnixMs,
    /// Wall-time the row was created (SPEC s2 `created_at`).
    pub created_at: UnixMs,
}

/// One row of `pending_ops` (SPEC s2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOpRow {
    /// Auto-increment id.
    pub id: PendingOpId,
    /// FK to `backup_sources.id`.
    pub source_id: SourceId,
    /// `'upload' | 'trash' | 'resume' | 'verify'`.
    pub op_type: PendingOpType,
    /// Path the op operates on.
    pub relative_path: RelativePath,
    /// Op-specific payload.
    pub payload_json: serde_json::Value,
    /// Retry count.
    pub attempts: u32,
    /// Last error message, if any.
    pub last_error: Option<String>,
    /// When the op next becomes due.
    pub scheduled_for: UnixMs,
    /// Wall-time the row was created.
    pub created_at: UnixMs,
}

/// Level discriminant on `activity_log.level` (SPEC s2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityLevel {
    /// Informational entry; visible in the default Activity view.
    Info,
    /// Warning entry; visible by default with a yellow badge.
    Warn,
    /// Error entry; visible by default with a red badge.
    Error,
}

/// A row to insert into `activity_log` (SPEC s2). Excludes `id` (assigned
/// by the storage layer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewActivity {
    /// Wall-time the event occurred (Unix epoch ms).
    pub ts: UnixMs,
    /// Source the event belongs to, or `None` for global events.
    pub source_id: Option<SourceId>,
    /// Severity.
    pub level: ActivityLevel,
    /// Event-type discriminant
    /// (e.g. `"scan_done" | "upload_done" | "trash_done" | "paused"`).
    pub event_type: String,
    /// File count associated with the event (e.g. uploads in a batch).
    pub file_count: Option<u64>,
    /// Byte count associated with the event.
    pub bytes: Option<u64>,
    /// Free-form human-readable message.
    pub message: Option<String>,
}

/// One row of `activity_log` (SPEC s2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    /// Auto-increment id.
    pub id: ActivityId,
    /// Wall-time of the event.
    pub ts: UnixMs,
    /// Source the event belongs to.
    pub source_id: Option<SourceId>,
    /// Severity.
    pub level: ActivityLevel,
    /// Event-type discriminant.
    pub event_type: String,
    /// File count.
    pub file_count: Option<u64>,
    /// Byte count.
    pub bytes: Option<u64>,
    /// Free-form message.
    pub message: Option<String>,
}

/// Filter for [`StateRepo::query_activity`].
///
/// All fields are optional; an empty filter matches every row. Conditions
/// combine with logical AND.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityFilter {
    /// Limit results to a single source.
    pub source_id: Option<SourceId>,
    /// Lower-bound timestamp, inclusive.
    pub since_ms: Option<UnixMs>,
    /// Upper-bound timestamp, exclusive.
    pub before_ms: Option<UnixMs>,
    /// Minimum severity (`Info <= Warn <= Error`).
    pub min_level: Option<ActivityLevel>,
    /// Event-type discriminants to include; empty = all.
    pub event_types: Vec<String>,
}

/// Page selector for `query_*` methods (SPEC s18.8 bounds: `limit
/// 1..=10_000`, `page 0..=u32::MAX`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRequest {
    /// Zero-based page index. `offset = page * limit`.
    pub page: u32,
    /// Max rows per page.
    pub limit: u32,
}

/// One page of activity rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityPage {
    /// Rows in newest-first order (SPEC s2 `idx_activity_ts ON
    /// activity_log(ts DESC)`).
    pub rows: Vec<ActivityRow>,
    /// Total matching rows across all pages (for UI paging widgets).
    pub total: u64,
}

/// One hit from the `file_state_fts` virtual table (SPEC s2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSearchHit {
    /// Source the match belongs to.
    pub source_id: SourceId,
    /// Matched relative path.
    pub relative_path: RelativePath,
    /// Current sync status (mirrors the `file_state` join).
    pub status: FileStateStatus,
    /// Drive `file_id` if uploaded.
    pub drive_file_id: Option<String>,
}

// -----------------------------------------------------------------------------
// The trait surface.
// -----------------------------------------------------------------------------

/// Storage contract for the SQLite-backed state at
/// `<config_dir>/driven/state.db` (SPEC s2).
///
/// The orchestrator, scanner, planner, and executor consume this trait
/// rather than the SQLite handle directly so they remain test-friendly
/// (the M1 phase 2 impl is `sqlx` over SQLite; an in-memory fake may be
/// substituted in tests).
#[async_trait]
pub trait StateRepo: Send + Sync {
    // --- accounts -----------------------------------------------------------

    /// Returns every row in `accounts`.
    async fn list_accounts(&self) -> Result<Vec<AccountRow>>;

    /// Inserts or replaces an `accounts` row by id.
    async fn upsert_account(&self, row: &AccountRow) -> Result<()>;

    /// Updates `accounts.state` for the given account.
    async fn mark_account_state(&self, id: AccountId, state: AccountState) -> Result<()>;

    /// Deletes an `accounts` row and (via `ON DELETE CASCADE` per SPEC
    /// s2) every dependent row in `backup_sources`, `file_state`, and
    /// `pending_ops`.
    async fn delete_account(&self, id: AccountId) -> Result<()>;

    // --- backup_sources -----------------------------------------------------

    /// Returns every row in `backup_sources`.
    async fn list_sources(&self) -> Result<Vec<SourceRow>>;

    /// Returns every enabled source owned by the given account. Used by
    /// the orchestrator each tick.
    async fn list_enabled_sources_for(&self, account: AccountId) -> Result<Vec<SourceRow>>;

    /// Inserts or replaces a `backup_sources` row by id.
    async fn upsert_source(&self, row: &SourceRow) -> Result<()>;

    /// Deletes a `backup_sources` row and (via `ON DELETE CASCADE`) every
    /// dependent `file_state` and `pending_ops` row.
    async fn delete_source(&self, id: SourceId) -> Result<()>;

    // --- file_state ---------------------------------------------------------

    /// Loads every `file_state` row for one source as a map keyed by
    /// relative path. Used by the scanner's diff loop (SPEC s6).
    async fn load_source_file_state(
        &self,
        source: SourceId,
    ) -> Result<HashMap<RelativePath, FileStateRow>>;

    /// Returns one `file_state` row by primary key.
    async fn get_file_state(
        &self,
        source: SourceId,
        path: &RelativePath,
    ) -> Result<Option<FileStateRow>>;

    /// Inserts or replaces a `file_state` row by primary key.
    async fn upsert_file_state(&self, row: &FileStateRow) -> Result<()>;

    /// Deletes a `file_state` row by primary key.
    async fn delete_file_state(&self, source: SourceId, path: &RelativePath) -> Result<()>;

    // --- pending_ops --------------------------------------------------------

    /// Enqueues a `pending_ops` row. Returns the new auto-increment id.
    async fn enqueue_pending_op(&self, row: NewPendingOp) -> Result<PendingOpId>;

    /// Returns pending ops whose `scheduled_for <= now_ms`, ordered by
    /// `scheduled_for` ascending. Caps the result at `limit`.
    async fn get_pending_ops_due(&self, now_ms: UnixMs, limit: u32) -> Result<Vec<PendingOpRow>>;

    /// Per-source pending_ops fetch (DESIGN s5.6 reconciliation).
    ///
    /// On startup the reconciliation pass scoops in-flight resumable
    /// sessions per source so it can resume or invalidate them. The
    /// existing [`Self::get_pending_ops_due`] returns globally-due ops
    /// across all sources; this one is per-source so the orchestrator
    /// can reason about one source at a time. Rows are ordered by `id`
    /// ascending (insertion order), which is also the order resumable
    /// ops should be inspected on recovery.
    async fn get_pending_ops_for_source(&self, source: SourceId) -> Result<Vec<PendingOpRow>>;

    /// Increments `attempts`, sets `last_error`, and rolls `scheduled_for`
    /// forward to the next retry time. Used after a non-terminal failure
    /// per the pacer's backoff classification (SPEC s9).
    async fn mark_pending_op_attempted(
        &self,
        id: PendingOpId,
        error: Option<&str>,
        next_attempt_ms: UnixMs,
    ) -> Result<()>;

    /// Removes a `pending_ops` row by id. Called after the op completes
    /// or after the orchestrator gives up on it.
    async fn delete_pending_op(&self, id: PendingOpId) -> Result<()>;

    /// Atomically commit the result of a successful `create` op.
    ///
    /// Upserts the new `file_state` row AND deletes the `pending_op` that
    /// produced it in a single SQL transaction. DESIGN s5.6 step 3 calls
    /// this the load-bearing transaction for the reconciliation protocol:
    /// without it, a crash between the two writes leaves an orphaned
    /// `pending_op` whose result is already adopted into `file_state`, and
    /// the next reconciliation pass cannot tell whether the op completed
    /// or not.
    async fn commit_create_result(
        &self,
        op_id: PendingOpId,
        file_state: &FileStateRow,
    ) -> Result<()>;

    /// Atomically commit the result of a successful `update` op.
    ///
    /// Same DESIGN s5.6 invariant as [`Self::commit_create_result`] and
    /// identical SQL semantics; named distinctly so the caller's intent
    /// (create vs update) is clear at the call site.
    async fn commit_update_result(
        &self,
        op_id: PendingOpId,
        file_state: &FileStateRow,
    ) -> Result<()>;

    // --- activity_log -------------------------------------------------------

    /// Appends an `activity_log` row. Returns the new auto-increment id.
    async fn write_activity(&self, row: NewActivity) -> Result<ActivityId>;

    /// Returns a page of activity rows matching `filter`, newest-first.
    async fn query_activity(
        &self,
        filter: ActivityFilter,
        page: PageRequest,
    ) -> Result<ActivityPage>;

    /// Prune `activity_log` rows older than `before_ms`, batched to keep
    /// the write transaction short (DESIGN s18.4 retention policy).
    ///
    /// Deletes in rounds of at most `batch_size` rows per transaction
    /// (default `10_000` if `None`), stopping when a round deletes fewer
    /// than `batch_size` (no more eligible rows) OR when the cumulative
    /// total reaches `hard_cap`. After the loop runs
    /// `PRAGMA wal_checkpoint(TRUNCATE)` so a catastrophic-growth prune
    /// does not leave the freed pages stranded in the WAL. Returns the
    /// total number of rows deleted across batches.
    async fn prune_activity_older_than(
        &self,
        before_ms: UnixMs,
        hard_cap: u64,
        batch_size: Option<u32>,
    ) -> Result<u64>;

    /// Null out `activity_log.source_id` for every row owned by `source`.
    ///
    /// Companion to [`Self::delete_source`] for admin-driven source
    /// removal: even though the schema has `ON DELETE SET NULL` on the
    /// FK, calling this explicitly before [`Self::delete_source`] keeps
    /// the activity rows preserved for cross-source reporting (the
    /// retention path still prunes by `ts` per
    /// [`Self::prune_activity_older_than`]). Returns the number of rows
    /// touched.
    async fn delete_activity_by_source(&self, source: SourceId) -> Result<u64>;

    // --- settings -----------------------------------------------------------

    /// Reads a setting value (SPEC s22). Returns `None` if the key is
    /// absent. Values are JSON-typed per the schema's TEXT column.
    async fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>>;

    /// Writes a setting value, replacing any prior value at this key.
    async fn set_setting(&self, key: &str, value: &serde_json::Value) -> Result<()>;

    // --- search -------------------------------------------------------------

    /// Queries the `file_state_fts` virtual table (SPEC s2). When
    /// `source` is `Some`, restricts the search to that source; when
    /// `None`, searches across all sources. Caps the result at `limit`.
    async fn search_files(
        &self,
        source: Option<SourceId>,
        query: &str,
        limit: u32,
    ) -> Result<Vec<FileSearchHit>>;
}
