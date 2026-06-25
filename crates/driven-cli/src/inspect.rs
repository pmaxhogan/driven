//! Local-state inspection subcommands (no Drive / network needed).
//!
//! These read the on-disk Driven state database directly via
//! `driven-core`'s [`StateRepo`] surface and print a human summary:
//!
//! - `status`  - each backup source with its `file_state` counts by status,
//!   pending-op count, and last scan / deep-verify times.
//! - `history` - the most recent `activity_log` entries (optionally
//!   warnings-and-errors only).
//! - `verify`  - flags `file_state` rows in a problem status (corrupt /
//!   error); exits non-zero when any are found, so it is scriptable.
//!
//! Each command is split into a pure `gather_*` (queries the repo, returns
//! data) and a `render_*` / `run_*` (opens the DB, prints) so the query
//! logic is unit-tested against a seeded temporary database without
//! capturing stdout.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use driven_core::state::{
    ActivityFilter, ActivityLevel, ActivityRow, PageRequest, SqliteStateRepo, StateRepo,
};
use driven_core::types::{FileStateStatus, UnixMs};

/// Shared args for the read-only inspection commands. The default state-db
/// path matches the dev DB at the repo root (`/state.db`, gitignored).
#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Path to the Driven state database.
    #[arg(long, default_value = "state.db")]
    pub db: PathBuf,
}

/// Args for `driven-cli history`.
#[derive(Debug, Args)]
pub struct HistoryArgs {
    /// Path to the Driven state database.
    #[arg(long, default_value = "state.db")]
    pub db: PathBuf,
    /// Maximum number of activity rows to show (newest first).
    #[arg(long, default_value_t = 20)]
    pub limit: u32,
    /// Only show warnings and errors.
    #[arg(long)]
    pub errors_only: bool,
}

/// Open an EXISTING state database. Unlike a normal app boot we do not want a
/// stray `status` to silently create an empty DB, so a missing file is an
/// error rather than a fresh-create.
async fn open_existing(db: &Path) -> Result<SqliteStateRepo> {
    if !db.exists() {
        anyhow::bail!(
            "no state database at {} - point --db at Driven's state.db (is Driven configured?)",
            db.display()
        );
    }
    SqliteStateRepo::open(db)
        .await
        .with_context(|| format!("open state database {}", db.display()))
}

/// One source's summary for `status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStatus {
    pub display_name: String,
    pub enabled: bool,
    pub local_path: String,
    pub total_files: u64,
    /// Counts in the fixed order: synced, pending, corrupt, locked, error,
    /// excluded-orphan.
    pub counts: StatusCounts,
    pub pending_ops: u64,
    pub last_full_scan_at: Option<UnixMs>,
    pub last_deep_verify_at: Option<UnixMs>,
}

/// `file_state` counts by status for one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatusCounts {
    pub synced: u64,
    pub pending: u64,
    pub corrupt: u64,
    pub locked: u64,
    pub error: u64,
    pub excluded_orphan: u64,
}

impl StatusCounts {
    fn tally(&mut self, status: FileStateStatus) {
        match status {
            FileStateStatus::Synced => self.synced += 1,
            FileStateStatus::Pending => self.pending += 1,
            FileStateStatus::Corrupt => self.corrupt += 1,
            FileStateStatus::Locked => self.locked += 1,
            FileStateStatus::Error => self.error += 1,
            FileStateStatus::ExcludedOrphan => self.excluded_orphan += 1,
        }
    }

    /// Rows that need attention (a real, non-transient problem).
    fn problems(&self) -> u64 {
        self.corrupt + self.error
    }
}

/// Gather the per-source status rows (pure query; testable).
pub async fn gather_status(repo: &dyn StateRepo) -> Result<Vec<SourceStatus>> {
    let mut out = Vec::new();
    for s in repo.list_sources().await? {
        let file_state = repo.load_source_file_state(s.id).await?;
        let mut counts = StatusCounts::default();
        for row in file_state.values() {
            counts.tally(row.status);
        }
        let pending_ops = repo.get_pending_ops_for_source(s.id).await?.len() as u64;
        out.push(SourceStatus {
            display_name: s.display_name,
            enabled: s.enabled,
            local_path: s.local_path,
            total_files: file_state.len() as u64,
            counts,
            pending_ops,
            last_full_scan_at: s.last_full_scan_at,
            last_deep_verify_at: s.last_deep_verify_at,
        });
    }
    Ok(out)
}

/// Gather recent activity rows (pure query; testable).
pub async fn gather_history(
    repo: &dyn StateRepo,
    limit: u32,
    errors_only: bool,
) -> Result<Vec<ActivityRow>> {
    let filter = ActivityFilter {
        source_id: None,
        since_ms: None,
        before_ms: None,
        min_level: errors_only.then_some(ActivityLevel::Warn),
        event_types: vec![],
    };
    // SPEC s18.8 bounds the page size to 1..=10_000.
    let page = repo
        .query_activity(filter, PageRequest::first(limit.clamp(1, 10_000)))
        .await?;
    Ok(page.rows)
}

