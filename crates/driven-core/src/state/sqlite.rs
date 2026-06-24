//! SQLite-backed [`StateRepo`] implementation (SPEC s2).
//!
//! Concrete implementation of the [`StateRepo`] trait declared in
//! [`crate::state`]. Opens the DB at `<config_dir>/driven/state.db` with WAL
//! mode + foreign keys on, runs the embedded migrations under
//! `src/migrations/`, and verifies `PRAGMA integrity_check` on first open
//! (returning `state.db_corrupt` per SPEC s24 when corruption is detected).
//!
//! Conventions:
//! - `sqlx::query!` (anonymous-record macro) is used so queries are
//!   compile-time-checked against the schema; the `.sqlx/` offline cache at
//!   the workspace root keeps CI green without a live DB.
//! - Newtype wrappers / enums (e.g. [`SourceId`], [`FileStateStatus`]) do not
//!   have `sqlx::Encode`/`Decode` impls; rows are reassembled by hand from
//!   the primitive `String` / `i64` / `Vec<u8>` columns sqlx returns.
//! - `INSERT ... ON CONFLICT DO UPDATE` is used for every upsert (never
//!   `INSERT OR REPLACE`) so the `ON DELETE CASCADE` chain on `accounts` /
//!   `backup_sources` does not nuke dependent rows on a benign re-upsert,
//!   and `file_state.rowid` stays stable for the external-content FTS index.
//! - [`RelativePath`] currently has a `todo!()` `TryFrom` (M2 lands the real
//!   validator); rows are deserialised via [`serde_json`] over the
//!   `#[serde(transparent)] String` shape so reads do not panic before then.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use uuid::Uuid;

use super::{
    AccountRow, ActivityFilter, ActivityLevel, ActivityPage, ActivityRow, ActivitySummary,
    FileSearchHit, FileStateRow, FileStatusCount, NewActivity, NewPendingOp, PageRequest,
    PendingOpRow, RestoreFileRow, SourceRow, StateRepo,
};
use crate::types::{
    AccountId, AccountState, ActivityId, FileStateStatus, PendingOpId, RelativePath, SourceId,
    UnixMs,
};

/// SQLite-backed [`StateRepo`] handle.
///
/// Wraps a [`SqlitePool`] over `<config_dir>/driven/state.db` (SPEC s2).
/// Cheap to clone via the inner pool's `Arc`-shaped handle.
#[derive(Debug, Clone)]
pub struct SqliteStateRepo {
    pool: SqlitePool,
}

impl SqliteStateRepo {
    /// Open (or create) the SQLite state DB at `path`, configure pragmas,
    /// run all embedded migrations, then verify integrity.
    ///
    /// Pragmas applied (DESIGN s5.6, SPEC s2):
    /// - `journal_mode = WAL` for concurrent reads with one writer.
    /// - `synchronous = NORMAL` (the WAL-mode-safe choice).
    /// - `foreign_keys = ON` so the schema's `ON DELETE CASCADE` chain works.
    /// - `busy_timeout = 5s` so the rare contended commit waits instead of
    ///   surfacing `SQLITE_BUSY` to the orchestrator.
    ///
    /// The pool is capped at a single connection (`max_connections = 1`).
    /// SQLite permits only one writer at a time regardless of pool size, and
    /// with a multi-connection pool the M3 concurrency=4 executor races
    /// produce a write-transaction upgrade deadlock that `busy_timeout` cannot
    /// resolve (it returns `SQLITE_BUSY_SNAPSHOT` immediately, not after the
    /// timeout). Serializing every statement through one connection makes
    /// `busy_timeout` fully effective and removes the deadlock; at this app's
    /// state-DB scale (a few MB, sub-millisecond statements) the lost read
    /// concurrency is immaterial.
    ///
    /// Surfaces [`crate::types::ErrorCode::StateDbCorrupt`] (as an
    /// `anyhow` error carrying the `state.db_corrupt` code prefix) when
    /// `PRAGMA integrity_check` returns anything other than `ok`.
    pub async fn open(path: &Path) -> Result<Self> {
        // `create_if_missing(true)` creates the DB FILE but not its parent
        // DIR. On first boot `<config_dir>/driven/` may not exist yet, so
        // create the parent tree before connecting (DESIGN s5.6 / SPEC s2).
        // `parent()` is `Some("")` for a bare filename; `create_dir_all("")`
        // errors, so only create when the parent is a non-empty path.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| anyhow!("create state dir {}: {e}", parent.display()))?;
            }
        }

        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;

        sqlx::migrate!("./src/migrations").run(&pool).await?;

        // Integrity check on every open (SPEC s24 state.db_corrupt). Cheap
        // for the V1 state.db (typically a few MB).
        let check: (String,) = sqlx::query_as("PRAGMA integrity_check;")
            .fetch_one(&pool)
            .await?;
        if check.0 != "ok" {
            return Err(anyhow!(
                "state.db_corrupt: PRAGMA integrity_check returned {}",
                check.0
            ));
        }

        Ok(Self { pool })
    }

    /// Borrow the underlying pool. Useful for advanced callers that want
    /// to run their own transaction; the trait surface is enough for
    /// every orchestrator path.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Internal helper backing both [`StateRepo::commit_create_result`] and
    /// [`StateRepo::commit_update_result`].
    ///
    /// Opens a single transaction, upserts the `file_state` row, deletes
    /// the `pending_ops` row, and commits. The `?` on each statement
    /// causes the implicit `tx` drop on the `Err` path to roll back both
    /// writes - DESIGN s5.6 step 3 atomicity.
    ///
    /// `expected_op_type` is the `pending_ops.op_type` this commit is
    /// finalizing (both callers finalize an UPLOAD, so they pass
    /// `"upload"`). The DELETE is bound to it so a stale/mismatched
    /// `op_id` that happens to exist for the SAME file under a DIFFERENT
    /// queued op (e.g. a `trash` or `verify`) cannot be deleted and have
    /// the upload result committed in its place (silent queue/state
    /// corruption).
    async fn commit_op_result_inner(
        &self,
        op_id: PendingOpId,
        expected_op_type: &str,
        file_state: &FileStateRow,
    ) -> Result<()> {
        let source_id = file_state.source_id.to_string();
        let relative_path = file_state.relative_path.as_str().to_string();
        let size = file_state.size as i64;
        let hash: &[u8] = &file_state.hash_blake3[..];
        let md5_owned: Option<Vec<u8>> = file_state.drive_md5.map(|m| m.to_vec());
        let md5: Option<&[u8]> = md5_owned.as_deref();
        let status = file_state_status_to_str(file_state.status);
        let op_id_v = op_id.0;

        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            r#"
            INSERT INTO file_state (
                source_id, relative_path, size, mtime_ns,
                hash_blake3, drive_file_id, drive_md5, encrypted_remote_path,
                status, last_uploaded_at, last_verified_at
            ) VALUES (
                ?1, ?2, ?3, ?4,
                ?5, ?6, ?7, ?8,
                ?9, ?10, ?11
            )
            ON CONFLICT(source_id, relative_path) DO UPDATE SET
                size                  = excluded.size,
                mtime_ns              = excluded.mtime_ns,
                hash_blake3           = excluded.hash_blake3,
                drive_file_id         = excluded.drive_file_id,
                drive_md5             = excluded.drive_md5,
                encrypted_remote_path = excluded.encrypted_remote_path,
                status                = excluded.status,
                last_uploaded_at      = excluded.last_uploaded_at,
                last_verified_at      = excluded.last_verified_at
            "#,
            source_id,
            relative_path,
            size,
            file_state.mtime_ns,
            hash,
            file_state.drive_file_id,
            md5,
            file_state.encrypted_remote_path,
            status,
            file_state.last_uploaded_at,
            file_state.last_verified_at,
        )
        .execute(&mut *tx)
        .await?;

        // DESIGN s5.6 step 3: the pending_op delete is the load-bearing
        // reconciliation invariant. A wrong/already-committed op_id must NOT
        // be allowed to commit `file_state` (and must not silently delete an
        // unrelated row). Bind the DELETE to the EXACT op this commit is
        // for - id AND (source_id, relative_path) AND op_type - so a
        // stale/mismatched op_id that happens to exist for a DIFFERENT file,
        // OR for the SAME file but a DIFFERENT queued op_type (e.g. a `trash`
        // or `verify`), cannot delete that unrelated pending_op and commit
        // the upload result in its place. Require the DELETE to affect
        // EXACTLY one row; otherwise return an `Err` so the implicit `tx`
        // drop rolls back the `file_state` upsert too.
        let deleted = sqlx::query!(
            "DELETE FROM pending_ops \
             WHERE id = ?1 AND source_id = ?2 AND relative_path = ?3 AND op_type = ?4",
            op_id_v,
            source_id,
            relative_path,
            expected_op_type,
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if deleted != 1 {
            return Err(anyhow!(
                "state.reconcile_op_missing: pending_op id {op_id_v} not found \
                 (DELETE affected {deleted} rows); refusing to commit file_state"
            ));
        }

        tx.commit().await?;
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// Encoding / decoding helpers between SQL primitives and the typed rows.
// -----------------------------------------------------------------------------

fn relative_path_from_string(s: String) -> Result<RelativePath> {
    // Route DB reads through the real validator+normalizer so a stored row
    // is held to the same invariants as a freshly-constructed path (NFC
    // normalization, no `..`/NUL/absolute). SPEC s24 local.unicode_collision
    // relies on `file_state.relative_path` being a stable NFC key.
    RelativePath::try_from(s).map_err(|e| anyhow!("invalid stored relative_path: {e}"))
}

fn account_state_to_str(s: AccountState) -> &'static str {
    match s {
        AccountState::Ok => "ok",
        AccountState::NeedsReauth => "needs_reauth",
        AccountState::Disabled => "disabled",
    }
}

fn account_state_from_str(s: &str) -> Result<AccountState> {
    match s {
        "ok" => Ok(AccountState::Ok),
        "needs_reauth" => Ok(AccountState::NeedsReauth),
        "disabled" => Ok(AccountState::Disabled),
        other => Err(anyhow!("invalid accounts.state value: {other}")),
    }
}

fn file_state_status_to_str(s: FileStateStatus) -> &'static str {
    match s {
        FileStateStatus::Synced => "synced",
        FileStateStatus::Pending => "pending",
        FileStateStatus::Corrupt => "corrupt",
        FileStateStatus::Locked => "locked",
        FileStateStatus::Error => "error",
        FileStateStatus::ExcludedOrphan => "excluded_orphan",
    }
}

fn file_state_status_from_str(s: &str) -> Result<FileStateStatus> {
    match s {
        "synced" => Ok(FileStateStatus::Synced),
        "pending" => Ok(FileStateStatus::Pending),
        "corrupt" => Ok(FileStateStatus::Corrupt),
        "locked" => Ok(FileStateStatus::Locked),
        "error" => Ok(FileStateStatus::Error),
        "excluded_orphan" => Ok(FileStateStatus::ExcludedOrphan),
        other => Err(anyhow!("invalid file_state.status value: {other}")),
    }
}

fn activity_level_to_str(l: ActivityLevel) -> &'static str {
    match l {
        ActivityLevel::Info => "info",
        ActivityLevel::Warn => "warn",
        ActivityLevel::Error => "error",
    }
}

fn activity_level_from_str(s: &str) -> Result<ActivityLevel> {
    match s {
        "info" => Ok(ActivityLevel::Info),
        "warn" => Ok(ActivityLevel::Warn),
        "error" => Ok(ActivityLevel::Error),
        other => Err(anyhow!("invalid activity_log.level value: {other}")),
    }
}

/// Numeric severity for `activity_level` ordering (`info < warn < error`).
/// Used for `ActivityFilter.min_level` filtering because the on-disk value
/// is TEXT and alphabetical ordering would put `error < info < warn`.
fn activity_level_rank(l: ActivityLevel) -> i64 {
    match l {
        ActivityLevel::Info => 0,
        ActivityLevel::Warn => 1,
        ActivityLevel::Error => 2,
    }
}

/// R2-P2-3: decode a SQLite INTEGER column that holds a logically-unsigned
/// count (`activity_log.file_count` / `bytes`) into a `u64`, rejecting a
/// negative stored value with a structured error instead of wrapping it with
/// `as`. A negative value is impossible from the (now `try_from`-guarded) write
/// path, so this only fires on external tampering or a row written before the
/// fix; rejecting it keeps a corrupt row from silently becoming a huge u64.
fn decode_nonneg_u64(v: i64) -> Result<u64> {
    u64::try_from(v)
        .map_err(|_| anyhow!("internal.bug: activity count column stored a negative value {v}"))
}

fn hash32_from_bytes(b: Vec<u8>) -> Result<[u8; 32]> {
    <[u8; 32]>::try_from(b.as_slice()).map_err(|_| anyhow!("hash_blake3 must be 32 bytes"))
}

fn md5_from_bytes(b: Option<Vec<u8>>) -> Result<Option<[u8; 16]>> {
    match b {
        None => Ok(None),
        Some(v) => <[u8; 16]>::try_from(v.as_slice())
            .map(Some)
            .map_err(|_| anyhow!("drive_md5 must be 16 bytes")),
    }
}

fn uuid_from_str(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| anyhow!("invalid uuid {s:?}: {e}"))
}

/// Build a safe FTS5 MATCH string from raw user input (see
/// [`StateRepo::search_files`] for the rationale).
///
/// Splits on whitespace and, per token, emits either a quoted literal
/// phrase (`"foo-bar"`) or - when the token ends in `*` with a non-empty
/// stem - a quoted prefix query (`"proj"*`). Internal double-quotes are
/// doubled so the phrase stays well-formed. A token that is only `*`
/// (empty stem) is dropped to avoid emitting `""*`, which FTS5 rejects.
/// Returns an empty `String` when the input has no usable tokens; the
/// caller treats that as "match nothing".
fn build_fts_match_query(query: &str) -> String {
    let mut terms: Vec<String> = Vec::new();
    for token in query.split_whitespace() {
        if token.ends_with('*') {
            // Trailing `*` (one or more) marks a prefix query. Collapse any
            // run of trailing `*` to a single prefix operator; a token that
            // is ALL `*` (e.g. `*` or `**`) has an empty stem and is dropped
            // rather than emitting an invalid `""*` that FTS5 would reject.
            let stem = token.trim_end_matches('*');
            if stem.is_empty() {
                continue;
            }
            terms.push(format!("\"{}\"*", stem.replace('"', "\"\"")));
        } else {
            terms.push(format!("\"{}\"", token.replace('"', "\"\"")));
        }
    }
    terms.join(" ")
}

// -----------------------------------------------------------------------------
// StateRepo impl.
// -----------------------------------------------------------------------------

#[async_trait]
impl StateRepo for SqliteStateRepo {
    // --- accounts -----------------------------------------------------------

