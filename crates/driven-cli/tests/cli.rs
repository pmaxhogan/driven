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
        backend_kind: driven_core::state::BackendKind::GoogleDrive,
        backend_config_json: None,
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
        drive_id: None,
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
        mtime_granularity_ns: None,
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
            .and(predicate::str::contains("verify"))
            .and(predicate::str::contains("rclone"))
            .and(predicate::str::contains("scrub"))
            .and(predicate::str::contains("restore")),
    );
}

/// Seed `path` with one source and `runs` recorded scrub reports.
fn seed_scrub_runs(path: &Path, runs: &[driven_core::scrub::ScrubReport]) {
    block_on(async {
        let repo = SqliteStateRepo::open(path).await.unwrap();
        let acc = AccountId::new_v4();
        let src = SourceId::new_v4();
        repo.upsert_account(&account(acc)).await.unwrap();
        repo.upsert_source(&source(acc, src, "Docs")).await.unwrap();
        for (i, report) in runs.iter().enumerate() {
            repo.insert_scrub_run(&driven_core::state::NewScrubRun {
                source_id: src,
                started_at: i as i64 + 1,
                finished_at: i as i64 + 2,
                report: report.clone(),
            })
            .await
            .unwrap();
        }
    });
}

#[test]
fn scrub_on_a_fresh_database_reports_the_shipped_policy_and_no_runs() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed_empty(&db);

    cli()
        .args(["scrub", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(
            // The shipped posture: on, weekly, metadata-only.
            predicate::str::contains("integrity scrub: enabled")
                .and(predicate::str::contains("every 604800s"))
                .and(predicate::str::contains("deep sample 0"))
                .and(predicate::str::contains("No scrub has run yet.")),
        );
}

#[test]
fn scrub_renders_recorded_runs_newest_first_with_counts_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let mut clean = driven_core::scrub::ScrubReport {
        checked: 40,
        ok: 40,
        ..Default::default()
    };
    clean.finish();
    let mut drifted = driven_core::scrub::ScrubReport {
        checked: 40,
        ok: 38,
        missing: 1,
        hash_mismatch: 1,
        healed: 1,
        unrecoverable: 1,
        ..Default::default()
    };
    drifted.finish();
    seed_scrub_runs(&db, &[clean, drifted]);

    cli()
        .args(["scrub", "--db"])
        .arg(&db)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("drift")
                .and(predicate::str::contains("clean"))
                .and(predicate::str::contains("checked 40"))
                .and(predicate::str::contains("unrecoverable 1"))
                .and(predicate::str::contains("Docs")),
        );
}

/// `--fail-on-drift` makes the command usable as a monitoring check.
#[test]
fn scrub_fail_on_drift_exits_non_zero_only_when_the_latest_run_still_has_unrepaired_drift() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let mut drifted = driven_core::scrub::ScrubReport {
        checked: 5,
        ok: 4,
        hash_mismatch: 1,
        unrecoverable: 1,
        ..Default::default()
    };
    drifted.finish();
    seed_scrub_runs(&db, &[drifted]);

    cli()
        .args(["scrub", "--fail-on-drift", "--db"])
        .arg(&db)
        .assert()
        .failure()
        .stderr(predicate::str::contains("could not be repaired"));

    // A LATER clean run clears it: a source that drifted once and has been
    // clean since is not a live problem, and flagging it forever would make the
    // check useless as a monitor.
    let dir2 = tempfile::tempdir().unwrap();
    let db2 = dir2.path().join("state.db");
    let mut clean = driven_core::scrub::ScrubReport {
        checked: 5,
        ok: 5,
        ..Default::default()
    };
    clean.finish();
    seed_scrub_runs(&db2, &[drifted_report(), clean]);
    cli()
        .args(["scrub", "--fail-on-drift", "--db"])
        .arg(&db2)
        .assert()
        .success();
}

