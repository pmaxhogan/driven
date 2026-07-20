//! End-to-end integration tests that run the ACTUAL `driven-cli` binary
//! (via `assert_cmd`) and assert on its stdout / stderr / exit code.
//!
//! The unit tests in `src/inspect.rs` cover the pure `gather_*` / `run_*`
//! helpers in-process; these tests instead prove the shipped executable as a
//! user invokes it: argument parsing for every subcommand, the offline
//! `status` / `history` / `verify` inspection path against a real on-disk
//! state database, and `verify`'s scriptable non-zero exit on corruption.
//!
//! The state database is seeded through `driven-core`'s public `StateRepo`
//! surface (the same way the GUI app creates it), then the connection is
//! dropped so the binary opens the file independently - exercising the real
//! "open an existing Driven state.db" code path.
//!
//! `auth` / `sync` need live Google credentials and a real refresh token
//! (gitignored, not present in CI), so they are NOT run here - their argument
//! parsing and required-argument error paths are covered instead. The live
//! auth/sync round-trip is exercised by the real-Drive e2e contract suite.

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

use driven_core::state::{
    AccountRow, ActivityLevel, FileStateRow, NewActivity, SourceRow, SqliteStateRepo, StateRepo,
};
use driven_core::types::{AccountId, AccountState, FileStateStatus, RelativePath, SourceId};

// ---------------------------------------------------------------------------
// Seeding helpers - build a realistic state.db the way the app would, using
// only the public driven-core surface, then close the pool so the binary can
// reopen the file.
// ---------------------------------------------------------------------------

/// Run an async seeding closure to completion on a fresh single-thread tokio
/// runtime. The integration test harness is synchronous (assert_cmd), so we
/// drive the async repo work via a local runtime rather than `#[tokio::test]`.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(fut)
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
        placeholder_policy: Default::default(),
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

/// Seed `path` with one enabled source ("Docs"), the given `(relative_path,
/// status)` file rows, and a handful of activity-log entries at increasing
/// levels. The repo is dropped at the end so the WAL is flushed and the
/// binary can open the file independently.
fn seed(path: &Path, files: &[(&str, FileStateStatus)]) {
    block_on(async {
        let repo = SqliteStateRepo::open(path).await.unwrap();
        let acc = AccountId::new_v4();
        let src = SourceId::new_v4();
        repo.upsert_account(&account(acc)).await.unwrap();
        repo.upsert_source(&source(acc, src, "Docs")).await.unwrap();
        for (p, st) in files {
            repo.upsert_file_state(&file_state(src, p, *st))
                .await
                .unwrap();
        }
        for (i, (level, event, msg)) in [
            (ActivityLevel::Info, "scan.complete", "scanned the source"),
            (
                ActivityLevel::Warn,
                "upload.retry",
                "transient network error",
            ),
            (ActivityLevel::Error, "upload.failed", "permission denied"),
        ]
        .into_iter()
        .enumerate()
        {
            repo.write_activity(NewActivity {
                ts: i as i64 + 1,
                source_id: Some(src),
                level,
                event_type: event.into(),
                file_count: Some(i as u64 + 1),
                bytes: None,
                message: Some(msg.into()),
            })
            .await
            .unwrap();
        }
        // Dropping `repo` closes the pool (and checkpoints the WAL).
    });
}

/// Create a freshly-migrated but otherwise EMPTY state database at `path`
/// (no accounts, sources, files, or activity), then close it.
fn seed_empty(path: &Path) {
    block_on(async {
        drop(SqliteStateRepo::open(path).await.unwrap());
    });
}

/// A fresh `driven-cli` command bound to the compiled test binary.
fn cli() -> Command {
    Command::cargo_bin("driven-cli").expect("driven-cli binary built")
}

// ---------------------------------------------------------------------------
// Top-level help / version.
// ---------------------------------------------------------------------------

#[test]
fn top_level_help_lists_every_subcommand() {
    cli().arg("--help").assert().success().stdout(
        predicate::str::contains("auth")
            .and(predicate::str::contains("dump-refresh-token"))
            .and(predicate::str::contains("sync"))
            .and(predicate::str::contains("status"))
            .and(predicate::str::contains("history"))
            .and(predicate::str::contains("verify")),
    );
}

#[test]
fn version_flag_prints_version() {
    cli()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("driven-cli"));
}