pub async fn run_status(args: InspectArgs) -> Result<()> {
    let repo = open_existing(&args.db).await?;
    let rows = gather_status(&repo).await?;
    if rows.is_empty() {
        println!("No backup sources configured in {}.", args.db.display());
        return Ok(());
    }
    for s in &rows {
        let state = if s.enabled { "enabled" } else { "disabled" };
        println!("{} [{}]  {}", s.display_name, state, s.local_path);
        println!(
            "  files: {} total  (synced {}, pending {}, corrupt {}, locked {}, error {}, excluded {})",
            s.total_files,
            s.counts.synced,
            s.counts.pending,
            s.counts.corrupt,
            s.counts.locked,
            s.counts.error,
            s.counts.excluded_orphan,
        );
        println!("  pending ops: {}", s.pending_ops);
        println!(
            "  last full scan: {}   last deep-verify: {}",
            fmt_epoch_ms(s.last_full_scan_at),
            fmt_epoch_ms(s.last_deep_verify_at),
        );
    }
    Ok(())
}

pub async fn run_history(args: HistoryArgs) -> Result<()> {
    let repo = open_existing(&args.db).await?;
    let rows = gather_history(&repo, args.limit, args.errors_only).await?;
    if rows.is_empty() {
        println!("No activity recorded.");
        return Ok(());
    }
    for r in &rows {
        let mut line = format!(
            "{}  {:<5}  {}",
            fmt_epoch_ms(Some(r.ts)),
            level_label(r.level),
            r.event_type,
        );
        if let Some(msg) = &r.message {
            line.push_str(&format!("  {msg}"));
        }
        if let Some(n) = r.file_count {
            line.push_str(&format!("  (files: {n})"));
        }
        println!("{line}");
    }
    Ok(())
}

pub async fn run_verify(args: InspectArgs) -> Result<()> {
    let repo = open_existing(&args.db).await?;
    let rows = gather_status(&repo).await?;
    let mut problems = 0u64;
    for s in &rows {
        let p = s.counts.problems();
        problems += p;
        if p > 0 {
            println!(
                "{}: {} file(s) need attention (corrupt {}, error {})",
                s.display_name, p, s.counts.corrupt, s.counts.error,
            );
        }
    }
    if problems == 0 {
        println!(
            "OK - no files in a corrupt or error state across {} source(s).",
            rows.len()
        );
        Ok(())
    } else {
        // Non-zero exit so the command is scriptable.
        anyhow::bail!("{problems} file(s) in a problem state");
    }
}

/// Severity label for an activity row.
fn level_label(level: ActivityLevel) -> &'static str {
    match level {
        ActivityLevel::Info => "INFO",
        ActivityLevel::Warn => "WARN",
        ActivityLevel::Error => "ERROR",
    }
}