fn drifted_report() -> driven_core::scrub::ScrubReport {
    let mut r = driven_core::scrub::ScrubReport {
        checked: 5,
        ok: 4,
        hash_mismatch: 1,
        unrecoverable: 1,
        ..Default::default()
    };
    r.finish();
    r
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
        "rclone",
        "restore",
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

// ---------------------------------------------------------------------------
// `rclone list` / `rclone import` - the migration path.
//
// The mapping itself is unit-tested exhaustively in `driven-rclone`; these
// prove the SHIPPED binary end to end: argument parsing, reading a real file
// off disk, the exit codes, and - most importantly - that a credential in the
// config does not reach stdout or stderr unless the user explicitly asks.
// ---------------------------------------------------------------------------

/// A realistic `rclone.conf` covering both importable types, an unsupported
/// wrapper, and rclone's native B2 backend. Every credential in it is a
/// recognisable marker so the leak assertions below are exact.
const RCLONE_CONF: &str = r#"# my rclone config

[r2]
type = s3
provider = Cloudflare
access_key_id = 0123456789abcdef
secret_access_key = SUPERSECRETR2KEY
endpoint = https://acct123.r2.cloudflarestorage.com
region = auto
acl = private

[aws]
type = s3
provider = AWS
access_key_id = AKIAIOSFODNN7EXAMPLE
secret_access_key = SUPERSECRETAWSKEY
region = eu-west-2

[gdrive]
type = drive
scope = drive
root_folder_id = 1AbCdEfGhIjKlMnOpQrSt
team_drive = 0ABCdefGHI
token = {"access_token":"ya29.SECRETACCESS","refresh_token":"1//0eSECRETREFRESH"}

[secretstore]
type = crypt
remote = r2:mybucket/enc
password = 7DDMx0sPIbhRhqSXHkNwPQ

[archive]
type = b2
account = 001abc
key = K001xyz
"#;

/// Every credential-shaped string in [`RCLONE_CONF`]. None may appear in output
/// unless `--reveal-secrets` was passed (and the OAuth token never may).
const SECRETS: &[&str] = &[
    "SUPERSECRETR2KEY",
    "SUPERSECRETAWSKEY",
    "1//0eSECRETREFRESH",
    "ya29.SECRETACCESS",
    "7DDMx0sPIbhRhqSXHkNwPQ",
];

fn write_conf(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("rclone.conf");
    std::fs::write(&path, body).unwrap();
    path
}

/// Assert that no marker credential appears anywhere in a command's output.
fn assert_no_secrets(output: &std::process::Output, context: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for secret in SECRETS {
        assert!(
            !stdout.contains(secret),
            "{context}: stdout leaked {secret}:\n{stdout}"
        );
        assert!(
            !stderr.contains(secret),
            "{context}: stderr leaked {secret}:\n{stderr}"
        );
    }
}

#[test]
fn rclone_list_classifies_every_remote() {
    let dir = tempfile::tempdir().unwrap();
    let conf = write_conf(dir.path(), RCLONE_CONF);
    let out = cli()
        .args(["rclone", "list", "--config", conf.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("r2"))
        .stdout(predicate::str::contains("gdrive"))
        .stdout(predicate::str::contains("Google Drive"))
        .stdout(predicate::str::contains("3 of 5 remote(s)"))
        .get_output()
        .clone();
    assert_no_secrets(&out, "rclone list");
}

#[test]
fn rclone_import_redacts_the_secret_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let conf = write_conf(dir.path(), RCLONE_CONF);
    let out = cli()
        .args([
            "rclone",
            "import",
            "r2",
            "--config",
            conf.to_str().unwrap(),
            "--bucket",
            "backups",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "https://acct123.r2.cloudflarestorage.com",
        ))
        .stdout(predicate::str::contains("0123456789abcdef"))
        .stdout(predicate::str::contains("<redacted>"))
        .stdout(predicate::str::contains("\"bucket\":\"backups\""))
        .get_output()
        .clone();
    assert_no_secrets(&out, "rclone import (default)");
}

#[test]
fn rclone_import_prints_the_secret_only_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let conf = write_conf(dir.path(), RCLONE_CONF);
    cli()
        .args([
            "rclone",
            "import",
            "r2",
            "--config",
            conf.to_str().unwrap(),
            "--reveal-secrets",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("SUPERSECRETR2KEY"));
}

#[test]
fn rclone_import_never_prints_the_drive_oauth_token_even_with_reveal_secrets() {
    // The token cannot authorize Driven, so printing it would be pure exposure.
    let dir = tempfile::tempdir().unwrap();
    let conf = write_conf(dir.path(), RCLONE_CONF);
    for extra in [Vec::new(), vec!["--reveal-secrets"]] {
        let mut args = vec![
            "rclone",
            "import",
            "gdrive",
            "--config",
            conf.to_str().unwrap(),
        ];
        args.extend(extra.iter().copied());
        let out = cli()
            .args(&args)
            .assert()
            .success()
            .stdout(predicate::str::contains("1AbCdEfGhIjKlMnOpQrSt"))
            .stdout(predicate::str::contains("RFC 6749"))
            .get_output()
            .clone();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains("1//0eSECRETREFRESH") && !stdout.contains("ya29.SECRETACCESS"),
            "the OAuth token leaked (args {args:?}):\n{stdout}"
        );
    }
}