#[test]
fn no_subcommand_is_an_error() {
    // clap requires a subcommand; invoking with none exits 2 and prints usage.
    cli()
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn unrecognized_subcommand_is_an_error() {
    cli()
        .arg("frobnicate")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

// ---------------------------------------------------------------------------
// Per-subcommand --help parses for every subcommand.
// ---------------------------------------------------------------------------

#[test]
fn every_subcommand_help_parses() {
    for sub in [
        "auth",
        "dump-refresh-token",
        "sync",
        "status",
        "history",
        "verify",
    ] {
        cli()
            .args([sub, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Usage").and(predicate::str::contains(sub)));
    }
}

// ---------------------------------------------------------------------------
// Required-argument error paths for the network subcommands (not run live).
// ---------------------------------------------------------------------------

#[test]
fn auth_requires_account() {
    cli()
        .arg("auth")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--account"));
}

#[test]
fn sync_requires_source_dest_and_account() {
    cli().arg("sync").assert().failure().code(2).stderr(
        predicate::str::contains("--source")
            .and(predicate::str::contains("--dest-folder-id"))
            .and(predicate::str::contains("--account")),
    );
}

#[test]
fn dump_refresh_token_requires_account() {
    cli()
        .arg("dump-refresh-token")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("--account"));
}

// ---------------------------------------------------------------------------
// Missing state database: the inspection commands must error (exit 1), not
// silently create an empty DB.
// ---------------------------------------------------------------------------

#[test]
fn status_on_missing_db_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("absent.db");
    cli()
        .args(["status", "--db", db.to_str().unwrap()])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no state database"));
}

#[test]
fn verify_on_missing_db_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("absent.db");
    cli()
        .args(["verify", "--db", db.to_str().unwrap()])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no state database"));
}

#[test]
fn history_on_missing_db_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("absent.db");
    cli()
        .args(["history", "--db", db.to_str().unwrap()])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no state database"));
}

// ---------------------------------------------------------------------------
// status against an empty and a seeded database.
// ---------------------------------------------------------------------------

#[test]
fn status_on_empty_db_reports_no_sources() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed_empty(&db);
    cli()
        .args(["status", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("No backup sources"));
}

#[test]
fn status_on_seeded_db_shows_source_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed(
        &db,
        &[
            ("a", FileStateStatus::Synced),
            ("b", FileStateStatus::Synced),
            ("c", FileStateStatus::Pending),
        ],
    );
    cli()
        .args(["status", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Docs")
                .and(predicate::str::contains("enabled"))
                .and(predicate::str::contains("files: 3 total"))
                .and(predicate::str::contains("synced 2"))
                .and(predicate::str::contains("pending 1")),
        );
}

// ---------------------------------------------------------------------------
// history against an empty and a seeded database, including --errors-only.
// ---------------------------------------------------------------------------

#[test]
fn history_on_empty_db_reports_no_activity() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed_empty(&db);
    cli()
        .args(["history", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("No activity recorded"));
}

#[test]
fn history_on_seeded_db_shows_all_levels() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed(&db, &[("a", FileStateStatus::Synced)]);
    cli()
        .args(["history", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("INFO")
                .and(predicate::str::contains("WARN"))
                .and(predicate::str::contains("ERROR"))
                .and(predicate::str::contains("scan.complete"))
                .and(predicate::str::contains("upload.failed")),
        );
}

#[test]
fn history_errors_only_filters_out_info() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed(&db, &[("a", FileStateStatus::Synced)]);
    cli()
        .args(["history", "--db", db.to_str().unwrap(), "--errors-only"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("WARN")
                .and(predicate::str::contains("ERROR"))
                .and(predicate::str::contains("INFO").not())
                .and(predicate::str::contains("scan.complete").not()),
        );
}

#[test]
fn history_limit_caps_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed(&db, &[("a", FileStateStatus::Synced)]);
    // Only the single newest row (the Error) should appear with --limit 1.
    let output = cli()
        .args(["history", "--db", db.to_str().unwrap(), "--limit", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(lines, 1, "limit 1 should print exactly one activity row");
    assert!(text.contains("upload.failed"), "newest row first: {text}");
}

// ---------------------------------------------------------------------------
// verify: zero exit on a clean DB, non-zero on corruption.
// ---------------------------------------------------------------------------

#[test]
fn verify_clean_db_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed(
        &db,
        &[
            ("a", FileStateStatus::Synced),
            ("b", FileStateStatus::Synced),
        ],
    );
    cli()
        .args(["verify", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("OK"));
}

#[test]
fn verify_empty_db_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed_empty(&db);
    cli()
        .args(["verify", "--db", db.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("OK"));
}

#[test]
fn verify_corrupt_db_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed(
        &db,
        &[
            ("ok", FileStateStatus::Synced),
            ("bad", FileStateStatus::Corrupt),
            ("worse", FileStateStatus::Error),
        ],
    );
    cli()
        .args(["verify", "--db", db.to_str().unwrap()])
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("need attention"))
        .stderr(predicate::str::contains("problem state"));
}
