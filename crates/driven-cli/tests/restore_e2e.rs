//! FULL round trip through the SHIPPED `driven-cli` binary: back a folder up to
//! a real destination, then restore it with `driven-cli restore` and let the
//! command's own `--verify-against` prove the bytes came back.
//!
//! The other CLI tests stop at argument parsing, and the engine's own tests
//! (`driven-core/tests/restore_fetch.rs`) call it in-process. Neither exercises
//! what `restore` actually has to get right at runtime: resolving the source
//! row, building the account's destination through the `driven-backend` factory
//! from a persisted `backend_config_json`, and turning the report into an exit
//! code. This test does, end to end, in a subprocess.
//!
//! The destination is the LOCAL-FOLDER backend - a real `RemoteStore`
//! implementation, not a fake - chosen because it is the one backend that needs
//! no OS-keychain secret, so this runs in headless CI. The upload half uses the
//! real scanner / planner / executor; only the destination differs from a Drive
//! run.

use std::path::Path;
use std::sync::Arc;

use assert_cmd::Command;
use predicates::prelude::*;

use driven_core::executor::{DefaultExecutor, Executor, ExecutorDeps, OpOutcome};
use driven_core::state::{AccountRow, BackendKind, SourceRow, SqliteStateRepo, StateRepo};
use driven_core::time::SystemClock;
use driven_core::types::{AccountId, AccountState, ScanMode, SourceId};

/// A pass-through pacer: this test asserts on restored bytes, not rate shaping.
struct NoopPacer;

#[async_trait::async_trait]
impl driven_core::pacer::Pacer for NoopPacer {
    async fn permit_request(&self) {}
    async fn permit_file_create(&self) {}
    async fn permit_bytes(&self, _n: u64) {}
    fn note_response(&self, _classification: driven_core::pacer::ResponseClass) {}
    fn ceilings(&self) -> driven_core::pacer::PacerCeilings {
        driven_core::pacer::PacerCeilings::default()
    }
}

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(fut)
}

fn write_file(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, contents).unwrap();
}