#[test]
fn rclone_import_json_derives_the_aws_endpoint_and_stays_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let conf = write_conf(dir.path(), RCLONE_CONF);
    let out = cli()
        .args([
            "rclone",
            "import",
            "aws",
            "--config",
            conf.to_str().unwrap(),
            "--bucket",
            "b",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\"backend\": \"s3\""), "{stdout}");
    assert!(stdout.contains("\"importable\": true"), "{stdout}");
    assert!(
        stdout.contains("\"endpoint\": \"https://s3.eu-west-2.amazonaws.com\""),
        "the AWS regional endpoint must be derived from the region:\n{stdout}"
    );
    assert!(
        stdout.contains("\"pathStyle\": false"),
        "AWS is virtual-host style:\n{stdout}"
    );
    assert_no_secrets(&out, "rclone import --json");
}

#[test]
fn rclone_import_of_an_unknown_remote_lists_the_names_it_does_have() {
    let dir = tempfile::tempdir().unwrap();
    let conf = write_conf(dir.path(), RCLONE_CONF);
    let out = cli()
        .args([
            "rclone",
            "import",
            "nope",
            "--config",
            conf.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no remote named"))
        .stderr(predicate::str::contains("r2"))
        .get_output()
        .clone();
    assert_no_secrets(&out, "rclone import (unknown remote)");
}

#[test]
fn rclone_refuses_a_whole_file_encrypted_config_without_asking_for_a_password() {
    let dir = tempfile::tempdir().unwrap();
    let conf = write_conf(
        dir.path(),
        "# Encrypted rclone configuration File\n\nRCLONE_ENCRYPT_V0:\nc2VjcmV0Ym9keQ\n",
    );
    cli()
        .args(["rclone", "list", "--config", conf.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("rclone config show"))
        // The ciphertext body must not be echoed back at the user.
        .stderr(predicate::str::contains("c2VjcmV0Ym9keQ").not());
}

#[test]
fn rclone_reports_a_missing_config_file_with_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.conf");
    cli()
        .args(["rclone", "list", "--config", missing.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nope.conf"));
}

#[test]
fn rclone_reports_a_malformed_config_by_line_number_only() {
    let dir = tempfile::tempdir().unwrap();
    let conf = write_conf(
        dir.path(),
        "[a]\ntype = s3\nsecret_access_key = SUPERSECRETR2KEY\nthis line is broken\n",
    );
    let out = cli()
        .args(["rclone", "list", "--config", conf.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("line 4"))
        .get_output()
        .clone();
    assert_no_secrets(&out, "rclone list (malformed)");
}

#[test]
fn rclone_list_of_an_empty_config_succeeds_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let conf = write_conf(dir.path(), "# nothing configured yet\n");
    cli()
        .args(["rclone", "list", "--config", conf.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("No remotes found"));
}

#[test]
fn rclone_subcommands_require_their_arguments() {
    cli().args(["rclone"]).assert().failure();
    cli().args(["rclone", "import"]).assert().failure();
    cli()
        .args(["rclone", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("import"));
}

// ---------------------------------------------------------------------------
// restore: argument plumbing and source resolution.
//
// These stop BEFORE the store is built - resolution happens first - so nothing
// here touches the OS keychain or the network. The restore engine itself is
// covered by the round-trip acceptance suite in
// `driven-core/tests/restore_fetch.rs`.
// ---------------------------------------------------------------------------

/// Seed `path` with one account and two sources, so a `restore` with no
/// selector is genuinely ambiguous.
fn seed_two_sources(path: &Path) -> (SourceId, SourceId) {
    let (a, b) = (SourceId::new_v4(), SourceId::new_v4());
    block_on(async {
        let repo = SqliteStateRepo::open(path).await.unwrap();
        let acc = AccountId::new_v4();
        repo.upsert_account(&account(acc)).await.unwrap();
        repo.upsert_source(&source(acc, a, "Docs")).await.unwrap();
        repo.upsert_source(&source(acc, b, "Photos")).await.unwrap();
    });
    (a, b)
}

#[test]
fn restore_requires_a_destination() {
    cli()
        .args(["restore"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--dest"));
}

#[test]
fn restore_on_missing_db_errors_with_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("absent.db");
    cli()
        .args([
            "restore",
            "--db",
            db.to_str().unwrap(),
            "--dest",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("absent.db"));
}

#[test]
fn restore_rejects_both_source_selectors_at_once() {
    // They name the same thing two ways; accepting both would leave the
    // precedence undefined.
    let dir = tempfile::tempdir().unwrap();
    cli()
        .args([
            "restore",
            "--dest",
            dir.path().to_str().unwrap(),
            "--source-id",
            "00000000-0000-0000-0000-000000000000",
            "--source-path",
            "/data/Docs",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn restore_without_a_selector_lists_the_sources_when_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let (a, b) = seed_two_sources(&db);
    let out = cli()
        .args([
            "restore",
            "--db",
            db.to_str().unwrap(),
            "--dest",
            dir.path().join("out").to_str().unwrap(),
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(out.stderr).unwrap();
    // The error has to be actionable: it must print the ids to pass next.
    assert!(stderr.contains(&a.to_string()), "{stderr}");
    assert!(stderr.contains(&b.to_string()), "{stderr}");
    assert!(stderr.contains("--source-id"), "{stderr}");
}

#[test]
fn restore_reports_an_unknown_source_id() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed_two_sources(&db);
    let missing = "11111111-2222-3333-4444-555555555555";
    cli()
        .args([
            "restore",
            "--db",
            db.to_str().unwrap(),
            "--dest",
            dir.path().join("out").to_str().unwrap(),
            "--source-id",
            missing,
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(missing));
}

#[test]
fn restore_reports_a_malformed_source_id() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed_two_sources(&db);
    cli()
        .args([
            "restore",
            "--db",
            db.to_str().unwrap(),
            "--dest",
            dir.path().join("out").to_str().unwrap(),
            "--source-id",
            "not-a-uuid",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a UUID"));
}

#[test]
fn restore_reports_an_unmatched_source_path() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    seed_two_sources(&db);
    cli()
        .args([
            "restore",
            "--db",
            db.to_str().unwrap(),
            "--dest",
            dir.path().join("out").to_str().unwrap(),
            "--source-path",
            "/data/NoSuchFolder",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("NoSuchFolder"));
}

#[test]
fn restore_help_documents_every_flag() {
    cli().args(["restore", "--help"]).assert().success().stdout(
        predicate::str::contains("--db")
            .and(predicate::str::contains("--source-id"))
            .and(predicate::str::contains("--source-path"))
            .and(predicate::str::contains("--dest"))
            .and(predicate::str::contains("--verify-against"))
            .and(predicate::str::contains("--allow-missing-crypto"))
            .and(predicate::str::contains("--overwrite")),
    );
}