/// Format a Unix-epoch-millisecond instant as `YYYY-MM-DDThh:mm:ssZ`, or
/// `never` for `None`. Kept dependency-free (no chrono) like the rest of the
/// CLI; the civil-date split is Howard Hinnant's `civil_from_days`.
fn fmt_epoch_ms(ms: Option<UnixMs>) -> String {
    let Some(ms) = ms else {
        return "never".to_string();
    };
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 -> (year, month,
/// day). Valid across the full `i64` range used here.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    use driven_core::state::{AccountRow, FileStateRow, NewActivity, SourceRow};
    use driven_core::types::{AccountId, AccountState, FileStateStatus, RelativePath, SourceId};

    async fn temp_repo() -> (SqliteStateRepo, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let repo = SqliteStateRepo::open(&dir.path().join("state.db"))
            .await
            .unwrap();
        (repo, dir)
    }

    fn account(id: AccountId) -> AccountRow {
        AccountRow {
            id,
            email: "t@example.com".into(),
            display_name: None,
            state: AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        }
    }

    fn source(account: AccountId, id: SourceId, name: &str) -> SourceRow {
        SourceRow {
            id,
            account_id: account,
            display_name: name.into(),
            enabled: true,
            local_path: format!("/data/{name}"),
            drive_folder_id: "f".into(),
            drive_folder_path: "/f".into(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: Some(0),
            last_deep_verify_at: None,
            created_at: 0,
        }
    }

    fn file_state(source: SourceId, path: &str, status: FileStateStatus) -> FileStateRow {
        FileStateRow {
            source_id: source,
            relative_path: RelativePath::try_from(path.to_string()).unwrap(),
            size: 1,
            mtime_ns: 0,
            hash_blake3: [0u8; 32],
            drive_file_id: None,
            drive_md5: None,
            encrypted_remote_path: None,
            status,
            last_uploaded_at: None,
            last_verified_at: None,
        }
    }

    #[tokio::test]
    async fn gather_status_counts_file_state_by_status() {
        let (repo, _dir) = temp_repo().await;
        let acc = AccountId::new_v4();
        let src = SourceId::new_v4();
        repo.upsert_account(&account(acc)).await.unwrap();
        repo.upsert_source(&source(acc, src, "Docs")).await.unwrap();
        for (p, st) in [
            ("a", FileStateStatus::Synced),
            ("b", FileStateStatus::Synced),
            ("c", FileStateStatus::Pending),
            ("d", FileStateStatus::Corrupt),
            ("e", FileStateStatus::Error),
        ] {
            repo.upsert_file_state(&file_state(src, p, st))
                .await
                .unwrap();
        }

        let rows = gather_status(&repo).await.unwrap();
        assert_eq!(rows.len(), 1);
        let s = &rows[0];
        assert_eq!(s.display_name, "Docs");
        assert_eq!(s.total_files, 5);
        assert_eq!(s.counts.synced, 2);
        assert_eq!(s.counts.pending, 1);
        assert_eq!(s.counts.corrupt, 1);
        assert_eq!(s.counts.error, 1);
        assert_eq!(s.counts.problems(), 2);
    }

    #[tokio::test]
    async fn gather_status_empty_when_no_sources() {
        let (repo, _dir) = temp_repo().await;
        assert!(gather_status(&repo).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn gather_history_respects_limit_and_errors_only() {
        let (repo, _dir) = temp_repo().await;
        let acc = AccountId::new_v4();
        let src = SourceId::new_v4();
        repo.upsert_account(&account(acc)).await.unwrap();
        repo.upsert_source(&source(acc, src, "Docs")).await.unwrap();
        for (i, level) in [
            ActivityLevel::Info,
            ActivityLevel::Warn,
            ActivityLevel::Error,
            ActivityLevel::Info,
        ]
        .into_iter()
        .enumerate()
        {
            repo.write_activity(NewActivity {
                ts: i as i64 + 1,
                source_id: Some(src),
                level,
                event_type: "scan.complete".into(),
                file_count: Some(i as u64),
                bytes: None,
                message: None,
            })
            .await
            .unwrap();
        }

        // Newest-first, limited.
        let all = gather_history(&repo, 2, false).await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].ts >= all[1].ts, "rows are newest-first");

        // errors_only -> Warn + Error only (min_level = Warn).
        let problems = gather_history(&repo, 100, true).await.unwrap();
        assert_eq!(problems.len(), 2);
        assert!(problems
            .iter()
            .all(|r| matches!(r.level, ActivityLevel::Warn | ActivityLevel::Error)));
    }

    #[test]
    fn fmt_epoch_ms_formats_known_instants() {
        assert_eq!(fmt_epoch_ms(None), "never");
        assert_eq!(fmt_epoch_ms(Some(0)), "1970-01-01T00:00:00Z");
        // 2024-01-01T00:00:00Z = 1_704_067_200 s.
        assert_eq!(
            fmt_epoch_ms(Some(1_704_067_200_000)),
            "2024-01-01T00:00:00Z"
        );
        // A time-of-day check: +13h 37m 42s.
        assert_eq!(
            fmt_epoch_ms(Some(1_704_067_200_000 + (13 * 3600 + 37 * 60 + 42) * 1000)),
            "2024-01-01T13:37:42Z"
        );
    }

    /// Seed a database at `path` with one source, a clean + a corrupt file,
    /// and one activity row, then close the connection so `run_*` can reopen.
    async fn seed_at(path: &Path) {
        let repo = SqliteStateRepo::open(path).await.unwrap();
        let acc = AccountId::new_v4();
        let src = SourceId::new_v4();
        repo.upsert_account(&account(acc)).await.unwrap();
        repo.upsert_source(&source(acc, src, "Docs")).await.unwrap();
        repo.upsert_file_state(&file_state(src, "ok", FileStateStatus::Synced))
            .await
            .unwrap();
        repo.upsert_file_state(&file_state(src, "bad", FileStateStatus::Corrupt))
            .await
            .unwrap();
        repo.write_activity(NewActivity {
            ts: 1,
            source_id: Some(src),
            level: ActivityLevel::Info,
            event_type: "scan.complete".into(),
            file_count: Some(2),
            bytes: None,
            message: Some("done".into()),
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_status_and_history_render_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        seed_at(&path).await;
        run_status(InspectArgs { db: path.clone() }).await.unwrap();
        run_history(HistoryArgs {
            db: path,
            limit: 10,
            errors_only: false,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_verify_errors_when_a_corrupt_file_is_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        seed_at(&path).await; // includes a Corrupt row
        assert!(run_verify(InspectArgs { db: path }).await.is_err());
    }

    #[tokio::test]
    async fn run_verify_ok_and_status_empty_on_clean_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        // A freshly-migrated DB with no sources: status reports none, verify OK.
        drop(SqliteStateRepo::open(&path).await.unwrap());
        run_status(InspectArgs { db: path.clone() }).await.unwrap();
        run_verify(InspectArgs { db: path }).await.unwrap();
    }

    #[tokio::test]
    async fn open_existing_missing_db_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open_existing(&dir.path().join("absent.db")).await.is_err());
    }
}