/// Back `src_dir` up into `backend_root` through the real pipeline, recording
/// everything in a state database at `db`. Returns the source id.
///
/// The account is persisted exactly as the app persists a local-folder account
/// (kind + `backend_config_json` from `prepare_local_folder`), which is what
/// lets the SUBPROCESS rebuild the same destination from the database alone.
fn back_up(db: &Path, src_dir: &Path, backend_root: &Path) -> SourceId {
    block_on(async {
        let repo = SqliteStateRepo::open(db).await.unwrap();
        let account_id = AccountId::new_v4();
        let config_json = driven_backend::prepare_local_folder(backend_root, 0).unwrap();
        repo.upsert_account(&AccountRow {
            backend_kind: BackendKind::LocalFolder,
            backend_config_json: Some(config_json.clone()),
            id: account_id,
            email: "local@example.com".into(),
            display_name: None,
            state: AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        })
        .await
        .unwrap();

        let source_id = SourceId::new_v4();
        let source = SourceRow {
            id: source_id,
            account_id,
            display_name: "Round Trip".into(),
            enabled: true,
            local_path: src_dir.to_string_lossy().into_owned(),
            // A local destination's ids are paths relative to the configured
            // root, so the destination root is the empty path.
            drive_folder_id: driven_backend::picker_root_id(BackendKind::LocalFolder).to_string(),
            drive_id: None,
            drive_folder_path: "/".into(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore: false,
            include_patterns: vec![],
            exclude_patterns: vec![],
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: Some(0),
            mtime_granularity_ns: None,
            created_at: 0,
        };
        repo.upsert_source(&source).await.unwrap();

        // The SAME factory the subprocess will use, from the SAME persisted
        // config - so a config the CLI could not rebuild would fail here too.
        let store = driven_backend::build_store(
            &driven_backend::AccountBackend {
                account_id: account_id.to_string(),
                kind: BackendKind::LocalFolder,
                config_json: Some(config_json),
            },
            driven_backend::BackendContext {
                ca: &driven_drive::CustomCaConfig::none(),
                proxy: &driven_drive::ProxyConfig::system(),
            },
        )
        .unwrap()
        .store()
        .expect("the local-folder backend needs no keychain secret");

        let repo = Arc::new(repo);
        let scan = driven_core::scanner::scan(&source, repo.as_ref(), ScanMode::FastPath)
            .await
            .unwrap();
        let plan = driven_core::planner::plan(
            &source,
            &scan,
            repo.as_ref(),
            0,
            &driven_core::planner::BundleConfig::default(),
        )
        .await
        .unwrap();
        let exec = DefaultExecutor::with_clock(
            ExecutorDeps {
                remote: store,
                state: repo.clone(),
                pacer: Arc::new(NoopPacer),
                crypto: None,
                vss: None,
                network: None,
            },
            Arc::new(SystemClock),
        );
        let out = exec
            .execute(&source, &plan, &|_p| {}, &|_o: &OpOutcome| {
                Box::pin(async {}) as futures::future::BoxFuture<'static, ()>
            })
            .await
            .unwrap();
        assert!(
            out.iter().all(|o| matches!(o, OpOutcome::Done { .. })),
            "the backup half must succeed before restore means anything: {out:?}"
        );
        // Drop the pool so the subprocess opens the file independently.
        drop(exec);
        source_id
    })
}

fn cli() -> Command {
    Command::cargo_bin("driven-cli").expect("driven-cli binary built")
}

/// The headline row: back up, restore through the shipped binary, and have the
/// binary's own `--verify-against` confirm every file matches the original.
#[test]
fn restore_round_trips_a_local_folder_backup_through_the_binary() {
    let home = tempfile::tempdir().unwrap();
    let src_dir = home.path().join("source");
    let backend_root = home.path().join("destination");
    let dest_dir = home.path().join("restored");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&backend_root).unwrap();
    let db = home.path().join("state.db");

    let fixtures: Vec<(&str, Vec<u8>)> = vec![
        ("notes.txt", b"plain top-level bytes".to_vec()),
        ("empty.bin", Vec::new()),
        (
            "deep/nested/data.bin",
            (0..5_000u32).map(|i| (i % 251) as u8).collect(),
        ),
    ];
    for (rel, bytes) in &fixtures {
        write_file(&src_dir, rel, bytes);
    }

    back_up(&db, &src_dir, &backend_root);

    cli()
        .args([
            "restore",
            "--db",
            db.to_str().unwrap(),
            "--dest",
            dest_dir.to_str().unwrap(),
            "--verify-against",
            src_dir.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Restored 3 file(s)")
                .and(predicate::str::contains("0 failed"))
                .and(predicate::str::contains("Verified every restored file")),
        );

    // Independent of what the command reported, the bytes on disk must match.
    for (rel, bytes) in &fixtures {
        let restored = dest_dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        assert_eq!(
            &std::fs::read(&restored).unwrap_or_else(|e| panic!("{}: {e}", restored.display())),
            bytes,
            "restored {rel} differs from the original"
        );
    }
}

/// `--verify-against` is a real gate, not decoration: pointed at a directory
/// whose contents differ, the command must exit non-zero and say which file.
#[test]
fn verify_against_a_mismatched_directory_exits_non_zero() {
    let home = tempfile::tempdir().unwrap();
    let src_dir = home.path().join("source");
    let backend_root = home.path().join("destination");
    let dest_dir = home.path().join("restored");
    let tampered = home.path().join("tampered");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&backend_root).unwrap();
    std::fs::create_dir_all(&tampered).unwrap();
    let db = home.path().join("state.db");

    write_file(&src_dir, "doc.txt", b"the original bytes");
    back_up(&db, &src_dir, &backend_root);
    // Same name, same length, different content - the case a size-only check
    // would wave through.
    write_file(&tampered, "doc.txt", b"the tampered bytes");

    cli()
        .args([
            "restore",
            "--db",
            db.to_str().unwrap(),
            "--dest",
            dest_dir.to_str().unwrap(),
            "--verify-against",
            tampered.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("DIFF doc.txt"))
        .stderr(predicate::str::contains("verification problem"));
}

/// A single-source database needs no selector: the harness use case is one
/// source, and demanding a UUID it can only learn by running another command
/// would make the round trip two steps for no reason.
#[test]
fn restore_uses_the_only_source_when_no_selector_is_given() {
    let home = tempfile::tempdir().unwrap();
    let src_dir = home.path().join("source");
    let backend_root = home.path().join("destination");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&backend_root).unwrap();
    let db = home.path().join("state.db");
    write_file(&src_dir, "only.txt", b"one file, one source");
    let source_id = back_up(&db, &src_dir, &backend_root);

    // No selector at all.
    cli()
        .args([
            "restore",
            "--db",
            db.to_str().unwrap(),
            "--dest",
            home.path().join("out-a").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Restored 1 file(s)"));

    // ... and naming that source explicitly restores the same thing.
    cli()
        .args([
            "restore",
            "--db",
            db.to_str().unwrap(),
            "--dest",
            home.path().join("out-b").to_str().unwrap(),
            "--source-id",
            &source_id.to_string(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Restored 1 file(s)"));

    assert_eq!(
        std::fs::read(home.path().join("out-a/only.txt")).unwrap(),
        std::fs::read(home.path().join("out-b/only.txt")).unwrap()
    );
}

/// Without `--overwrite` a populated destination is left alone and the command
/// fails, so a mis-aimed restore cannot quietly eat a real folder.
#[test]
fn restore_refuses_to_clobber_an_existing_file_without_overwrite() {
    let home = tempfile::tempdir().unwrap();
    let src_dir = home.path().join("source");
    let backend_root = home.path().join("destination");
    let dest_dir = home.path().join("restored");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&backend_root).unwrap();
    std::fs::create_dir_all(&dest_dir).unwrap();
    let db = home.path().join("state.db");

    write_file(&src_dir, "doc.txt", b"the backed-up bytes");
    back_up(&db, &src_dir, &backend_root);
    std::fs::write(dest_dir.join("doc.txt"), b"PRE-EXISTING").unwrap();

    cli()
        .args([
            "restore",
            "--db",
            db.to_str().unwrap(),
            "--dest",
            dest_dir.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("already exists"));
    assert_eq!(
        std::fs::read(dest_dir.join("doc.txt")).unwrap(),
        b"PRE-EXISTING",
        "the existing file must survive"
    );

    cli()
        .args([
            "restore",
            "--db",
            db.to_str().unwrap(),
            "--dest",
            dest_dir.to_str().unwrap(),
            "--overwrite",
        ])
        .assert()
        .success();
    assert_eq!(
        std::fs::read(dest_dir.join("doc.txt")).unwrap(),
        b"the backed-up bytes"
    );
}