    async fn list_accounts(&self) -> Result<Vec<AccountRow>> {
        let rows = sqlx::query!(
            r#"
            SELECT
                id                       AS "id!: String",
                email                    AS "email!: String",
                display_name             AS "display_name: String",
                state                    AS "state!: String",
                encryption_master_key_id AS "encryption_master_key_id: String",
                created_at               AS "created_at!: i64",
                last_synced_at           AS "last_synced_at: i64"
            FROM accounts
            ORDER BY created_at ASC, id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|r| {
                Ok(AccountRow {
                    id: AccountId(uuid_from_str(&r.id)?),
                    email: r.email,
                    display_name: r.display_name,
                    state: account_state_from_str(&r.state)?,
                    encryption_master_key_id: r.encryption_master_key_id,
                    created_at: r.created_at,
                    last_synced_at: r.last_synced_at,
                })
            })
            .collect()
    }

    async fn upsert_account(&self, row: &AccountRow) -> Result<()> {
        let id = row.id.to_string();
        let state = account_state_to_str(row.state);
        sqlx::query!(
            r#"
            INSERT INTO accounts (
                id, email, display_name, state,
                encryption_master_key_id, created_at, last_synced_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(id) DO UPDATE SET
                email                    = excluded.email,
                display_name             = excluded.display_name,
                state                    = excluded.state,
                encryption_master_key_id = excluded.encryption_master_key_id,
                created_at               = excluded.created_at,
                last_synced_at           = excluded.last_synced_at
            "#,
            id,
            row.email,
            row.display_name,
            state,
            row.encryption_master_key_id,
            row.created_at,
            row.last_synced_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_account_state(&self, id: AccountId, state: AccountState) -> Result<()> {
        let id_str = id.to_string();
        let state_str = account_state_to_str(state);
        sqlx::query!(
            "UPDATE accounts SET state = ?1 WHERE id = ?2",
            state_str,
            id_str,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn account_state(&self, id: AccountId) -> Result<Option<AccountState>> {
        let id_str = id.to_string();
        // Runtime `sqlx::query_as` (NOT the compile-checked `query!` macro) so
        // this additive read needs NO `.sqlx` cache regeneration.
        let row: Option<(String,)> = sqlx::query_as("SELECT state FROM accounts WHERE id = ?1")
            .bind(id_str.as_str())
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some((state,)) => Ok(Some(account_state_from_str(&state)?)),
            None => Ok(None),
        }
    }

    async fn mark_account_synced(&self, id: AccountId, at: UnixMs) -> Result<()> {
        let id_str = id.to_string();
        sqlx::query!(
            "UPDATE accounts SET last_synced_at = ?1 WHERE id = ?2",
            at,
            id_str,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_account(&self, id: AccountId) -> Result<()> {
        let id_str = id.to_string();
        sqlx::query!("DELETE FROM accounts WHERE id = ?1", id_str)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // --- backup_sources -----------------------------------------------------

    async fn list_sources(&self) -> Result<Vec<SourceRow>> {
        let rows = sqlx::query!(
            r#"
            SELECT
                id                        AS "id!: String",
                account_id                AS "account_id!: String",
                display_name              AS "display_name!: String",
                enabled                   AS "enabled!: i64",
                local_path                AS "local_path!: String",
                drive_folder_id           AS "drive_folder_id!: String",
                drive_folder_path         AS "drive_folder_path!: String",
                encryption_enabled        AS "encryption_enabled!: i64",
                wrapped_source_key        AS "wrapped_source_key: Vec<u8>",
                respect_gitignore         AS "respect_gitignore!: i64",
                include_patterns          AS "include_patterns!: String",
                exclude_patterns          AS "exclude_patterns!: String",
                schedule_json_v2_reserved AS "schedule_json_v2_reserved: String",
                deep_verify_interval_secs AS "deep_verify_interval_secs!: i64",
                last_full_scan_at         AS "last_full_scan_at: i64",
                last_deep_verify_at       AS "last_deep_verify_at: i64",
                created_at                AS "created_at!: i64"
            FROM backup_sources
            ORDER BY created_at ASC, id ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|r| {
                Ok(SourceRow {
                    id: SourceId(uuid_from_str(&r.id)?),
                    account_id: AccountId(uuid_from_str(&r.account_id)?),
                    display_name: r.display_name,
                    enabled: r.enabled != 0,
                    local_path: r.local_path,
                    drive_folder_id: r.drive_folder_id,
                    drive_folder_path: r.drive_folder_path,
                    encryption_enabled: r.encryption_enabled != 0,
                    wrapped_source_key: r.wrapped_source_key,
                    respect_gitignore: r.respect_gitignore != 0,
                    include_patterns: serde_json::from_str(&r.include_patterns)?,
                    exclude_patterns: serde_json::from_str(&r.exclude_patterns)?,
                    schedule_json_v2_reserved: r.schedule_json_v2_reserved,
                    deep_verify_interval_secs: u32::try_from(r.deep_verify_interval_secs)
                        .map_err(|_| anyhow!("deep_verify_interval_secs out of u32 range"))?,
                    last_full_scan_at: r.last_full_scan_at,
                    last_deep_verify_at: r.last_deep_verify_at,
                    created_at: r.created_at,
                })
            })
            .collect()
    }

    async fn list_enabled_sources_for(&self, account: AccountId) -> Result<Vec<SourceRow>> {
        let account_str = account.to_string();
        let rows = sqlx::query!(
            r#"
            SELECT
                id                        AS "id!: String",
                account_id                AS "account_id!: String",
                display_name              AS "display_name!: String",
                enabled                   AS "enabled!: i64",
                local_path                AS "local_path!: String",
                drive_folder_id           AS "drive_folder_id!: String",
                drive_folder_path         AS "drive_folder_path!: String",
                encryption_enabled        AS "encryption_enabled!: i64",
                wrapped_source_key        AS "wrapped_source_key: Vec<u8>",
                respect_gitignore         AS "respect_gitignore!: i64",
                include_patterns          AS "include_patterns!: String",
                exclude_patterns          AS "exclude_patterns!: String",
                schedule_json_v2_reserved AS "schedule_json_v2_reserved: String",
                deep_verify_interval_secs AS "deep_verify_interval_secs!: i64",
                last_full_scan_at         AS "last_full_scan_at: i64",
                last_deep_verify_at       AS "last_deep_verify_at: i64",
                created_at                AS "created_at!: i64"
            FROM backup_sources
            WHERE account_id = ?1 AND enabled = 1
            ORDER BY created_at ASC, id ASC
            "#,
            account_str,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|r| {
                Ok(SourceRow {
                    id: SourceId(uuid_from_str(&r.id)?),
                    account_id: AccountId(uuid_from_str(&r.account_id)?),
                    display_name: r.display_name,
                    enabled: r.enabled != 0,
                    local_path: r.local_path,
                    drive_folder_id: r.drive_folder_id,
                    drive_folder_path: r.drive_folder_path,
                    encryption_enabled: r.encryption_enabled != 0,
                    wrapped_source_key: r.wrapped_source_key,
                    respect_gitignore: r.respect_gitignore != 0,
                    include_patterns: serde_json::from_str(&r.include_patterns)?,
                    exclude_patterns: serde_json::from_str(&r.exclude_patterns)?,
                    schedule_json_v2_reserved: r.schedule_json_v2_reserved,
                    deep_verify_interval_secs: u32::try_from(r.deep_verify_interval_secs)
                        .map_err(|_| anyhow!("deep_verify_interval_secs out of u32 range"))?,
                    last_full_scan_at: r.last_full_scan_at,
                    last_deep_verify_at: r.last_deep_verify_at,
                    created_at: r.created_at,
                })
            })
            .collect()
    }

    async fn upsert_source(&self, row: &SourceRow) -> Result<()> {
        let id = row.id.to_string();
        let account_id = row.account_id.to_string();
        let enabled = row.enabled as i64;
        let encryption_enabled = row.encryption_enabled as i64;
        let respect_gitignore = row.respect_gitignore as i64;
        let include_patterns = serde_json::to_string(&row.include_patterns)?;
        let exclude_patterns = serde_json::to_string(&row.exclude_patterns)?;
        let wrapped: Option<&[u8]> = row.wrapped_source_key.as_deref();
        let deep_verify_interval_secs = row.deep_verify_interval_secs as i64;

        sqlx::query!(
            r#"
            INSERT INTO backup_sources (
                id, account_id, display_name, enabled,
                local_path, drive_folder_id, drive_folder_path,
                encryption_enabled, wrapped_source_key, respect_gitignore,
                include_patterns, exclude_patterns, schedule_json_v2_reserved,
                deep_verify_interval_secs, last_full_scan_at, last_deep_verify_at,
                created_at
            ) VALUES (
                ?1, ?2, ?3, ?4,
                ?5, ?6, ?7,
                ?8, ?9, ?10,
                ?11, ?12, ?13,
                ?14, ?15, ?16,
                ?17
            )
            ON CONFLICT(id) DO UPDATE SET
                account_id                = excluded.account_id,
                display_name              = excluded.display_name,
                enabled                   = excluded.enabled,
                local_path                = excluded.local_path,
                drive_folder_id           = excluded.drive_folder_id,
                drive_folder_path         = excluded.drive_folder_path,
                encryption_enabled        = excluded.encryption_enabled,
                wrapped_source_key        = excluded.wrapped_source_key,
                respect_gitignore         = excluded.respect_gitignore,
                include_patterns          = excluded.include_patterns,
                exclude_patterns          = excluded.exclude_patterns,
                schedule_json_v2_reserved = excluded.schedule_json_v2_reserved,
                deep_verify_interval_secs = excluded.deep_verify_interval_secs,
                last_full_scan_at         = excluded.last_full_scan_at,
                last_deep_verify_at       = excluded.last_deep_verify_at,
                created_at                = excluded.created_at
            "#,
            id,
            account_id,
            row.display_name,
            enabled,
            row.local_path,
            row.drive_folder_id,
            row.drive_folder_path,
            encryption_enabled,
            wrapped,
            respect_gitignore,
            include_patterns,
            exclude_patterns,
            row.schedule_json_v2_reserved,
            deep_verify_interval_secs,
            row.last_full_scan_at,
            row.last_deep_verify_at,
            row.created_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn insert_source_with_optional_master_key_stamp(
        &self,
        source: &SourceRow,
        stamp_master_key_id: Option<(AccountId, String)>,
    ) -> Result<()> {
        // R1-P1-1: do the (optional) account master-key stamp AND the source
        // insert in ONE transaction so a source-insert failure cannot leave the
        // account stamped-but-phraseless (an unrestorable encrypted backup).
        let mut tx = self.pool.begin().await?;

        if let Some((account_id, master_key_id)) = stamp_master_key_id.as_ref() {
            let account_id = account_id.to_string();
            // R2-P1-1: a COMPARE-AND-SET stamp - update the master-key column ONLY
            // when it is still NULL. A targeted column update (not a full-row
            // replace) so the stamp cannot clobber any other account column, AND
            // the `IS NULL` guard means a concurrent writer that already stamped a
            // (possibly different) key is NEVER overwritten. Affecting one row =
            // we won the stamp; affecting zero needs disambiguation below.
            let affected = sqlx::query!(
                "UPDATE accounts SET encryption_master_key_id = ?1 \
                 WHERE id = ?2 AND encryption_master_key_id IS NULL",
                master_key_id,
                account_id,
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if affected == 0 {
                // Zero rows updated: either the account vanished, or its
                // master-key column was ALREADY set by a concurrent stamp (the
                // CAS lost the race). Read the current value to decide.
                let current = sqlx::query!(
                    r#"SELECT encryption_master_key_id AS "mk: String" FROM accounts WHERE id = ?1"#,
                    account_id,
                )
                .fetch_optional(&mut *tx)
                .await?;
                match current {
                    // Account gone: roll back rather than insert a phantom-keyed
                    // orphan.
                    None => {
                        tx.rollback().await?;
                        return Err(anyhow!(
                            "account not found for master-key stamp; transaction rolled back"
                        ));
                    }
                    // Already stamped with the SAME key id: idempotent, proceed
                    // (the source row still needs inserting under this key).
                    Some(row) if row.mk.as_deref() == Some(master_key_id.as_str()) => {}
                    // Already stamped with a DIFFERENT key id: the caller generated
                    // a divergent master key (a lost CAS race WITHOUT the per-
                    // account lock). Refuse - committing would wrap this source
                    // under a key the account does not record, leaving it
                    // unrestorable. Roll back so the caller rolls back its key.
                    Some(_) => {
                        tx.rollback().await?;
                        return Err(anyhow!(
                            "account master key already set by a concurrent writer; \
                             refusing to stamp a divergent key (transaction rolled back)"
                        ));
                    }
                }
            }
        }

        // Insert/replace the source row inside the same transaction.
        let id = source.id.to_string();
        let account_id = source.account_id.to_string();
        let enabled = source.enabled as i64;
        let encryption_enabled = source.encryption_enabled as i64;
        let respect_gitignore = source.respect_gitignore as i64;
        let include_patterns = serde_json::to_string(&source.include_patterns)?;
        let exclude_patterns = serde_json::to_string(&source.exclude_patterns)?;
        let wrapped: Option<&[u8]> = source.wrapped_source_key.as_deref();
        let deep_verify_interval_secs = source.deep_verify_interval_secs as i64;

        sqlx::query!(
            r#"
            INSERT INTO backup_sources (
                id, account_id, display_name, enabled,
                local_path, drive_folder_id, drive_folder_path,
                encryption_enabled, wrapped_source_key, respect_gitignore,
                include_patterns, exclude_patterns, schedule_json_v2_reserved,
                deep_verify_interval_secs, last_full_scan_at, last_deep_verify_at,
                created_at
            ) VALUES (
                ?1, ?2, ?3, ?4,
                ?5, ?6, ?7,
                ?8, ?9, ?10,
                ?11, ?12, ?13,
                ?14, ?15, ?16,
                ?17
            )
            ON CONFLICT(id) DO UPDATE SET
                account_id                = excluded.account_id,
                display_name              = excluded.display_name,
                enabled                   = excluded.enabled,
                local_path                = excluded.local_path,
                drive_folder_id           = excluded.drive_folder_id,
                drive_folder_path         = excluded.drive_folder_path,
                encryption_enabled        = excluded.encryption_enabled,
                wrapped_source_key        = excluded.wrapped_source_key,
                respect_gitignore         = excluded.respect_gitignore,
                include_patterns          = excluded.include_patterns,
                exclude_patterns          = excluded.exclude_patterns,
                schedule_json_v2_reserved = excluded.schedule_json_v2_reserved,
                deep_verify_interval_secs = excluded.deep_verify_interval_secs,
                last_full_scan_at         = excluded.last_full_scan_at,
                last_deep_verify_at       = excluded.last_deep_verify_at,
                created_at                = excluded.created_at
            "#,
            id,
            account_id,
            source.display_name,
            enabled,
            source.local_path,
            source.drive_folder_id,
            source.drive_folder_path,
            encryption_enabled,
            wrapped,
            respect_gitignore,
            include_patterns,
            exclude_patterns,
            source.schedule_json_v2_reserved,
            deep_verify_interval_secs,
            source.last_full_scan_at,
            source.last_deep_verify_at,
            source.created_at,
        )
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn mark_source_scanned(
        &self,
        id: SourceId,
        full_scan_at: UnixMs,
        deep_verify_at: Option<UnixMs>,
    ) -> Result<()> {
        let id_str = id.to_string();
        // `last_full_scan_at` is always advanced; `last_deep_verify_at` is only
        // bumped when this was a deep-verify cycle (`deep_verify_at` is `Some`)
        // - a `None` bind leaves the existing value via COALESCE so a plain
        // fast-path scan never resets the verify cadence.
        sqlx::query!(
            r#"
            UPDATE backup_sources
            SET last_full_scan_at   = ?1,
                last_deep_verify_at = COALESCE(?2, last_deep_verify_at)
            WHERE id = ?3
            "#,
            full_scan_at,
            deep_verify_at,
            id_str,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_source(&self, id: SourceId) -> Result<()> {
        let id_str = id.to_string();
        sqlx::query!("DELETE FROM backup_sources WHERE id = ?1", id_str)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // --- file_state ---------------------------------------------------------

    async fn load_source_file_state(
        &self,
        source: SourceId,
    ) -> Result<HashMap<RelativePath, FileStateRow>> {
        let source_str = source.to_string();
        let rows = sqlx::query!(
            r#"
            SELECT
                source_id             AS "source_id!: String",
                relative_path         AS "relative_path!: String",
                size                  AS "size!: i64",
                mtime_ns              AS "mtime_ns!: i64",
                hash_blake3           AS "hash_blake3!: Vec<u8>",
                drive_file_id         AS "drive_file_id: String",
                drive_md5             AS "drive_md5: Vec<u8>",
                encrypted_remote_path AS "encrypted_remote_path: String",
                status                AS "status!: String",
                last_uploaded_at      AS "last_uploaded_at: i64",
                last_verified_at      AS "last_verified_at: i64"
            FROM file_state
            WHERE source_id = ?1
            "#,
            source_str,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let path = relative_path_from_string(r.relative_path)?;
            let row = FileStateRow {
                source_id: SourceId(uuid_from_str(&r.source_id)?),
                relative_path: path.clone(),
                size: r.size as u64,
                mtime_ns: r.mtime_ns,
                hash_blake3: hash32_from_bytes(r.hash_blake3)?,
                drive_file_id: r.drive_file_id,
                drive_md5: md5_from_bytes(r.drive_md5)?,
                encrypted_remote_path: r.encrypted_remote_path,
                status: file_state_status_from_str(&r.status)?,
                last_uploaded_at: r.last_uploaded_at,
                last_verified_at: r.last_verified_at,
            };
            out.insert(path, row);
        }
        Ok(out)
    }

    async fn get_file_state(
        &self,
        source: SourceId,
        path: &RelativePath,
    ) -> Result<Option<FileStateRow>> {
        let source_str = source.to_string();
        let path_str = path.as_str().to_string();
        let opt = sqlx::query!(
            r#"
            SELECT
                source_id             AS "source_id!: String",
                relative_path         AS "relative_path!: String",
                size                  AS "size!: i64",
                mtime_ns              AS "mtime_ns!: i64",
                hash_blake3           AS "hash_blake3!: Vec<u8>",
                drive_file_id         AS "drive_file_id: String",
                drive_md5             AS "drive_md5: Vec<u8>",
                encrypted_remote_path AS "encrypted_remote_path: String",
                status                AS "status!: String",
                last_uploaded_at      AS "last_uploaded_at: i64",
                last_verified_at      AS "last_verified_at: i64"
            FROM file_state
            WHERE source_id = ?1 AND relative_path = ?2
            "#,
            source_str,
            path_str,
        )
        .fetch_optional(&self.pool)
        .await?;

        let Some(r) = opt else { return Ok(None) };
        let rp = relative_path_from_string(r.relative_path)?;
        Ok(Some(FileStateRow {
            source_id: SourceId(uuid_from_str(&r.source_id)?),
            relative_path: rp,
            size: r.size as u64,
            mtime_ns: r.mtime_ns,
            hash_blake3: hash32_from_bytes(r.hash_blake3)?,
            drive_file_id: r.drive_file_id,
            drive_md5: md5_from_bytes(r.drive_md5)?,
            encrypted_remote_path: r.encrypted_remote_path,
            status: file_state_status_from_str(&r.status)?,
            last_uploaded_at: r.last_uploaded_at,
            last_verified_at: r.last_verified_at,
        }))
    }

    async fn upsert_file_state(&self, row: &FileStateRow) -> Result<()> {
        let source_id = row.source_id.to_string();
        let relative_path = row.relative_path.as_str().to_string();
        let size = row.size as i64;
        let hash: &[u8] = &row.hash_blake3[..];
        let md5_owned: Option<Vec<u8>> = row.drive_md5.map(|m| m.to_vec());
        let md5: Option<&[u8]> = md5_owned.as_deref();
        let status = file_state_status_to_str(row.status);

        sqlx::query!(
            r#"
            INSERT INTO file_state (
                source_id, relative_path, size, mtime_ns,
                hash_blake3, drive_file_id, drive_md5, encrypted_remote_path,
                status, last_uploaded_at, last_verified_at
            ) VALUES (
                ?1, ?2, ?3, ?4,
                ?5, ?6, ?7, ?8,
                ?9, ?10, ?11
            )
            ON CONFLICT(source_id, relative_path) DO UPDATE SET
                size                  = excluded.size,
                mtime_ns              = excluded.mtime_ns,
                hash_blake3           = excluded.hash_blake3,
                drive_file_id         = excluded.drive_file_id,
                drive_md5             = excluded.drive_md5,
                encrypted_remote_path = excluded.encrypted_remote_path,
                status                = excluded.status,
                last_uploaded_at      = excluded.last_uploaded_at,
                last_verified_at      = excluded.last_verified_at
            "#,
            source_id,
            relative_path,
            size,
            row.mtime_ns,
            hash,
            row.drive_file_id,
            md5,
            row.encrypted_remote_path,
            status,
            row.last_uploaded_at,
            row.last_verified_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_file_state(&self, source: SourceId, path: &RelativePath) -> Result<()> {
        let source_str = source.to_string();
        let path_str = path.as_str().to_string();
        sqlx::query!(
            "DELETE FROM file_state WHERE source_id = ?1 AND relative_path = ?2",
            source_str,
            path_str,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn clear_file_state_drive_file_id(
        &self,
        source: SourceId,
        path: &RelativePath,
    ) -> Result<()> {
        let source_str = source.to_string();
        let path_str = path.as_str().to_string();
        // Runtime `sqlx::query` (NOT the compile-checked `query!` macro) so this
        // additive method needs NO `.sqlx` cache regeneration (R3-P1-2). A
        // targeted single-column UPDATE: it cannot clobber any other column a
        // concurrent edit may have changed, and a missing row updates 0 rows
        // (idempotent no-op).
        sqlx::query(
            "UPDATE file_state SET drive_file_id = NULL \
             WHERE source_id = ?1 AND relative_path = ?2",
        )
        .bind(source_str.as_str())
        .bind(path_str.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn bump_checksum_mismatch_count(
        &self,
        source: SourceId,
        path: &RelativePath,
    ) -> Result<u32> {
        let source_str = source.to_string();
        let path_str = path.as_str().to_string();
        // One transaction: upsert-increment, then read the new count back. Two
        // statements (no `RETURNING`) so the query shapes match the rest of this
        // module's proven patterns; the transaction makes the RMW atomic against
        // any concurrent bump. Runtime `sqlx::query` / `sqlx::query_as` (NOT the
        // compile-checked `query!` macro) so the new table needs NO `.sqlx` cache
        // regeneration - only the additive 0003 migration.
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO file_checksum_mismatch (source_id, relative_path, count) \
             VALUES (?1, ?2, 1) \
             ON CONFLICT(source_id, relative_path) \
             DO UPDATE SET count = count + 1",
        )
        .bind(source_str.as_str())
        .bind(path_str.as_str())
        .execute(&mut *tx)
        .await?;
        let (count,): (i64,) = sqlx::query_as(
            "SELECT count FROM file_checksum_mismatch \
             WHERE source_id = ?1 AND relative_path = ?2",
        )
        .bind(source_str.as_str())
        .bind(path_str.as_str())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(u32::try_from(count.max(0)).unwrap_or(u32::MAX))
    }

    async fn clear_checksum_mismatch_count(
        &self,
        source: SourceId,
        path: &RelativePath,
    ) -> Result<()> {
        let source_str = source.to_string();
        let path_str = path.as_str().to_string();
        sqlx::query(
            "DELETE FROM file_checksum_mismatch WHERE source_id = ?1 AND relative_path = ?2",
        )
        .bind(source_str.as_str())
        .bind(path_str.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_excluded_orphans(&self, source: SourceId, paths: &[RelativePath]) -> Result<u64> {
        if paths.is_empty() {
            return Ok(0);
        }
        let source_str = source.to_string();
        // One transaction, one compile-time-checked UPDATE per path. A
        // per-path loop keeps the `query!` macro static (no dynamic IN-list
        // that would defeat offline sqlx) while still being atomic: the
        // implicit `tx` drop on any `?` rolls back every prior UPDATE.
        let mut tx = self.pool.begin().await?;
        let mut updated: u64 = 0;
        for path in paths {
            let path_str = path.as_str().to_string();
            updated += sqlx::query!(
                "UPDATE file_state SET status = 'excluded_orphan' \
                 WHERE source_id = ?1 AND relative_path = ?2",
                source_str,
                path_str,
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        Ok(updated)
    }

    // --- pending_ops --------------------------------------------------------

    async fn enqueue_pending_op(&self, row: NewPendingOp) -> Result<PendingOpId> {
        let source_id = row.source_id.to_string();
        let relative_path = row.relative_path.as_str().to_string();
        let payload_json = serde_json::to_string(&row.payload_json)?;
        let result = sqlx::query!(
            r#"
            INSERT INTO pending_ops (
                source_id, op_type, relative_path, payload_json,
                attempts, last_error, scheduled_for, created_at
            ) VALUES (?1, ?2, ?3, ?4, 0, NULL, ?5, ?6)
            "#,
            source_id,
            row.op_type,
            relative_path,
            payload_json,
            row.scheduled_for,
            row.created_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(PendingOpId(result.last_insert_rowid()))
    }

    async fn get_pending_ops_due(&self, now_ms: UnixMs, limit: u32) -> Result<Vec<PendingOpRow>> {
        let limit_i = limit as i64;
        let rows = sqlx::query!(
            r#"
            SELECT
                id            AS "id!: i64",
                source_id     AS "source_id!: String",
                op_type       AS "op_type!: String",
                relative_path AS "relative_path!: String",
                payload_json  AS "payload_json!: String",
                attempts      AS "attempts!: i64",
                last_error    AS "last_error: String",
                scheduled_for AS "scheduled_for!: i64",
                created_at    AS "created_at!: i64"
            FROM pending_ops
            WHERE scheduled_for <= ?1
            ORDER BY scheduled_for ASC, id ASC
            LIMIT ?2
            "#,
            now_ms,
            limit_i,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|r| {
                Ok(PendingOpRow {
                    id: PendingOpId(r.id),
                    source_id: SourceId(uuid_from_str(&r.source_id)?),
                    op_type: r.op_type,
                    relative_path: relative_path_from_string(r.relative_path)?,
                    payload_json: serde_json::from_str(&r.payload_json)?,
                    attempts: r.attempts as u32,
                    last_error: r.last_error,
                    scheduled_for: r.scheduled_for,
                    created_at: r.created_at,
                })
            })
            .collect()
    }

    async fn mark_pending_op_attempted(
        &self,
        id: PendingOpId,
        error: Option<&str>,
        next_attempt_ms: UnixMs,
    ) -> Result<()> {
        let id_v = id.0;
        sqlx::query!(
            r#"
            UPDATE pending_ops
               SET attempts = attempts + 1,
                   last_error = ?2,
                   scheduled_for = ?3
             WHERE id = ?1
            "#,
            id_v,
            error,
            next_attempt_ms,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_pending_op(&self, id: PendingOpId) -> Result<()> {
        let id_v = id.0;
        sqlx::query!("DELETE FROM pending_ops WHERE id = ?1", id_v)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_pending_op_payload(
        &self,
        id: PendingOpId,
        payload_json: &serde_json::Value,
    ) -> Result<()> {
        let id_v = id.0;
        let payload = serde_json::to_string(payload_json)?;
        let result = sqlx::query!(
            "UPDATE pending_ops SET payload_json = ?1 WHERE id = ?2",
            payload,
            id_v,
        )
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            anyhow::bail!("state.update_pending_op_payload: pending_op id {id_v} not found");
        }
        Ok(())
    }

    // --- activity_log -------------------------------------------------------

    async fn write_activity(&self, row: NewActivity) -> Result<ActivityId> {
        let source_id = row.source_id.map(|s| s.to_string());
        let level = activity_level_to_str(row.level);
        // R2-P2-3: encode `file_count` / `bytes` with `i64::try_from` (not `as`).
        // SQLite has no unsigned ints; a u64 above `i64::MAX` would wrap negative
        // with `as`, then summaries would clamp it to 0 (silent corruption). A
        // value that large is not a real backup count, so REJECT it with a
        // structured `internal.bad_request` rather than store a wrapped row.
        let file_count: Option<i64> = match row.file_count {
            None => None,
            Some(v) => Some(i64::try_from(v).map_err(|_| {
                anyhow!("internal.bad_request: activity file_count {v} exceeds i64::MAX")
            })?),
        };
        let bytes: Option<i64> = match row.bytes {
            None => None,
            Some(v) => Some(i64::try_from(v).map_err(|_| {
                anyhow!("internal.bad_request: activity bytes {v} exceeds i64::MAX")
            })?),
        };
        let result = sqlx::query!(
            r#"
            INSERT INTO activity_log (
                ts, source_id, level, event_type, file_count, bytes, message
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            row.ts,
            source_id,
            level,
            row.event_type,
            file_count,
            bytes,
            row.message,
        )
        .execute(&self.pool)
        .await?;
        Ok(ActivityId(result.last_insert_rowid()))
    }

    async fn query_activity(
        &self,
        filter: ActivityFilter,
        page: PageRequest,
    ) -> Result<ActivityPage> {
        // The combined predicate is the AND of all populated filter fields,
        // pushed down to SQL so paging + the total count stay correct.
        //
        // `event_types`: a dynamic IN-list. Rather than build the query at
        // runtime (losing sqlx's compile-time column typing), encode the
        // list as a JSON array and match against `json_each` - this keeps a
        // single static bind and applies the filter in BOTH the page query
        // and the count, so pagination counts cannot lie (M7's activity UI
        // depends on this).
        //
        // `min_level`: TEXT compare on `level` is alphabetical and would
        // sort `error < info < warn`. Compare numeric rank instead.
        let source_id = filter.source_id.map(|s| s.to_string());
        let since = filter.since_ms;
        let before = filter.before_ms;
        let min_rank = filter.min_level.map(activity_level_rank);
        // `None` => no event_type filter; `Some(json)` => filter to the set.
        let event_types_json: Option<String> = if filter.event_types.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&filter.event_types)?)
        };
        // SPEC s18.8: `limit` must be in 1..=10_000; a value outside that
        // range is a caller bug and is REJECTED with a structured
        // `internal.bad_request` (the prior code silently clamped, which
        // accepted `0` / `u32::MAX` as if valid).
        if !(1..=10_000).contains(&page.limit) {
            return Err(anyhow!(
                "internal.bad_request: activity limit must be 1..=10000, got {}",
                page.limit
            ));
        }
        let limit_i = page.limit as i64;
        // R2-P1-2: KEYSET cursor. The cursor `(before_ts, before_id)` is the
        // OLDEST `(ts, id)` the caller already holds; this page is the rows
        // STRICTLY older than it in `(ts DESC, id DESC)` order, so a row
        // prepended to `activity_log` between page fetches can never shift /
        // skip an already-paged row. `before_ts` / `before_id` are both `Some`
        // (a continuation page) or both `None` (the first page). The composite
        // predicate is `ts < before_ts OR (ts = before_ts AND id < before_id)`,
        // pushed into SQL so the `(ts, id)` total order is honoured at the DB.
        if page.before_ts.is_some() != page.before_id.is_some() {
            return Err(anyhow!(
                "internal.bad_request: activity cursor before_ts and before_id must be set together"
            ));
        }
        let before_ts = page.before_ts;
        let before_id = page.before_id;

        let rows = sqlx::query!(
            r#"
            SELECT
                id         AS "id!: i64",
                ts         AS "ts!: i64",
                source_id  AS "source_id: String",
                level      AS "level!: String",
                event_type AS "event_type!: String",
                file_count AS "file_count: i64",
                bytes      AS "bytes: i64",
                message    AS "message: String"
            FROM activity_log
            WHERE (?1 IS NULL OR source_id = ?1)
              AND (?2 IS NULL OR ts >= ?2)
              AND (?3 IS NULL OR ts <  ?3)
              AND (?4 IS NULL OR CASE level
                                    WHEN 'info'  THEN 0
                                    WHEN 'warn'  THEN 1
                                    WHEN 'error' THEN 2
                                    ELSE -1
                                 END >= ?4)
              AND (?5 IS NULL
                   OR event_type IN (SELECT value FROM json_each(?5)))
              AND (?6 IS NULL
                   OR ts < ?6
                   OR (ts = ?6 AND id < ?7))
            ORDER BY ts DESC, id DESC
            LIMIT ?8
            "#,
            source_id,
            since,
            before,
            min_rank,
            event_types_json,
            before_ts,
            before_id,
            limit_i,
        )
        .fetch_all(&self.pool)
        .await?;
        // KEYSET `has_more`: a full page MAY have more older rows; a short page
        // is the tail. The store re-queries with the last row as cursor, so a
        // false-positive (an exactly-full final page) costs only one extra
        // empty fetch, never a skipped row.
        let has_more = rows.len() as i64 == limit_i;

        let total = sqlx::query!(
            r#"
            SELECT COUNT(*) AS "total!: i64"
            FROM activity_log
            WHERE (?1 IS NULL OR source_id = ?1)
              AND (?2 IS NULL OR ts >= ?2)
              AND (?3 IS NULL OR ts <  ?3)
              AND (?4 IS NULL OR CASE level
                                    WHEN 'info'  THEN 0
                                    WHEN 'warn'  THEN 1
                                    WHEN 'error' THEN 2
                                    ELSE -1
                                 END >= ?4)
              AND (?5 IS NULL
                   OR event_type IN (SELECT value FROM json_each(?5)))
            "#,
            source_id,
            since,
            before,
            min_rank,
            event_types_json,
        )
        .fetch_one(&self.pool)
        .await?
        .total;

        let mut decoded = Vec::with_capacity(rows.len());
        for r in rows {
            let parsed_source = match r.source_id {
                None => None,
                Some(s) => Some(SourceId(uuid_from_str(&s)?)),
            };
            let event_type = r.event_type;
            // R2-P2-3: decode `file_count` / `bytes` with `try_from` (not `as`).
            // A negative stored value (only possible via external tampering or a
            // pre-fix wrapped write) is rejected with a structured error rather
            // than wrapping to a huge u64.
            let file_count = r.file_count.map(decode_nonneg_u64).transpose()?;
            let bytes = r.bytes.map(decode_nonneg_u64).transpose()?;
            decoded.push(ActivityRow {
                id: ActivityId(r.id),
                ts: r.ts,
                source_id: parsed_source,
                level: activity_level_from_str(&r.level)?,
                event_type,
                file_count,
                bytes,
                message: r.message,
            });
        }

        Ok(ActivityPage {
            rows: decoded,
            total: u64::try_from(total).unwrap_or(0),
            has_more,
        })
    }

    async fn distinct_activity_event_types(&self) -> Result<Vec<String>> {
        // M7-P2-4: the DISTINCT event_type set so the dashboard filter dropdown
        // can offer types present in history but not in the loaded rows. Sorted
        // ascending for a stable UI order.
        let rows = sqlx::query!(
            r#"
            SELECT DISTINCT event_type AS "event_type!: String"
            FROM activity_log
            ORDER BY event_type ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.event_type).collect())
    }

    async fn activity_summary(
        &self,
        day_start_ms: UnixMs,
        week_start_ms: UnixMs,
        throughput_window_start_ms: UnixMs,
        throughput_window_ms: u64,
    ) -> Result<ActivitySummary> {
        // M7-P2-5 (DESIGN s8.3 header aggregates). Three bounded sums over
        // `activity_log.bytes` (today / this week / recent throughput window)
        // plus a GROUP BY over `file_state.status`. `bytes` is nullable and
        // SUM over no rows is NULL, so COALESCE to 0. The caller passes every
        // time boundary so this is deterministic + testable.
        //
        // SQLite has no unsigned ints; bytes are stored as INTEGER and summed
        // into i64. A negative byte count is impossible (only non-negative
        // upload sizes are written), but the decode below clamps via `max(0)`
        // as defence-in-depth before the lossless cast to u64.
        //
        // R1-P2-2: the outer row filter MUST cover the EARLIEST of the three
        // window starts, not just `week_start`. Near the start of a week the
        // throughput window (e.g. last 60 min) can begin BEFORE `week_start`;
        // an outer `WHERE ts >= week_start` would drop those rows before the
        // per-sum CASE ran, undercounting throughput at week boundaries. Gate
        // by `MIN(day, week, throughput)` so every CASE owns its own window.
        let byte_sums = sqlx::query!(
            r#"
            SELECT
                COALESCE(SUM(CASE WHEN ts >= ?1 THEN bytes ELSE 0 END), 0) AS "today!: i64",
                COALESCE(SUM(CASE WHEN ts >= ?2 THEN bytes ELSE 0 END), 0) AS "week!: i64",
                COALESCE(SUM(CASE WHEN ts >= ?3 THEN bytes ELSE 0 END), 0) AS "window!: i64"
            FROM activity_log
            WHERE ts >= MIN(?1, ?2, ?3)
            "#,
            day_start_ms,
            week_start_ms,
            throughput_window_start_ms,
        )
        .fetch_one(&self.pool)
        .await?;

        let status_rows = sqlx::query!(
            r#"
            SELECT
                status      AS "status!: String",
                COUNT(*)    AS "count!: i64"
            FROM file_state
            GROUP BY status
            ORDER BY status ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut file_status_counts = Vec::with_capacity(status_rows.len());
        for r in status_rows {
            file_status_counts.push(FileStatusCount {
                status: file_state_status_from_str(&r.status)?,
                count: r.count.max(0) as u64,
            });
        }

        Ok(ActivitySummary {
            bytes_today: byte_sums.today.max(0) as u64,
            bytes_week: byte_sums.week.max(0) as u64,
            file_status_counts,
            throughput_window_bytes: byte_sums.window.max(0) as u64,
            throughput_window_ms,
        })
    }

    async fn prune_activity_older_than(
        &self,
        before_ms: UnixMs,
        hard_cap: u64,
        batch_size: Option<u32>,
    ) -> Result<u64> {
        // DESIGN s18.4: prune in batches so a catastrophic-growth prune
        // does not hold a single transaction over 1B rows. Stop when a
        // round deletes fewer than `batch_size` (no more eligible) or
        // when the cumulative total reaches `hard_cap`. After the loop
        // runs `PRAGMA wal_checkpoint(TRUNCATE)` so freed pages do not
        // sit in the WAL.
        let batch = batch_size.unwrap_or(10_000).max(1) as u64;
        let mut total: u64 = 0;
        loop {
            if total >= hard_cap {
                break;
            }
            let this_round = batch.min(hard_cap - total);
            let limit_i = i64::try_from(this_round).unwrap_or(i64::MAX);
            let deleted = sqlx::query!(
                r#"
                DELETE FROM activity_log
                 WHERE id IN (
                    SELECT id FROM activity_log
                     WHERE ts < ?1
                     ORDER BY ts ASC, id ASC
                     LIMIT ?2
                 )
                "#,
                before_ms,
                limit_i,
            )
            .execute(&self.pool)
            .await?
            .rows_affected();
            total = total.saturating_add(deleted);
            if deleted < this_round {
                break;
            }
        }
        // `PRAGMA wal_checkpoint(TRUNCATE)` returns three rows; use the
        // dynamic query API (the `query!` macro chokes on PRAGMA shapes).
        let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE);")
            .execute(&self.pool)
            .await?;
        Ok(total)
    }

    async fn delete_activity_by_source(&self, source: SourceId) -> Result<u64> {
        let source_str = source.to_string();
        let n = sqlx::query!(
            "UPDATE activity_log SET source_id = NULL WHERE source_id = ?1",
            source_str,
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(n)
    }

    async fn get_pending_ops_for_source(&self, source: SourceId) -> Result<Vec<PendingOpRow>> {
        let source_str = source.to_string();
        let rows = sqlx::query!(
            r#"
            SELECT
                id            AS "id!: i64",
                source_id     AS "source_id!: String",
                op_type       AS "op_type!: String",
                relative_path AS "relative_path!: String",
                payload_json  AS "payload_json!: String",
                attempts      AS "attempts!: i64",
                last_error    AS "last_error: String",
                scheduled_for AS "scheduled_for!: i64",
                created_at    AS "created_at!: i64"
            FROM pending_ops
            WHERE source_id = ?1
            ORDER BY id ASC
            "#,
            source_str,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|r| {
                Ok(PendingOpRow {
                    id: PendingOpId(r.id),
                    source_id: SourceId(uuid_from_str(&r.source_id)?),
                    op_type: r.op_type,
                    relative_path: relative_path_from_string(r.relative_path)?,
                    payload_json: serde_json::from_str(&r.payload_json)?,
                    attempts: r.attempts as u32,
                    last_error: r.last_error,
                    scheduled_for: r.scheduled_for,
                    created_at: r.created_at,
                })
            })
            .collect()
    }

    async fn commit_create_result(
        &self,
        op_id: PendingOpId,
        file_state: &FileStateRow,
    ) -> Result<()> {
        // A create finalizes an `upload` pending_op (migration 0001).
        self.commit_op_result_inner(op_id, "upload", file_state)
            .await
    }

    async fn commit_update_result(
        &self,
        op_id: PendingOpId,
        file_state: &FileStateRow,
    ) -> Result<()> {
        // An update also finalizes an `upload` pending_op (migration 0001).
        self.commit_op_result_inner(op_id, "upload", file_state)
            .await
    }

    async fn schema_version(&self) -> Result<i64> {
        // PRAGMA returns a non-standard row shape the `query!` macro cannot
        // describe, so use the dynamic query API (mirrors the wal_checkpoint
        // call). `user_version` is an i64-typed pragma column.
        let row: (i64,) = sqlx::query_as("PRAGMA user_version;")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    async fn table_row_count(&self, table: &str) -> Result<i64> {
        // R2-P2-4: a table name cannot be a bound parameter, so guard against
        // injection by ONLY accepting a name from the migration-defined
        // allow-list and building the (constant, no-user-substring) query from
        // the matched &'static str - never from the caller's raw input.
        let safe: &'static str = crate::state::KNOWN_STATE_TABLES
            .iter()
            .copied()
            .find(|t| *t == table)
            .ok_or_else(|| anyhow!("unknown state table for row count: {table}"))?;
        let sql = format!("SELECT COUNT(*) FROM {safe}");
        let count: i64 = sqlx::query_scalar(&sql).fetch_one(&self.pool).await?;
        Ok(count)
    }

    // --- settings -----------------------------------------------------------

    async fn get_setting(&self, key: &str) -> Result<Option<Value>> {
        let row = sqlx::query!(
            r#"SELECT value AS "value!: String" FROM settings WHERE key = ?1"#,
            key,
        )
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(serde_json::from_str(&r.value)?)),
        }
    }

    async fn set_setting(&self, key: &str, value: &Value) -> Result<()> {
        let v = serde_json::to_string(value)?;
        sqlx::query!(
            r#"
            INSERT INTO settings (key, value) VALUES (?1, ?2)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            "#,
            key,
            v,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // --- search -------------------------------------------------------------

    async fn search_files(
        &self,
        source: Option<SourceId>,
        query: &str,
        limit: u32,
    ) -> Result<Vec<FileSearchHit>> {
        let source_str = source.map(|s| s.to_string());
        let limit_i = limit as i64;
        // Build the FTS5 MATCH string per-token so ordinary filename terms
        // are treated as literals (a hyphen / quote does not error as FTS5
        // syntax) WHILE a trailing `*` still works as a prefix query (M8
        // restore search needs `proj*` to prefix-match). Quoting the WHOLE
        // input as one phrase would turn `proj*` into the literal phrase
        // `"proj*"` (the `*` inside the quotes is a non-token char that the
        // tokenizer drops), so it would match nothing.
        //
        // Per token (split on whitespace):
        // - ends with `*` (and has a non-empty stem): quote the stem and
        //   append `*` OUTSIDE the quotes -> `proj` becomes `"proj"*`, a
        //   real FTS5 prefix query.
        // - otherwise: quote the whole token as a literal phrase ->
        //   `foo-bar` becomes `"foo-bar"` (no NOT-operator error).
        // Internal double-quotes are doubled in either case. Tokens are
        // joined with a space (FTS5 implicit AND). A bare `*` token (empty
        // stem) is dropped so we never emit `""*`, which FTS5 rejects.
        // Empty input (no tokens) matches nothing - return early without
        // touching MATCH.
        let match_query = build_fts_match_query(query);
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
        let escaped = match_query;
        let rows = sqlx::query!(
            r#"
            SELECT
                fs.source_id     AS "source_id!: String",
                fs.relative_path AS "relative_path!: String",
                fs.status        AS "status!: String",
                fs.drive_file_id AS "drive_file_id: String"
            FROM file_state_fts
            JOIN file_state fs ON fs.rowid = file_state_fts.rowid
            WHERE file_state_fts MATCH ?1
              AND (?2 IS NULL OR fs.source_id = ?2)
            ORDER BY rank
            LIMIT ?3
            "#,
            escaped,
            source_str,
            limit_i,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|r| {
                Ok(FileSearchHit {
                    source_id: SourceId(uuid_from_str(&r.source_id)?),
                    relative_path: relative_path_from_string(r.relative_path)?,
                    status: file_state_status_from_str(&r.status)?,
                    drive_file_id: r.drive_file_id,
                })
            })
            .collect()
    }

    async fn list_file_state_under_prefix(
        &self,
        source: SourceId,
        prefix: &str,
        limit: u32,
    ) -> Result<Vec<RestoreFileRow>> {
        let source_str = source.to_string();
        // Fetch limit + 1 so the caller can detect truncation without a COUNT.
        let fetch = (u64::from(limit).saturating_add(1)).min(i64::MAX as u64) as i64;

        // The Restore tree shows files at/under `prefix`. An empty prefix is the
        // source root (every row). A non-empty prefix matches the directory
        // `prefix` itself: rows whose path is exactly `prefix` (a file with that
        // name) OR start with `prefix/` (anything inside the directory). We do
        // this as an INDEXED RANGE SCAN over the `(source_id, relative_path)`
        // primary key, not a `LIKE 'prefix/%'` (which cannot use the index for a
        // leading-literal-then-wildcard on all SQLite builds and risks a full
        // scan): `relative_path >= 'prefix/' AND relative_path < 'prefix0'` where
        // '0' is the byte after '/'. We additionally OR in the exact `prefix`
        // match (a file named exactly `prefix`). For an empty prefix the bounds
        // collapse to "all rows for the source".
        let rows = if prefix.is_empty() {
            sqlx::query!(
                r#"
                SELECT
                    source_id     AS "source_id!: String",
                    relative_path AS "relative_path!: String",
                    size          AS "size!: i64",
                    status        AS "status!: String",
                    drive_file_id AS "drive_file_id: String"
                FROM file_state
                WHERE source_id = ?1
                ORDER BY relative_path ASC
                LIMIT ?2
                "#,
                source_str,
                fetch,
            )
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| {
                (
                    r.source_id,
                    r.relative_path,
                    r.size,
                    r.status,
                    r.drive_file_id,
                )
            })
            .collect::<Vec<_>>()
        } else {
            // Lower bound inclusive: "prefix/"; upper bound exclusive: "prefix0"
            // ('0' == 0x30 is the next byte after '/' == 0x2F), so the range
            // covers exactly the descendants of the `prefix` directory.
            let lower = format!("{prefix}/");
            let upper = format!("{prefix}0");
            let exact = prefix.to_string();
            sqlx::query!(
                r#"
                SELECT
                    source_id     AS "source_id!: String",
                    relative_path AS "relative_path!: String",
                    size          AS "size!: i64",
                    status        AS "status!: String",
                    drive_file_id AS "drive_file_id: String"
                FROM file_state
                WHERE source_id = ?1
                  AND (
                    relative_path = ?2
                    OR (relative_path >= ?3 AND relative_path < ?4)
                  )
                ORDER BY relative_path ASC
                LIMIT ?5
                "#,
                source_str,
                exact,
                lower,
                upper,
                fetch,
            )
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|r| {
                (
                    r.source_id,
                    r.relative_path,
                    r.size,
                    r.status,
                    r.drive_file_id,
                )
            })
            .collect::<Vec<_>>()
        };

        rows.into_iter()
            .map(|(source_id, relative_path, size, status, drive_file_id)| {
                Ok(RestoreFileRow {
                    source_id: SourceId(uuid_from_str(&source_id)?),
                    relative_path: relative_path_from_string(relative_path)?,
                    size: size as u64,
                    status: file_state_status_from_str(&status)?,
                    drive_file_id,
                })
            })
            .collect()
    }

    async fn search_files_glob(
        &self,
        source: Option<SourceId>,
        pattern: &str,
        limit: u32,
    ) -> Result<Vec<FileSearchHit>> {
        let source_str = source.map(|s| s.to_string());
        let limit_i = limit as i64;
        // SQLite GLOB is case-sensitive and uses `*` / `?` / `[...]` wildcards -
        // exactly the glob grammar the Restore search box exposes (e.g. `*.rs`).
        // The pattern is passed straight through as a bound parameter (no SQL
        // injection risk - it is a value, not concatenated SQL); GLOB has no
        // ESCAPE clause, so a literal `*`/`?` cannot be searched for, which is
        // acceptable for filename glob search.
        let glob = pattern.to_string();
        let rows = sqlx::query!(
            r#"
            SELECT
                source_id     AS "source_id!: String",
                relative_path AS "relative_path!: String",
                status        AS "status!: String",
                drive_file_id AS "drive_file_id: String"
            FROM file_state
            WHERE relative_path GLOB ?1
              AND (?2 IS NULL OR source_id = ?2)
            ORDER BY relative_path ASC
            LIMIT ?3
            "#,
            glob,
            source_str,
            limit_i,
        )
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|r| {
                Ok(FileSearchHit {
                    source_id: SourceId(uuid_from_str(&r.source_id)?),
                    relative_path: relative_path_from_string(r.relative_path)?,
                    status: file_state_status_from_str(&r.status)?,
                    drive_file_id: r.drive_file_id,
                })
            })
            .collect()
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ActivityFilter, NewActivity, NewPendingOp, PageRequest};
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn temp_repo() -> (SqliteStateRepo, TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");
        let repo = SqliteStateRepo::open(&path).await.expect("open");
        (repo, dir)
    }

    fn rp(s: &str) -> RelativePath {
        // Tests construct RelativePath via the serde-transparent string
        // shape (TryFrom is `todo!()` until M2 lands the real validator).
        serde_json::from_value(Value::String(s.to_string())).expect("rp")
    }

    fn sample_account() -> AccountRow {
        AccountRow {
            id: AccountId::new_v4(),
            email: "alice@example.com".into(),
            display_name: Some("Alice".into()),
            state: AccountState::Ok,
            encryption_master_key_id: Some("kc:alice".into()),
            created_at: 1_700_000_000_000,
            last_synced_at: None,
        }
    }

    fn sample_source(account_id: AccountId) -> SourceRow {
        SourceRow {
            id: SourceId::new_v4(),
            account_id,
            display_name: "Docs".into(),
            enabled: true,
            local_path: "/home/alice/docs".into(),
            drive_folder_id: "folder-1".into(),
            drive_folder_path: "/Driven/Docs".into(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore: true,
            include_patterns: vec!["**/*".into()],
            exclude_patterns: vec!["**/*.tmp".into()],
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 1_700_000_000_000,
        }
    }

    fn sample_file(source_id: SourceId, path: &str, hash_byte: u8) -> FileStateRow {
        FileStateRow {
            source_id,
            relative_path: rp(path),
            size: 1024,
            mtime_ns: 1_700_000_000_000_000_000,
            hash_blake3: [hash_byte; 32],
            drive_file_id: None,
            drive_md5: None,
            encrypted_remote_path: None,
            status: FileStateStatus::Pending,
            last_uploaded_at: None,
            last_verified_at: None,
        }
    }

    #[tokio::test]
    async fn account_round_trip() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();

        let listed = repo.list_accounts().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(&listed[0], &acct);

        // Idempotent upsert (would have cascade-nuked if REPLACE was used).
        repo.upsert_account(&acct).await.unwrap();
        assert_eq!(repo.list_accounts().await.unwrap().len(), 1);

        repo.mark_account_state(acct.id, AccountState::NeedsReauth)
            .await
            .unwrap();
        let after = repo.list_accounts().await.unwrap();
        assert_eq!(after[0].state, AccountState::NeedsReauth);

        repo.delete_account(acct.id).await.unwrap();
        assert!(repo.list_accounts().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn source_round_trip() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        let all = repo.list_sources().await.unwrap();
        assert_eq!(all, vec![src.clone()]);

        let enabled = repo.list_enabled_sources_for(acct.id).await.unwrap();
        assert_eq!(enabled, vec![src.clone()]);

        let mut disabled = src.clone();
        disabled.enabled = false;
        repo.upsert_source(&disabled).await.unwrap();
        let still_one_total = repo.list_sources().await.unwrap();
        assert_eq!(still_one_total.len(), 1);
        let enabled_after = repo.list_enabled_sources_for(acct.id).await.unwrap();
        assert!(enabled_after.is_empty());

        repo.delete_source(src.id).await.unwrap();
        assert!(repo.list_sources().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn atomic_stamp_and_insert_commits_both_on_success() {
        // R1-P1-1: the happy path stamps the account's master-key id AND inserts
        // the source in one transaction; both land.
        let (repo, _dir) = temp_repo().await;
        let mut acct = sample_account();
        acct.encryption_master_key_id = None; // start unprovisioned
        repo.upsert_account(&acct).await.unwrap();

        let mut src = sample_source(acct.id);
        src.encryption_enabled = true;
        repo.insert_source_with_optional_master_key_stamp(
            &src,
            Some((acct.id, "kc:alice-master".into())),
        )
        .await
        .unwrap();

        // Account is now provisioned AND the source exists.
        let after = repo.list_accounts().await.unwrap();
        assert_eq!(
            after[0].encryption_master_key_id.as_deref(),
            Some("kc:alice-master"),
            "the account master-key id must be stamped on success"
        );
        let sources = repo.list_sources().await.unwrap();
        assert_eq!(sources, vec![src]);
    }

    #[tokio::test]
    async fn atomic_stamp_and_insert_rolls_back_account_stamp_on_source_failure() {
        // R1-P1-1 (data-safety): if the source insert fails, the account
        // master-key stamp MUST roll back too - so the account is NEVER left
        // "provisioned" without the source/phrase (an unrestorable encrypted
        // backup). Force the source insert to fail with a foreign-key violation
        // (a source whose account_id points at NO account row).
        let (repo, _dir) = temp_repo().await;
        let mut acct = sample_account();
        acct.encryption_master_key_id = None;
        repo.upsert_account(&acct).await.unwrap();

        // A source row referencing a non-existent account -> FK violation on
        // insert (foreign_keys = ON).
        let mut bad_src = sample_source(AccountId::new_v4());
        bad_src.encryption_enabled = true;

        let err = repo
            .insert_source_with_optional_master_key_stamp(
                &bad_src,
                Some((acct.id, "kc:alice-master".into())),
            )
            .await
            .expect_err("source insert must fail on the FK violation");
        assert!(
            format!("{err:#}").to_lowercase().contains("foreign")
                || format!("{err:#}").to_lowercase().contains("constraint"),
            "expected a foreign-key/constraint error, got: {err:#}"
        );

        // The account stamp rolled back: still unprovisioned, and NO source row.
        let after = repo.list_accounts().await.unwrap();
        assert_eq!(
            after[0].encryption_master_key_id, None,
            "the account must NOT be marked provisioned after a rolled-back insert"
        );
        assert!(
            repo.list_sources().await.unwrap().is_empty(),
            "no source row may persist after a rolled-back insert"
        );

        // A RETRY against a valid source now succeeds and stamps the account.
        let mut good_src = sample_source(acct.id);
        good_src.encryption_enabled = true;
        repo.insert_source_with_optional_master_key_stamp(
            &good_src,
            Some((acct.id, "kc:alice-master".into())),
        )
        .await
        .expect("retry must succeed");
        let after = repo.list_accounts().await.unwrap();
        assert_eq!(
            after[0].encryption_master_key_id.as_deref(),
            Some("kc:alice-master")
        );
        assert_eq!(repo.list_sources().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn atomic_insert_without_stamp_just_inserts_the_source() {
        // R1-P1-1: a subsequent encrypted source (or an unencrypted one) passes
        // `None` for the stamp - the account row is untouched, only the source
        // is inserted.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account(); // already provisioned in the sample
        repo.upsert_account(&acct).await.unwrap();
        let original_key = acct.encryption_master_key_id.clone();

        let src = sample_source(acct.id);
        repo.insert_source_with_optional_master_key_stamp(&src, None)
            .await
            .unwrap();

        let after = repo.list_accounts().await.unwrap();
        assert_eq!(after[0].encryption_master_key_id, original_key);
        assert_eq!(repo.list_sources().await.unwrap(), vec![src]);
    }

    #[tokio::test]
    async fn cas_stamp_rejects_a_divergent_concurrent_key() {
        // R2-P1-1 (data-safety): the master-key stamp is a COMPARE-AND-SET. Once
        // the account is stamped with one key id, a later insert that tries to
        // stamp a DIFFERENT key id (a lost race WITHOUT the per-account lock) is
        // REFUSED and its source row is NOT inserted - so a source can never be
        // committed under a master key the account does not record (which would be
        // unrestorable). The first key id is preserved.
        let (repo, _dir) = temp_repo().await;
        let mut acct = sample_account();
        acct.encryption_master_key_id = None;
        repo.upsert_account(&acct).await.unwrap();

        // First add wins the stamp.
        let mut first = sample_source(acct.id);
        first.encryption_enabled = true;
        repo.insert_source_with_optional_master_key_stamp(&first, Some((acct.id, "key-A".into())))
            .await
            .expect("first stamp wins");

        // Second add tries to stamp a DIFFERENT key id -> rejected.
        let mut second = sample_source(acct.id);
        second.encryption_enabled = true;
        let err = repo
            .insert_source_with_optional_master_key_stamp(
                &second,
                Some((acct.id, "key-B-divergent".into())),
            )
            .await
            .expect_err("a divergent concurrent stamp must be rejected");
        assert!(
            format!("{err:#}").contains("concurrent"),
            "error must explain the concurrent-stamp refusal: {err:#}"
        );

        // The account keeps key-A and ONLY the first source persisted.
        let after = repo.list_accounts().await.unwrap();
        assert_eq!(
            after[0].encryption_master_key_id.as_deref(),
            Some("key-A"),
            "the first key id must be preserved (never overwritten)"
        );
        let sources = repo.list_sources().await.unwrap();
        assert_eq!(
            sources.len(),
            1,
            "the divergent-key source must NOT persist"
        );
        assert_eq!(sources[0].id, first.id);
    }

    #[tokio::test]
    async fn cas_stamp_is_idempotent_for_the_same_key() {
        // R2-P1-1: a second add that stamps the SAME key id (the per-account lock
        // makes the loser load the existing key and pass it again) is idempotent -
        // the stamp is unchanged and the second source IS inserted.
        let (repo, _dir) = temp_repo().await;
        let mut acct = sample_account();
        acct.encryption_master_key_id = None;
        repo.upsert_account(&acct).await.unwrap();

        let mut first = sample_source(acct.id);
        first.encryption_enabled = true;
        repo.insert_source_with_optional_master_key_stamp(&first, Some((acct.id, "key-A".into())))
            .await
            .expect("first stamp");

        // Second add stamps the SAME key id -> idempotent success, source inserted.
        let mut second = sample_source(acct.id);
        second.encryption_enabled = true;
        repo.insert_source_with_optional_master_key_stamp(&second, Some((acct.id, "key-A".into())))
            .await
            .expect("same-key stamp is idempotent");

        let after = repo.list_accounts().await.unwrap();
        assert_eq!(after[0].encryption_master_key_id.as_deref(), Some("key-A"));
        assert_eq!(
            repo.list_sources().await.unwrap().len(),
            2,
            "both sources persist under the single shared key"
        );
    }

    #[tokio::test]
    async fn cascade_delete_account_removes_sources_and_files() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();
        let f = sample_file(src.id, "a.txt", 0xAA);
        repo.upsert_file_state(&f).await.unwrap();

        repo.delete_account(acct.id).await.unwrap();
        assert!(repo.list_accounts().await.unwrap().is_empty());
        assert!(repo.list_sources().await.unwrap().is_empty());
        let map = repo.load_source_file_state(src.id).await.unwrap();
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn file_state_round_trip() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        let f1 = sample_file(src.id, "a/b.txt", 0x11);
        let f2 = sample_file(src.id, "c.txt", 0x22);
        repo.upsert_file_state(&f1).await.unwrap();
        repo.upsert_file_state(&f2).await.unwrap();

        let map = repo.load_source_file_state(src.id).await.unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&rp("a/b.txt")), Some(&f1));
        assert_eq!(map.get(&rp("c.txt")), Some(&f2));

        let got = repo
            .get_file_state(src.id, &rp("a/b.txt"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, f1);

        // Update via upsert (must NOT cascade-nuke).
        let mut f1_upd = f1.clone();
        f1_upd.status = FileStateStatus::Synced;
        f1_upd.drive_file_id = Some("drv-1".into());
        f1_upd.drive_md5 = Some([0xFF; 16]);
        repo.upsert_file_state(&f1_upd).await.unwrap();
        assert_eq!(
            repo.get_file_state(src.id, &rp("a/b.txt"))
                .await
                .unwrap()
                .unwrap(),
            f1_upd
        );

        repo.delete_file_state(src.id, &rp("c.txt")).await.unwrap();
        assert_eq!(repo.load_source_file_state(src.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn mark_excluded_orphans_flips_only_named_rows() {
        // P1-3: marking one of two synced rows excluded_orphan must flip that
        // row's status and leave the other untouched (DESIGN s5.5).
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        let mut a = sample_file(src.id, "a.txt", 0x11);
        a.status = FileStateStatus::Synced;
        let mut b = sample_file(src.id, "b.txt", 0x22);
        b.status = FileStateStatus::Synced;
        repo.upsert_file_state(&a).await.unwrap();
        repo.upsert_file_state(&b).await.unwrap();

        let updated = repo
            .mark_excluded_orphans(src.id, &[rp("a.txt")])
            .await
            .unwrap();
        assert_eq!(updated, 1);

        let map = repo.load_source_file_state(src.id).await.unwrap();
        assert_eq!(
            map.get(&rp("a.txt")).unwrap().status,
            FileStateStatus::ExcludedOrphan,
            "named row must be flipped to excluded_orphan"
        );
        assert_eq!(
            map.get(&rp("b.txt")).unwrap().status,
            FileStateStatus::Synced,
            "unnamed row must be untouched"
        );

        // Empty slice is a no-op (returns 0, touches nothing).
        let none = repo.mark_excluded_orphans(src.id, &[]).await.unwrap();
        assert_eq!(none, 0);
    }

    #[tokio::test]
    async fn pending_op_lifecycle() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        let id = repo
            .enqueue_pending_op(NewPendingOp {
                source_id: src.id,
                op_type: "upload".into(),
                relative_path: rp("a.txt"),
                payload_json: serde_json::json!({ "session": "abc" }),
                scheduled_for: 1_000,
                created_at: 500,
            })
            .await
            .unwrap();
        assert!(id.0 > 0);

        let due = repo.get_pending_ops_due(2_000, 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, id);
        assert_eq!(due[0].attempts, 0);

        let none_due = repo.get_pending_ops_due(500, 10).await.unwrap();
        assert!(none_due.is_empty());

        repo.mark_pending_op_attempted(id, Some("boom"), 5_000)
            .await
            .unwrap();
        let due_after = repo.get_pending_ops_due(10_000, 10).await.unwrap();
        assert_eq!(due_after.len(), 1);
        assert_eq!(due_after[0].attempts, 1);
        assert_eq!(due_after[0].last_error.as_deref(), Some("boom"));
        assert_eq!(due_after[0].scheduled_for, 5_000);

        repo.delete_pending_op(id).await.unwrap();
        assert!(repo
            .get_pending_ops_due(10_000, 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn activity_write_and_query() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        for (ts, level, et) in [
            (100, ActivityLevel::Info, "scan_done"),
            (200, ActivityLevel::Warn, "paused"),
            (300, ActivityLevel::Error, "error"),
        ] {
            repo.write_activity(NewActivity {
                ts,
                source_id: Some(src.id),
                level,
                event_type: et.into(),
                file_count: Some(42),
                bytes: Some(1024),
                message: Some("hello".into()),
            })
            .await
            .unwrap();
        }

        let page = repo
            .query_activity(ActivityFilter::default(), PageRequest::first(10))
            .await
            .unwrap();
        assert_eq!(page.total, 3);
        // newest-first order
        assert_eq!(page.rows.len(), 3);
        assert_eq!(page.rows[0].ts, 300);
        assert_eq!(page.rows[2].ts, 100);

        let warn_plus = repo
            .query_activity(
                ActivityFilter {
                    min_level: Some(ActivityLevel::Warn),
                    ..Default::default()
                },
                PageRequest::first(10),
            )
            .await
            .unwrap();
        assert_eq!(warn_plus.rows.len(), 2);
        assert_eq!(warn_plus.total, 2);

        let only_paused = repo
            .query_activity(
                ActivityFilter {
                    event_types: vec!["paused".into()],
                    ..Default::default()
                },
                PageRequest::first(10),
            )
            .await
            .unwrap();
        assert_eq!(only_paused.rows.len(), 1);
        assert_eq!(only_paused.rows[0].event_type, "paused");

        let since_200 = repo
            .query_activity(
                ActivityFilter {
                    since_ms: Some(200),
                    ..Default::default()
                },
                PageRequest::first(10),
            )
            .await
            .unwrap();
        assert_eq!(since_200.rows.len(), 2);
    }

    #[tokio::test]
    async fn query_activity_event_type_filter_paginates_correctly() {
        // Regression for the event_type filter being applied AFTER the SQL
        // LIMIT (so a page could come back empty even with matching rows
        // further down) and the total ignoring it. Both must now be SQL-side.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        // 10 rows: alternating "keep" / "skip" by ts. After ORDER BY ts DESC
        // the newest 5 (ts 6..10) are a mix; "keep" rows are the even ts.
        for ts in 1..=10 {
            let et = if ts % 2 == 0 { "keep" } else { "skip" };
            repo.write_activity(NewActivity {
                ts,
                source_id: Some(src.id),
                level: ActivityLevel::Info,
                event_type: et.into(),
                file_count: None,
                bytes: None,
                message: None,
            })
            .await
            .unwrap();
        }

        // 5 "keep" rows total (ts 2,4,6,8,10). Page them 2-at-a-time.
        let filter = || ActivityFilter {
            event_types: vec!["keep".into()],
            ..Default::default()
        };

        let p0 = repo
            .query_activity(filter(), PageRequest::first(2))
            .await
            .unwrap();
        // total must reflect ONLY the filtered type, not all 10 rows.
        assert_eq!(p0.total, 5, "total must count only matching event_type");
        assert_eq!(p0.rows.len(), 2);
        assert!(p0.rows.iter().all(|r| r.event_type == "keep"));
        // newest-first: ts 10 then ts 8.
        assert_eq!(p0.rows[0].ts, 10);
        assert_eq!(p0.rows[1].ts, 8);
        assert!(p0.has_more, "more keep rows remain after page 0");

        // R2-P1-2: page on via the KEYSET cursor (the oldest row of each page).
        // The filtered "keep" rows must all surface in order even though "skip"
        // rows interleave them - the old client-side filter could yield an empty
        // page here, and offset paging would skip rows under live prepends.
        let cursor1 = p0.rows.last().map(|r| (r.ts, r.id.0)).unwrap();
        let p1 = repo
            .query_activity(filter(), PageRequest::after_cursor(cursor1.0, cursor1.1, 2))
            .await
            .unwrap();
        assert_eq!(p1.total, 5);
        assert_eq!(p1.rows.len(), 2);
        assert_eq!(p1.rows[0].ts, 6);
        assert_eq!(p1.rows[1].ts, 4);
        assert!(p1.has_more, "the 5th keep row remains");

        let cursor2 = p1.rows.last().map(|r| (r.ts, r.id.0)).unwrap();
        let p2 = repo
            .query_activity(filter(), PageRequest::after_cursor(cursor2.0, cursor2.1, 2))
            .await
            .unwrap();
        assert_eq!(p2.total, 5);
        assert_eq!(p2.rows.len(), 1, "last page holds the 5th match");
        assert_eq!(p2.rows[0].ts, 2);
        assert_eq!(p2.rows[0].event_type, "keep");
        assert!(!p2.has_more, "no keep rows after the 5th");
    }

    #[tokio::test]
    async fn query_activity_keyset_is_stable_under_inserts() {
        // R2-P1-2: a keyset page must NOT skip or duplicate rows when new rows
        // are PREPENDED to activity_log between page fetches (the live-prepend
        // case that offset paging mishandled). Walk all rows by cursor while
        // inserting newer rows mid-walk; the original rows must all appear
        // exactly once with no gaps.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        let mk = |ts: i64| NewActivity {
            ts,
            source_id: Some(src.id),
            level: ActivityLevel::Info,
            event_type: "e".into(),
            file_count: None,
            bytes: None,
            message: None,
        };
        // Original rows ts 10..=15 (6 rows).
        for ts in 10..=15 {
            repo.write_activity(mk(ts)).await.unwrap();
        }

        // Page 1 (newest 2: ts 15, 14).
        let p0 = repo
            .query_activity(ActivityFilter::default(), PageRequest::first(2))
            .await
            .unwrap();
        assert_eq!(
            p0.rows.iter().map(|r| r.ts).collect::<Vec<_>>(),
            vec![15, 14]
        );

        // Simulate live prepends: 3 NEWER rows (ts 100, 101, 102) arrive between
        // page 1 and page 2. Under OFFSET paging these would shift page 2 and
        // skip the original ts-13 row; under keyset the cursor pins the walk.
        for ts in [100, 101, 102] {
            repo.write_activity(mk(ts)).await.unwrap();
        }

        let cursor = p0.rows.last().map(|r| (r.ts, r.id.0)).unwrap();
        let p1 = repo
            .query_activity(
                ActivityFilter::default(),
                PageRequest::after_cursor(cursor.0, cursor.1, 2),
            )
            .await
            .unwrap();
        // Strictly older than (14, id) -> ts 13, 12. The 3 prepended rows are
        // NEWER than the cursor, so they NEVER appear in this older page.
        assert_eq!(
            p1.rows.iter().map(|r| r.ts).collect::<Vec<_>>(),
            vec![13, 12],
            "keyset page is unaffected by newer prepended rows"
        );

        let cursor = p1.rows.last().map(|r| (r.ts, r.id.0)).unwrap();
        let p2 = repo
            .query_activity(
                ActivityFilter::default(),
                PageRequest::after_cursor(cursor.0, cursor.1, 2),
            )
            .await
            .unwrap();
        assert_eq!(
            p2.rows.iter().map(|r| r.ts).collect::<Vec<_>>(),
            vec![11, 10],
            "the final original rows surface with no skip/dup"
        );
        // p2 was an exactly-full page, so keyset `has_more` is true (a false
        // positive that costs only one extra empty fetch). The next page IS
        // empty - the original history is fully walked with no skip / dup.
        let cursor = p2.rows.last().map(|r| (r.ts, r.id.0)).unwrap();
        let p3 = repo
            .query_activity(
                ActivityFilter::default(),
                PageRequest::after_cursor(cursor.0, cursor.1, 2),
            )
            .await
            .unwrap();
        assert!(p3.rows.is_empty(), "original history exhausted");
        assert!(!p3.has_more, "an empty page reports no more");
    }

    #[tokio::test]
    async fn write_activity_rejects_counts_above_i64_max() {
        // R2-P2-3: a u64 file_count / bytes above i64::MAX must be REJECTED on
        // write (not silently wrapped negative via `as`). The boundary value
        // i64::MAX itself is accepted and round-trips losslessly.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        let base = |bytes: Option<u64>, file_count: Option<u64>| NewActivity {
            ts: 1,
            source_id: Some(src.id),
            level: ActivityLevel::Info,
            event_type: "upload_done".into(),
            file_count,
            bytes,
            message: None,
        };

        // bytes just over i64::MAX -> rejected.
        let over = (i64::MAX as u64) + 1;
        let err = repo
            .write_activity(base(Some(over), None))
            .await
            .expect_err("bytes > i64::MAX must be rejected");
        assert!(err.to_string().contains("internal.bad_request"));
        // file_count just over i64::MAX -> rejected.
        let err = repo
            .write_activity(base(None, Some(over)))
            .await
            .expect_err("file_count > i64::MAX must be rejected");
        assert!(err.to_string().contains("internal.bad_request"));

        // The exact i64::MAX boundary is accepted and decodes back losslessly.
        let boundary = i64::MAX as u64;
        repo.write_activity(base(Some(boundary), Some(boundary)))
            .await
            .expect("i64::MAX boundary must be accepted");
        let page = repo
            .query_activity(ActivityFilter::default(), PageRequest::first(10))
            .await
            .unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].bytes, Some(boundary));
        assert_eq!(page.rows[0].file_count, Some(boundary));
    }

    #[tokio::test]
    async fn distinct_activity_event_types_returns_sorted_unique_set() {
        // M7-P2-4: the dropdown source. DISTINCT, sorted, across the whole log -
        // including types not in any single loaded page.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        for (ts, et) in [
            (1, "upload_done"),
            (2, "scan_done"),
            (3, "upload_done"), // duplicate type
            (4, "paused"),
        ] {
            repo.write_activity(NewActivity {
                ts,
                source_id: Some(src.id),
                level: ActivityLevel::Info,
                event_type: et.into(),
                file_count: None,
                bytes: None,
                message: None,
            })
            .await
            .unwrap();
        }

        let types = repo.distinct_activity_event_types().await.unwrap();
        assert_eq!(types, vec!["paused", "scan_done", "upload_done"]);
    }

    #[tokio::test]
    async fn activity_summary_aggregates_bytes_and_status_counts() {
        // M7-P2-5 (DESIGN s8.3 header). Byte sums honour the today / week /
        // throughput-window boundaries; status counts group `file_state`.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        // Boundaries (ms): today >= 1000, week >= 100, throughput window >= 1500.
        let day_start = 1000;
        let week_start = 100;
        let window_start = 1500;
        let window_ms = 60_000;

        // Rows: one before the week window (excluded entirely), one in the week
        // but before today, one today-but-before-the-throughput-window, and one
        // inside the throughput window. `bytes` is the upload size.
        for (ts, bytes) in [
            (50, 999_u64), // before week_start -> excluded from every sum
            (200, 10),     // week only
            (1200, 20),    // week + today
            (1800, 30),    // week + today + throughput window
        ] {
            repo.write_activity(NewActivity {
                ts,
                source_id: Some(src.id),
                level: ActivityLevel::Info,
                event_type: "upload_done".into(),
                file_count: Some(1),
                bytes: Some(bytes),
                message: None,
            })
            .await
            .unwrap();
        }

        // file_state rows: 2 synced, 1 pending across statuses.
        let mut s1 = sample_file(src.id, "a.txt", 0x01);
        s1.status = FileStateStatus::Synced;
        let mut s2 = sample_file(src.id, "b.txt", 0x02);
        s2.status = FileStateStatus::Synced;
        let mut p1 = sample_file(src.id, "c.txt", 0x03);
        p1.status = FileStateStatus::Pending;
        repo.upsert_file_state(&s1).await.unwrap();
        repo.upsert_file_state(&s2).await.unwrap();
        repo.upsert_file_state(&p1).await.unwrap();

        let summary = repo
            .activity_summary(day_start, week_start, window_start, window_ms)
            .await
            .unwrap();

        // today = rows with ts >= 1000: 20 + 30 = 50.
        assert_eq!(summary.bytes_today, 50);
        // week = rows with ts >= 100: 10 + 20 + 30 = 60 (the ts=50 row excluded).
        assert_eq!(summary.bytes_week, 60);
        // throughput window = rows with ts >= 1500: 30.
        assert_eq!(summary.throughput_window_bytes, 30);
        assert_eq!(summary.throughput_window_ms, window_ms);

        // status counts: Pending=1, Synced=2 (sorted ascending by status text).
        assert_eq!(
            summary.file_status_counts,
            vec![
                FileStatusCount {
                    status: FileStateStatus::Pending,
                    count: 1,
                },
                FileStatusCount {
                    status: FileStateStatus::Synced,
                    count: 2,
                },
            ]
        );
    }

    #[tokio::test]
    async fn activity_summary_throughput_counts_rows_before_week_start() {
        // R1-P2-2: near the START of a week the throughput window can begin
        // BEFORE week_start. The old outer `WHERE ts >= week_start` dropped
        // those rows before the per-sum CASE ran, so throughput undercounted at
        // week boundaries. With the fix the outer filter is MIN(day, week,
        // throughput), so a row inside the throughput window but before
        // week_start is still counted in throughput (and excluded from
        // today/week, which it correctly precedes).
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        // The throughput window starts BEFORE the week boundary (week just rolled
        // over): throughput_window_start (500) < week_start (1000) < day_start.
        let day_start = 2000;
        let week_start = 1000;
        let window_start = 500;
        let window_ms = 60_000;

        // A row at ts=600: inside the throughput window (>=500), but BEFORE the
        // week boundary (<1000). The bug excluded it from throughput entirely.
        // A second row at ts=1500 is inside week + throughput (control).
        for (ts, bytes) in [(600_i64, 70_u64), (1500, 30)] {
            repo.write_activity(NewActivity {
                ts,
                source_id: Some(src.id),
                level: ActivityLevel::Info,
                event_type: "upload_done".into(),
                file_count: Some(1),
                bytes: Some(bytes),
                message: None,
            })
            .await
            .unwrap();
        }

        let summary = repo
            .activity_summary(day_start, week_start, window_start, window_ms)
            .await
            .unwrap();

        // throughput window = rows with ts >= 500: 70 + 30 = 100 (the pre-week
        // row is now counted; the bug would have reported only 30).
        assert_eq!(
            summary.throughput_window_bytes, 100,
            "throughput counts the row inside the window but before week_start"
        );
        // today = rows with ts >= 2000: none.
        assert_eq!(summary.bytes_today, 0);
        // week = rows with ts >= 1000: only the ts=1500 row -> 30.
        assert_eq!(summary.bytes_week, 30);
    }

    #[test]
    fn relative_path_nfc_normalizes() {
        // SPEC s24 local.unicode_collision: byte-distinct NFD/NFC spellings
        // of the same logical path must collapse to one canonical key.
        // Construct via the real validator (NOT the serde-transparent `rp()`
        // helper, which bypasses normalization).
        let nfd = "cafe\u{0301}.txt".to_string(); // 'e' + combining acute
        let nfc = "caf\u{00e9}.txt".to_string(); // precomposed 'e-acute'
        assert_ne!(nfd, nfc, "inputs must be byte-distinct to prove the point");

        let from_nfd = RelativePath::try_from(nfd).expect("nfd path valid");
        let from_nfc = RelativePath::try_from(nfc.clone()).expect("nfc path valid");

        // Both normalize to the same NFC form and compare equal.
        assert_eq!(from_nfd, from_nfc);
        assert_eq!(from_nfd.as_str(), nfc, "stored form is NFC");
    }

    #[tokio::test]
    async fn settings_round_trip() {
        let (repo, _dir) = temp_repo().await;

        // Migration 0002 seeded the canonical keys.
        let global = repo.get_setting("global").await.unwrap().unwrap();
        assert_eq!(global["scan_interval_secs"], serde_json::json!(600));

        let telemetry = repo.get_setting("telemetry").await.unwrap().unwrap();
        let install_id = telemetry["install_id"].as_str().unwrap();
        assert_eq!(install_id.len(), 32); // hex of 16 bytes

        // Round-trip a custom value.
        let v = serde_json::json!({"foo": "bar", "n": 7});
        repo.set_setting("custom", &v).await.unwrap();
        assert_eq!(repo.get_setting("custom").await.unwrap(), Some(v.clone()));

        // Overwrite.
        let v2 = serde_json::json!({"foo": "baz"});
        repo.set_setting("custom", &v2).await.unwrap();
        assert_eq!(repo.get_setting("custom").await.unwrap(), Some(v2));

        assert_eq!(repo.get_setting("does_not_exist").await.unwrap(), None);
    }

    #[tokio::test]
    async fn concurrent_upsert_file_state() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        let repo = Arc::new(repo);
        let mut handles = Vec::new();
        // 50 tasks upsert IDENTICAL bytes to the same key. Race-free
        // assertion: the final row matches the agreed payload.
        for _ in 0..50 {
            let repo = repo.clone();
            let f = sample_file(src.id, "race.txt", 0x77);
            handles.push(tokio::spawn(async move {
                repo.upsert_file_state(&f).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let got = repo
            .get_file_state(src.id, &rp("race.txt"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.hash_blake3, [0x77; 32]);
        // Exactly one row exists.
        let map = repo.load_source_file_state(src.id).await.unwrap();
        assert_eq!(map.len(), 1);
    }

    #[tokio::test]
    async fn prune_activity_age_only() {
        let (repo, _dir) = temp_repo().await;
        for ts in 0..200 {
            repo.write_activity(NewActivity {
                ts,
                source_id: None,
                level: ActivityLevel::Info,
                event_type: "noise".into(),
                file_count: None,
                bytes: None,
                message: None,
            })
            .await
            .unwrap();
        }
        // Age-only semantics: prune rows with ts < 150. hard_cap is a
        // ceiling on rows-deleted-this-call, not a row-count target.
        let deleted = repo
            .prune_activity_older_than(150, 10_000, None)
            .await
            .unwrap();
        assert_eq!(deleted, 150);

        let page = repo
            .query_activity(ActivityFilter::default(), PageRequest::first(1))
            .await
            .unwrap();
        assert_eq!(page.total, 50);
        // newest row survived
        assert_eq!(page.rows[0].ts, 199);
    }

    #[tokio::test]
    async fn prune_with_batch_size_iterates() {
        let (repo, _dir) = temp_repo().await;
        for ts in 0..123 {
            repo.write_activity(NewActivity {
                ts,
                source_id: None,
                level: ActivityLevel::Info,
                event_type: "noise".into(),
                file_count: None,
                bytes: None,
                message: None,
            })
            .await
            .unwrap();
        }
        // Batch size 10, no hard cap. Every row is eligible -> all 123
        // deleted across ceil(123/10) = 13 rounds.
        let deleted = repo
            .prune_activity_older_than(i64::MAX, 10_000, Some(10))
            .await
            .unwrap();
        assert_eq!(deleted, 123);
        let page = repo
            .query_activity(ActivityFilter::default(), PageRequest::first(1))
            .await
            .unwrap();
        assert_eq!(page.total, 0);
    }

    #[tokio::test]
    async fn prune_honours_hard_cap_ceiling() {
        let (repo, _dir) = temp_repo().await;
        for ts in 0..50 {
            repo.write_activity(NewActivity {
                ts,
                source_id: None,
                level: ActivityLevel::Info,
                event_type: "noise".into(),
                file_count: None,
                bytes: None,
                message: None,
            })
            .await
            .unwrap();
        }
        // 50 rows eligible, hard_cap = 20 caps deletion at 20 (oldest).
        let deleted = repo
            .prune_activity_older_than(i64::MAX, 20, Some(7))
            .await
            .unwrap();
        assert_eq!(deleted, 20);
        let page = repo
            .query_activity(ActivityFilter::default(), PageRequest::first(100))
            .await
            .unwrap();
        assert_eq!(page.total, 30);
        // newest 30 survived
        assert_eq!(page.rows[0].ts, 49);
        assert_eq!(page.rows[29].ts, 20);
    }

    #[tokio::test]
    async fn commit_create_result_atomic_persists_both() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        let op_id = repo
            .enqueue_pending_op(NewPendingOp {
                source_id: src.id,
                op_type: "upload".into(),
                relative_path: rp("a.txt"),
                payload_json: serde_json::json!({}),
                scheduled_for: 100,
                created_at: 50,
            })
            .await
            .unwrap();

        let f = sample_file(src.id, "a.txt", 0xAB);
        repo.commit_create_result(op_id, &f).await.unwrap();

        // file_state row landed.
        let got = repo
            .get_file_state(src.id, &rp("a.txt"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.hash_blake3, [0xAB; 32]);
        // pending_op row removed.
        assert!(repo
            .get_pending_ops_due(i64::MAX, 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn commit_create_result_rolls_back_on_failure() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let real_src = sample_source(acct.id);
        repo.upsert_source(&real_src).await.unwrap();

        // Enqueue a pending_op against the REAL source (so the row
        // exists and can survive a rollback).
        let op_id = repo
            .enqueue_pending_op(NewPendingOp {
                source_id: real_src.id,
                op_type: "upload".into(),
                relative_path: rp("a.txt"),
                payload_json: serde_json::json!({}),
                scheduled_for: 100,
                created_at: 50,
            })
            .await
            .unwrap();

        // Build a FileStateRow whose source_id does NOT exist. The
        // file_state.source_id FK violates -> upsert errors -> `?`
        // bubbles -> tx drops without commit -> rollback. The
        // pending_op must survive.
        let phantom = SourceId::new_v4();
        let bad = sample_file(phantom, "a.txt", 0xCD);
        let res = repo.commit_create_result(op_id, &bad).await;
        assert!(res.is_err(), "FK violation must surface as Err");

        let still_there = repo.get_pending_ops_due(i64::MAX, 10).await.unwrap();
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].id, op_id);
        // And no orphan file_state row landed.
        assert!(repo
            .get_file_state(phantom, &rp("a.txt"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn commit_op_result_rolls_back_on_missing_pending_op() {
        // DESIGN s5.6: committing a result for an op_id that has no
        // pending_ops row must NOT persist the file_state. Distinct from the
        // FK-violation test: here the file_state upsert WOULD succeed, so the
        // only thing that can stop the commit is the `rows_affected == 1`
        // guard on the pending_op DELETE.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        // Fabricate an op_id that was never enqueued (no pending_ops row).
        let phantom_op = PendingOpId(999_999);
        let f = sample_file(src.id, "ghost.txt", 0xEE);
        let res = repo.commit_create_result(phantom_op, &f).await;
        assert!(
            res.is_err(),
            "committing against a missing pending_op must Err"
        );

        // The file_state upsert must have been rolled back.
        assert!(repo
            .get_file_state(src.id, &rp("ghost.txt"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn open_creates_missing_parent_dir() {
        // P1-1: `create_if_missing(true)` makes the DB FILE but not its
        // parent DIR. `open()` must create the parent tree first, so a
        // first-boot path under a not-yet-existing `<config_dir>/driven/`
        // succeeds rather than failing before migrations.
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("does").join("not").join("exist");
        assert!(!nested.exists(), "precondition: parent dir is absent");
        let db_path = nested.join("state.db");
        let repo = SqliteStateRepo::open(&db_path).await.expect("open");
        // The DB is usable (migrations ran).
        assert!(repo.list_accounts().await.unwrap().is_empty());
        assert!(db_path.exists(), "db file created under fresh parent dir");
    }

    #[tokio::test]
    async fn commit_create_result_with_mismatched_op_does_not_delete_unrelated_op() {
        // P1-3: a stale/mismatched op_id that EXISTS but belongs to a
        // DIFFERENT (source_id, relative_path) must NOT delete that
        // unrelated pending_op nor commit the wrong file_state. The DELETE
        // is now bound to id + source_id + relative_path, so it affects 0
        // rows -> Err -> rollback.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        // An unrelated pending_op for "other.txt".
        let other_op = repo
            .enqueue_pending_op(NewPendingOp {
                source_id: src.id,
                op_type: "upload".into(),
                relative_path: rp("other.txt"),
                payload_json: serde_json::json!({}),
                scheduled_for: 100,
                created_at: 50,
            })
            .await
            .unwrap();

        // Commit a result for "a.txt" but pass the op_id that belongs to
        // "other.txt". The id exists, so the old id-only DELETE would have
        // wrongly removed `other_op` and committed a.txt's file_state.
        let f = sample_file(src.id, "a.txt", 0xAB);
        let res = repo.commit_create_result(other_op, &f).await;
        assert!(
            res.is_err(),
            "mismatched (source_id, relative_path) must Err"
        );

        // The unrelated pending_op must survive untouched.
        let due = repo.get_pending_ops_due(i64::MAX, 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, other_op);
        // And no file_state row for a.txt landed.
        assert!(repo
            .get_file_state(src.id, &rp("a.txt"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn commit_create_result_with_wrong_op_type_does_not_delete_or_commit() {
        // P1 r2: the pending_ops DELETE is bound to op_type so a commit that
        // finalizes an UPLOAD cannot consume a DIFFERENT queued op for the
        // SAME (source_id, relative_path) - e.g. a `trash` op. Without the
        // `AND op_type = 'upload'` guard the id+path-matched DELETE would
        // wrongly remove the trash op and commit the upload's file_state.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        // Queue a `trash` op for "a.txt" (NOT an upload).
        let trash_op = repo
            .enqueue_pending_op(NewPendingOp {
                source_id: src.id,
                op_type: "trash".into(),
                relative_path: rp("a.txt"),
                payload_json: serde_json::json!({}),
                scheduled_for: 100,
                created_at: 50,
            })
            .await
            .unwrap();

        // Finalize an UPLOAD result for the SAME file, passing the trash
        // op's id. id + (source_id, relative_path) match, but op_type does
        // not -> DELETE affects 0 rows -> Err -> rollback.
        let f = sample_file(src.id, "a.txt", 0xAB);
        let res = repo.commit_create_result(trash_op, &f).await;
        assert!(
            res.is_err(),
            "committing an upload against a trash op must Err"
        );

        // The trash op must survive untouched.
        let due = repo.get_pending_ops_due(i64::MAX, 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, trash_op);
        assert_eq!(due[0].op_type, "trash");

        // And no upload file_state row for a.txt landed.
        assert!(repo
            .get_file_state(src.id, &rp("a.txt"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn query_activity_rejects_out_of_range_limit() {
        // SPEC s18.8: `limit` must be in 1..=10_000. The pre-fix code
        // silently CLAMPED out-of-range limits (0 -> 1, u32::MAX -> 10_000),
        // accepting caller bugs. It now REJECTS them with a structured
        // `internal.bad_request`. A `limit` of u32::MAX is also what made
        // the old `page * limit` multiply overflow i64 (page == u32::MAX),
        // so rejecting it is both the spec behaviour and the overflow guard.
        let (repo, _dir) = temp_repo().await;

        // limit = 0: below range -> Err.
        let too_small = repo
            .query_activity(ActivityFilter::default(), PageRequest::first(0))
            .await;
        assert!(too_small.is_err(), "limit=0 must be rejected");
        assert!(too_small
            .unwrap_err()
            .to_string()
            .contains("internal.bad_request"));

        // limit = 20_000: above range -> Err.
        let too_big = repo
            .query_activity(ActivityFilter::default(), PageRequest::first(20_000))
            .await;
        assert!(too_big.is_err(), "limit=20_000 must be rejected");

        // limit = u32::MAX: rejected at the limit guard, so no overflow/panic.
        let huge = repo
            .query_activity(ActivityFilter::default(), PageRequest::first(u32::MAX))
            .await;
        assert!(huge.is_err(), "out-of-range limit rejected, no overflow");

        // A valid in-range limit still works.
        let ok = repo
            .query_activity(ActivityFilter::default(), PageRequest::first(50))
            .await
            .expect("in-range limit=50 must succeed");
        assert_eq!(ok.total, 0);
        assert!(ok.rows.is_empty());
    }

    #[tokio::test]
    async fn search_files_escapes_fts_syntax() {
        // P2-1: a raw filename term containing FTS5 operator characters
        // (a hyphen, a quote) must NOT error - the query is escaped into a
        // literal FTS5 phrase before MATCH.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        repo.upsert_file_state(&sample_file(src.id, "foo-bar.txt", 0x01))
            .await
            .unwrap();

        // Hyphen: raw FTS5 would read `-` as a NOT operator and error /
        // mis-search; escaped it matches the adjacent `foo`,`bar` tokens.
        let hits = repo.search_files(None, "foo-bar", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].relative_path.as_str(), "foo-bar.txt");

        // An embedded double-quote must not error (it is doubled when
        // escaped) - just assert no error and no spurious panic.
        let with_quote = repo.search_files(None, "foo\"bar", 10).await.unwrap();
        assert!(with_quote.len() <= 1);
    }

    #[tokio::test]
    async fn list_file_state_under_prefix_returns_subtree() {
        // M8: the Restore tree range-scans file_state under a plaintext prefix.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        for p in [
            "a.txt",
            "src/main.rs",
            "src/lib.rs",
            "src/nested/deep.rs",
            "srcfoo.txt", // NOT under "src/" - the range upper bound must exclude it
            "docs/readme.md",
        ] {
            repo.upsert_file_state(&sample_file(src.id, p, 0x01))
                .await
                .unwrap();
        }

        // Root (empty prefix) sees every file.
        let root = repo
            .list_file_state_under_prefix(src.id, "", 100)
            .await
            .unwrap();
        assert_eq!(root.len(), 6);

        // The "src" subtree sees only the three under src/, NOT "srcfoo.txt".
        let under_src = repo
            .list_file_state_under_prefix(src.id, "src", 100)
            .await
            .unwrap();
        let paths: Vec<&str> = under_src.iter().map(|r| r.relative_path.as_str()).collect();
        assert_eq!(paths, ["src/lib.rs", "src/main.rs", "src/nested/deep.rs"]);

        // The deeper "src/nested" prefix narrows further.
        let nested = repo
            .list_file_state_under_prefix(src.id, "src/nested", 100)
            .await
            .unwrap();
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].relative_path.as_str(), "src/nested/deep.rs");
    }

    #[tokio::test]
    async fn list_file_state_under_prefix_caps_at_limit_plus_one() {
        // The +1 over the limit lets the caller detect truncation.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();
        for i in 0..10 {
            repo.upsert_file_state(&sample_file(src.id, &format!("d/f{i:02}.txt"), 0x01))
                .await
                .unwrap();
        }
        let rows = repo
            .list_file_state_under_prefix(src.id, "d", 3)
            .await
            .unwrap();
        // limit 3 -> at most 4 rows fetched (3 + 1 truncation sentinel).
        assert_eq!(rows.len(), 4);
    }

    #[tokio::test]
    async fn search_files_glob_matches_wildcards() {
        // M8: the glob search path matches `*.rs` style patterns FTS5 cannot.
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();
        for p in ["a.rs", "b.rs", "c.txt", "src/d.rs"] {
            repo.upsert_file_state(&sample_file(src.id, p, 0x01))
                .await
                .unwrap();
        }

        // `*.rs` matches every .rs path (GLOB `*` spans `/`).
        let rs = repo.search_files_glob(None, "*.rs", 100).await.unwrap();
        let mut paths: Vec<&str> = rs.iter().map(|h| h.relative_path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, ["a.rs", "b.rs", "src/d.rs"]);

        // A source-scoped glob still works; a non-matching glob returns nothing.
        let none = repo
            .search_files_glob(Some(src.id), "*.md", 100)
            .await
            .unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn get_pending_ops_for_source_filters() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src_a = sample_source(acct.id);
        let mut src_b = sample_source(acct.id);
        src_b.display_name = "B".into();
        repo.upsert_source(&src_a).await.unwrap();
        repo.upsert_source(&src_b).await.unwrap();

        repo.enqueue_pending_op(NewPendingOp {
            source_id: src_a.id,
            op_type: "upload".into(),
            relative_path: rp("a1.txt"),
            payload_json: serde_json::json!({}),
            scheduled_for: 100,
            created_at: 50,
        })
        .await
        .unwrap();
        repo.enqueue_pending_op(NewPendingOp {
            source_id: src_b.id,
            op_type: "upload".into(),
            relative_path: rp("b1.txt"),
            payload_json: serde_json::json!({}),
            scheduled_for: 100,
            created_at: 50,
        })
        .await
        .unwrap();
        repo.enqueue_pending_op(NewPendingOp {
            source_id: src_a.id,
            op_type: "upload".into(),
            relative_path: rp("a2.txt"),
            payload_json: serde_json::json!({}),
            scheduled_for: 100,
            created_at: 50,
        })
        .await
        .unwrap();

        let a = repo.get_pending_ops_for_source(src_a.id).await.unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].relative_path.as_str(), "a1.txt");
        assert_eq!(a[1].relative_path.as_str(), "a2.txt");
        let b = repo.get_pending_ops_for_source(src_b.id).await.unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].relative_path.as_str(), "b1.txt");
    }

    #[tokio::test]
    async fn delete_activity_by_source_nulls_not_deletes() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        for ts in 0..3 {
            repo.write_activity(NewActivity {
                ts,
                source_id: Some(src.id),
                level: ActivityLevel::Info,
                event_type: "scan_done".into(),
                file_count: None,
                bytes: None,
                message: None,
            })
            .await
            .unwrap();
        }
        // A global row not owned by the source.
        repo.write_activity(NewActivity {
            ts: 5,
            source_id: None,
            level: ActivityLevel::Info,
            event_type: "global".into(),
            file_count: None,
            bytes: None,
            message: None,
        })
        .await
        .unwrap();

        let n = repo.delete_activity_by_source(src.id).await.unwrap();
        assert_eq!(n, 3);

        // Rows are still present, just with source_id NULL.
        let page = repo
            .query_activity(ActivityFilter::default(), PageRequest::first(100))
            .await
            .unwrap();
        assert_eq!(page.total, 4);
        assert!(page.rows.iter().all(|r| r.source_id.is_none()));
    }

    #[tokio::test]
    async fn fts5_prefix_search() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        for p in ["projects/alpha.md", "projects/beta.md", "notes/gamma.md"] {
            repo.upsert_file_state(&sample_file(src.id, p, 0x01))
                .await
                .unwrap();
        }

        let hits = repo.search_files(None, "projects*", 10).await.unwrap();
        assert_eq!(hits.len(), 2);
        for h in &hits {
            assert!(h.relative_path.as_str().starts_with("projects/"));
        }

        let only_notes = repo.search_files(None, "gamma*", 10).await.unwrap();
        assert_eq!(only_notes.len(), 1);
        assert_eq!(only_notes[0].relative_path.as_str(), "notes/gamma.md");
    }

    #[tokio::test]
    async fn search_files_prefix_literal_and_terms() {
        // P2 r2: the per-token MATCH builder must support real prefix
        // queries (`proj*` -> the `*` is applied OUTSIDE the quoted stem),
        // literal terms with FTS5 operator chars (`foo-bar` does not error),
        // and multi-token implicit-AND (`foo bar`). The pre-fix code quoted
        // the WHOLE input as one phrase, so `proj*` became the literal
        // `"proj*"` and matched nothing (the discriminating case the older
        // `projects*`-as-a-whole-token test could not catch).
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        for p in [
            "projects/alpha.md",
            "projects/beta.md",
            "notes/gamma.md",
            "foo-bar.txt",
        ] {
            repo.upsert_file_state(&sample_file(src.id, p, 0x01))
                .await
                .unwrap();
        }

        // Genuine prefix: `proj*` (stem "proj" is NOT a complete token;
        // the token is "projects") must still prefix-match both project
        // files. This is the case the whole-query-quoting fix regressed.
        let proj = repo.search_files(None, "proj*", 10).await.unwrap();
        assert_eq!(proj.len(), 2, "proj* must prefix-match projects/*");
        for h in &proj {
            assert!(h.relative_path.as_str().starts_with("projects/"));
        }

        // Literal hyphenated term must not error (the `-` is not parsed as
        // an FTS5 NOT operator); it matches the adjacent foo/bar tokens.
        let foobar = repo.search_files(None, "foo-bar", 10).await.unwrap();
        assert_eq!(foobar.len(), 1);
        assert_eq!(foobar[0].relative_path.as_str(), "foo-bar.txt");

        // Multi-token implicit AND: both tokens must be present in a hit.
        // "foo bar" -> "foo" AND "bar" -> only foo-bar.txt has both.
        let both = repo.search_files(None, "foo bar", 10).await.unwrap();
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].relative_path.as_str(), "foo-bar.txt");

        // An AND of two tokens that never co-occur returns nothing.
        let none = repo.search_files(None, "alpha beta", 10).await.unwrap();
        assert!(none.is_empty(), "alpha AND beta share no file");

        // Empty / whitespace-only input matches nothing without erroring.
        assert!(repo.search_files(None, "", 10).await.unwrap().is_empty());
        assert!(repo.search_files(None, "   ", 10).await.unwrap().is_empty());
        // A bare `*` token has no stem -> dropped -> empty match.
        assert!(repo.search_files(None, "*", 10).await.unwrap().is_empty());
        // An all-`*` token (`**`) also collapses to an empty stem -> dropped.
        assert!(repo.search_files(None, "**", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fts5_trigger_keeps_index_synced() {
        let (repo, _dir) = temp_repo().await;
        let acct = sample_account();
        repo.upsert_account(&acct).await.unwrap();
        let src = sample_source(acct.id);
        repo.upsert_source(&src).await.unwrap();

        // FTS5 unicode61 tokenizer treats `-` and `.` as separators, so a
        // path like "unique-token.txt" indexes the tokens `unique`, `token`,
        // `txt`. The FTS5 MATCH grammar also reads a bare `-` as NOT, so we
        // search by a single token (with prefix wildcard) rather than the
        // literal hyphenated string.
        let f = sample_file(src.id, "uniquetoken.txt", 0x33);
        repo.upsert_file_state(&f).await.unwrap();
        let hits = repo.search_files(None, "uniquetoken*", 10).await.unwrap();
        assert_eq!(hits.len(), 1);

        repo.delete_file_state(src.id, &rp("uniquetoken.txt"))
            .await
            .unwrap();
        let gone = repo.search_files(None, "uniquetoken*", 10).await.unwrap();
        assert!(gone.is_empty());

        // Update: rename a file's path. Old token should disappear from
        // FTS, new token should appear.
        let renamed = sample_file(src.id, "beforename.txt", 0x44);
        repo.upsert_file_state(&renamed).await.unwrap();
        let mut after = renamed.clone();
        after.relative_path = rp("aftername.txt");
        // Upsert against the new PK first, then delete the old row, since
        // the PK is (source_id, relative_path) and updating the path means
        // a new row from the table's perspective.
        repo.upsert_file_state(&after).await.unwrap();
        repo.delete_file_state(src.id, &rp("beforename.txt"))
            .await
            .unwrap();
        assert_eq!(
            repo.search_files(None, "beforename*", 10)
                .await
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            repo.search_files(None, "aftername*", 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn integrity_check_happy_path() {
        let (_repo, _dir) = temp_repo().await;
        // Reaching here means open() succeeded: migrations ran and the
        // PRAGMA integrity_check returned "ok".
    }
}
