//! Backup -> restore ROUND TRIP acceptance for the headless restore engine
//! (`driven_core::restore_fetch`).
//!
//! Every test here drives the REAL upload pipeline (`scanner::scan` ->
//! `planner::plan` -> `DefaultExecutor::execute`) against an
//! [`InMemoryRemoteStore`] and a real [`SqliteStateRepo`] on a throwaway temp
//! DB, then restores into a fresh directory and byte-compares. Seeding the
//! remote and the `file_state` rows by hand would let the engine agree with a
//! fixture that the uploader never produces; going through the executor is what
//! makes "the backup survives a round trip" a real claim.
//!
//! The unit tests inside `restore_fetch.rs` cover the framing decoder in
//! isolation (multi-frame, exact-frame-multiple, empty, wrong key, caps); these
//! cover the wiring: which rows are selected, standalone vs bundle byte sources,
//! and per-file fault isolation.

use std::sync::Arc;

use driven_core::executor::{DefaultExecutor, Executor, ExecutorDeps, OpOutcome};
use driven_core::restore_fetch::{restore_source, FileOutcome, RestoreOptions, SkipReason};
use driven_core::state::{AccountRow, SourceRow, SqliteStateRepo, StateRepo};
use driven_core::types::{AccountId, AccountState, RelativePath, ScanMode, SourceId};

use driven_crypto::key::SourceKey;
use driven_crypto::{DrivenCryptoSuite, SourceCryptoSuite};

use driven_drive::fake::InMemoryRemoteStore;
use driven_drive::remote_store::RemoteStore;

use driven_test_fixtures::clock::FakeClock;

// ---------------------------------------------------------------------------
// Harness (mirrors the helpers at the top of e2e_fake.rs)
// ---------------------------------------------------------------------------

/// A non-blocking pacer, for the same reason `e2e_fake.rs` has one: these rows
/// assert on restored bytes, not on rate shaping, and the real `AimdPacer`
/// deadlocks against a never-advancing `FakeClock` once its burst drains.
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

fn noop_progress(_p: driven_core::types::ExecProgress) {}

fn noop_outcome(_o: &OpOutcome) -> futures::future::BoxFuture<'static, ()> {
    Box::pin(async {})
}

async fn open_state(dir: &std::path::Path) -> Arc<SqliteStateRepo> {
    Arc::new(SqliteStateRepo::open(&dir.join("state.db")).await.unwrap())
}

async fn seed_account(state: &SqliteStateRepo) -> AccountId {
    let id = AccountId::new_v4();
    state
        .upsert_account(&AccountRow {
            backend_kind: driven_core::state::BackendKind::GoogleDrive,
            backend_config_json: None,
            id,
            email: "restore@example.com".into(),
            display_name: None,
            state: AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        })
        .await
        .unwrap();
    id
}

fn source_in(account: AccountId, root: &std::path::Path, folder_id: &str) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id: account,
        display_name: "restore-round-trip".into(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: folder_id.to_string(),
        drive_id: None,
        drive_folder_path: "/restore".into(),
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
    }
}

fn write_file(root: &std::path::Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, contents).unwrap();
}

fn rel(s: &str) -> RelativePath {
    RelativePath::try_from(s.to_string()).unwrap()
}

/// A deterministic pseudo-random body of `len` bytes. Deliberately NOT a
/// constant fill: a decoder that dropped or reordered a frame would still
/// reproduce a run of identical bytes.
fn body(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 31 + seed as u64 * 7) % 251) as u8)
        .collect()
}

/// Run one real scan -> plan -> execute cycle for `src`, returning the outcomes.
async fn sync_once(
    state: &Arc<SqliteStateRepo>,
    remote: &Arc<InMemoryRemoteStore>,
    src: &SourceRow,
    crypto: Option<Arc<dyn SourceCryptoSuite>>,
    bundles: driven_core::planner::BundleConfig,
) -> Vec<OpOutcome> {
    let scan = driven_core::scanner::scan(src, state.as_ref(), ScanMode::FastPath)
        .await
        .unwrap();
    // `i64::MAX / 2` as "now" so the freshly written fixtures still read as cold
    // when a test enables bundling (the coldness gate is mtime-relative).
    let plan = driven_core::planner::plan(src, &scan, state.as_ref(), i64::MAX / 2, &bundles)
        .await
        .unwrap();
    let clock = Arc::new(FakeClock::new());
    let exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer: Arc::new(NoopPacer),
            crypto: crypto.map(|suite| {
                Arc::new(driven_core::crypto_provider::SingleSuiteProvider::new(
                    suite,
                )) as Arc<dyn driven_core::crypto_provider::CryptoProvider>
            }),
            vss: None,
            network: None,
        },
        clock,
    );
    exec.execute(src, &plan, &noop_progress, &noop_outcome)
        .await
        .unwrap()
}

/// Assert that every `(rel, bytes)` fixture exists under `dest` with exactly
/// those bytes, and that no restore temp file was left behind.
fn assert_tree_matches(dest: &std::path::Path, expected: &[(String, Vec<u8>)]) {
    for (path, want) in expected {
        let full = dest.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
        let got = std::fs::read(&full)
            .unwrap_or_else(|e| panic!("restored file {} missing: {e}", full.display()));
        assert_eq!(
            got.len(),
            want.len(),
            "restored {path} has the wrong length"
        );
        assert!(got == *want, "restored {path} differs byte-for-byte");
    }
    // No stray temp files survived: a failed verify must clean up after itself,
    // and a successful one renames rather than leaving the temp behind.
    let mut stray = Vec::new();
    for entry in walk(dest) {
        let name = entry.file_name().unwrap_or_default().to_string_lossy();
        if name.contains(".driven-restore-tmp") {
            stray.push(entry.display().to_string());
        }
    }
    assert!(stray.is_empty(), "temp files left behind: {stray:?}");
}

/// Every file path under `root`, recursively.
fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

/// Plaintext source: nested paths, an empty file, and one body several read
/// buffers long all come back byte-for-byte.
#[tokio::test]
async fn plaintext_round_trip_restores_every_file_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    let fixtures: Vec<(String, Vec<u8>)> = vec![
        ("top.txt".to_string(), b"a top level file".to_vec()),
        ("empty.bin".to_string(), Vec::new()),
        ("nested/deep/inner.bin".to_string(), body(4_096, 3)),
        // Comfortably past the 64 KiB read buffer, so the copy loop iterates.
        ("nested/large.bin".to_string(), body(200_000, 11)),
    ];
    for (path, bytes) in &fixtures {
        write_file(src_dir.path(), path, bytes);
    }

    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    let out = sync_once(
        &state,
        &remote,
        &src,
        None,
        driven_core::planner::BundleConfig::default(),
    )
    .await;
    assert!(
        out.iter().all(|o| matches!(o, OpOutcome::Done { .. })),
        "every upload must succeed before restore is meaningful: {out:?}"
    );

    let report = restore_source(
        state.as_ref(),
        remote.as_ref() as &dyn RemoteStore,
        &src,
        None,
        dest_dir.path(),
        &RestoreOptions::default(),
    )
    .await
    .unwrap();

    assert!(report.ok(), "restore reported failures: {report:?}");
    assert_eq!(report.restored(), fixtures.len());
    assert_eq!(report.skipped(), 0);
    assert_eq!(
        report.bytes_restored(),
        fixtures.iter().map(|(_, b)| b.len() as u64).sum::<u64>()
    );
    assert!(
        report.files.iter().all(|f| matches!(
            f.outcome,
            FileOutcome::Restored {
                from_bundle: false,
                ..
            }
        )),
        "bundling was off, so every byte source is a standalone object"
    );
    assert_tree_matches(dest_dir.path(), &fixtures);
}

/// Encrypted source: the objects on the remote are ciphertext, and the restore
/// engine decrypts them back to the original bytes. The 200 KiB fixture spans
/// SEVERAL ciphertext frames, which is the only way the `decrypt_chunk` /
/// `decrypt_last` boundary logic is exercised at all.
#[tokio::test]
async fn encrypted_round_trip_restores_every_file_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    let fixtures: Vec<(String, Vec<u8>)> = vec![
        ("secret.txt".to_string(), b"the quick brown fox".to_vec()),
        ("nothing.bin".to_string(), Vec::new()),
        ("vault/multi-frame.bin".to_string(), body(200_000, 5)),
    ];
    for (path, bytes) in &fixtures {
        write_file(src_dir.path(), path, bytes);
    }

    // The executor's per-source crypto FAILS CLOSED on `encryption_enabled`, so
    // the row must say encrypted for the ciphertext path to be taken.
    let src = SourceRow {
        encryption_enabled: true,
        ..source_in(account, src_dir.path(), &folder)
    };
    state.upsert_source(&src).await.unwrap();

    let suite: Arc<dyn SourceCryptoSuite> = Arc::new(DrivenCryptoSuite::new(SourceKey::generate()));
    let out = sync_once(
        &state,
        &remote,
        &src,
        Some(suite.clone()),
        driven_core::planner::BundleConfig::default(),
    )
    .await;
    assert!(
        out.iter().all(|o| matches!(o, OpOutcome::Done { .. })),
        "every encrypted upload must succeed: {out:?}"
    );

    // Sanity: what landed on the remote is NOT the plaintext, so the round trip
    // below is genuinely exercising the decryptor.
    let stored = remote
        .download(
            state
                .get_file_state(src.id, &rel("secret.txt"))
                .await
                .unwrap()
                .expect("row")
                .drive_file_id
                .as_deref()
                .expect("standalone object"),
        )
        .await
        .unwrap();
    let mut blob = Vec::new();
    {
        use tokio::io::AsyncReadExt;
        let mut reader = stored.0;
        reader.read_to_end(&mut blob).await.unwrap();
    }
    assert!(
        !blob.windows(3).any(|w| w == b"fox"),
        "the stored object must be ciphertext, not plaintext"
    );

    let report = restore_source(
        state.as_ref(),
        remote.as_ref() as &dyn RemoteStore,
        &src,
        Some(suite.as_ref()),
        dest_dir.path(),
        &RestoreOptions::default(),
    )
    .await
    .unwrap();

    assert!(report.ok(), "restore reported failures: {report:?}");
    assert_eq!(report.restored(), fixtures.len());
    assert_tree_matches(dest_dir.path(), &fixtures);
}

/// Small cold files upload as ONE `.tar.gz`; each member has a NULL
/// `drive_file_id` and is restorable only through its bundle membership. This
/// covers the second byte source end to end, including the per-run bundle cache
/// (four members, one bundle object).
#[tokio::test]
async fn bundled_members_restore_from_the_bundle_object() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    let fixtures: Vec<(String, Vec<u8>)> = (0..4u32)
        .map(|i| {
            (
                format!("logs/f{i}.log"),
                format!("log line {i} - some bytes {i}{i}{i}").into_bytes(),
            )
        })
        .collect();
    for (path, bytes) in &fixtures {
        write_file(src_dir.path(), path, bytes);
    }

    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    let bundles = driven_core::planner::BundleConfig {
        enabled: true,
        min_files: 3,
        min_cold_age_days: 0,
        ..driven_core::planner::BundleConfig::enabled_defaults()
    };
    let out = sync_once(&state, &remote, &src, None, bundles).await;
    assert!(
        out.iter()
            .any(|o| matches!(o, OpOutcome::BundleDone { .. })),
        "the fixtures must upload as one bundle: {out:?}"
    );
    for (path, _) in &fixtures {
        let row = state
            .get_file_state(src.id, &rel(path))
            .await
            .unwrap()
            .expect("member row");
        assert!(
            row.drive_file_id.is_none(),
            "a bundled member has no standalone object, so restore must go through the bundle"
        );
    }

    let report = restore_source(
        state.as_ref(),
        remote.as_ref() as &dyn RemoteStore,
        &src,
        None,
        dest_dir.path(),
        &RestoreOptions::default(),
    )
    .await
    .unwrap();

    assert!(report.ok(), "restore reported failures: {report:?}");
    assert_eq!(report.restored(), fixtures.len());
    assert!(
        report.files.iter().all(|f| matches!(
            f.outcome,
            FileOutcome::Restored {
                from_bundle: true,
                ..
            }
        )),
        "every member's byte source is the bundle: {report:?}"
    );
    assert_tree_matches(dest_dir.path(), &fixtures);
}

/// FAULT ISOLATION: one recorded remote object is gone. Its file must fail with
/// a reported reason while every OTHER file still restores - a restore that
/// aborted the whole run on the first dead object would cost the user the rest
/// of their data for no reason.
#[tokio::test]
async fn a_dead_remote_object_fails_only_its_own_file() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    let fixtures: Vec<(String, Vec<u8>)> = vec![
        ("keep-a.txt".to_string(), b"first survivor".to_vec()),
        ("gone.txt".to_string(), b"this object vanishes".to_vec()),
        ("keep-b.txt".to_string(), b"second survivor".to_vec()),
    ];
    for (path, bytes) in &fixtures {
        write_file(src_dir.path(), path, bytes);
    }
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    sync_once(
        &state,
        &remote,
        &src,
        None,
        driven_core::planner::BundleConfig::default(),
    )
    .await;

    // Repoint ONE row at an id the store has never heard of. This is exactly the
    // shape of the damage the remote-existence audit heals - a `file_state` row
    // whose recorded object is no longer fetchable - and unlike deleting from the
    // fake's map it needs no fake internals.
    let gone = rel("gone.txt");
    let mut row = state
        .get_file_state(src.id, &gone)
        .await
        .unwrap()
        .expect("row");
    row.drive_file_id = Some("no-such-object-id".to_string());
    state.upsert_file_state(&row).await.unwrap();

    let report = restore_source(
        state.as_ref(),
        remote.as_ref() as &dyn RemoteStore,
        &src,
        None,
        dest_dir.path(),
        &RestoreOptions::default(),
    )
    .await
    .unwrap();

    assert!(!report.ok(), "a dead object must sink the run's ok()");
    assert_eq!(report.failed(), 1);
    assert_eq!(report.restored(), 2, "the other files still restore");
    let (path, reason) = report.failures().next().expect("one failure");
    assert_eq!(path.as_str(), "gone.txt");
    assert!(
        reason.contains("no-such-object-id"),
        "the failure must name the object it could not fetch: {reason}"
    );

    // The survivors are on disk with the right bytes, and the failed file left
    // nothing behind - not a partial, not a temp.
    for (path, bytes) in fixtures.iter().filter(|(p, _)| p != "gone.txt") {
        assert_eq!(&std::fs::read(dest_dir.path().join(path)).unwrap(), bytes);
    }
    assert!(
        !dest_dir.path().join("gone.txt").exists(),
        "a failed restore must not leave a file at the destination"
    );
    assert!(
        walk(dest_dir.path())
            .iter()
            .all(|p| !p.to_string_lossy().contains(".driven-restore-tmp")),
        "a failed restore must clean up its temp file"
    );
}

/// A row with no byte source is SKIPPED, not failed: nothing was ever uploaded,
/// so there is nothing for the restore to have got wrong. `ok()` stays true.
#[tokio::test]
async fn never_uploaded_and_unsynced_rows_are_skipped_not_failed() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    write_file(src_dir.path(), "uploaded.txt", b"this one is synced");
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    sync_once(
        &state,
        &remote,
        &src,
        None,
        driven_core::planner::BundleConfig::default(),
    )
    .await;

    // A row that has never been uploaded (no object, no bundle).
    let pending = rel("queued.txt");
    state
        .upsert_file_state(&driven_core::state::FileStateRow {
            source_id: src.id,
            relative_path: pending.clone(),
            size: 12,
            mtime_ns: 0,
            hash_blake3: *blake3::hash(b"not uploaded").as_bytes(),
            drive_file_id: None,
            drive_md5: None,
            encrypted_remote_path: None,
            status: driven_core::types::FileStateStatus::Pending,
            last_uploaded_at: None,
            last_verified_at: None,
        })
        .await
        .unwrap();

    // A row WITH an object whose status says the remote bytes are stale.
    let stale = rel("stale.txt");
    let uploaded = state
        .get_file_state(src.id, &rel("uploaded.txt"))
        .await
        .unwrap()
        .expect("row");
    state
        .upsert_file_state(&driven_core::state::FileStateRow {
            relative_path: stale.clone(),
            status: driven_core::types::FileStateStatus::Corrupt,
            ..uploaded
        })
        .await
        .unwrap();

    let report = restore_source(
        state.as_ref(),
        remote.as_ref() as &dyn RemoteStore,
        &src,
        None,
        dest_dir.path(),
        &RestoreOptions::default(),
    )
    .await
    .unwrap();

    assert!(report.ok(), "skips are not failures: {report:?}");
    assert_eq!(report.restored(), 1);
    assert_eq!(report.skipped(), 2);
    let reason_for = |p: &str| {
        report
            .files
            .iter()
            .find(|f| f.relative_path.as_str() == p)
            .map(|f| f.outcome.clone())
            .expect("reported")
    };
    assert_eq!(
        reason_for("queued.txt"),
        FileOutcome::Skipped(SkipReason::NotUploaded)
    );
    assert_eq!(
        reason_for("stale.txt"),
        FileOutcome::Skipped(SkipReason::NotSynced(
            driven_core::types::FileStateStatus::Corrupt
        ))
    );
    assert!(!dest_dir.path().join("queued.txt").exists());
    assert!(!dest_dir.path().join("stale.txt").exists());
}

/// FAIL CLOSED: restoring an encrypted source with no key material must error
/// out before any download, not quietly write ciphertext to disk under the
/// user's filenames.
#[tokio::test]
async fn an_encrypted_source_without_a_suite_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    let src = SourceRow {
        encryption_enabled: true,
        ..source_in(account, src_dir.path(), &folder)
    };
    state.upsert_source(&src).await.unwrap();

    let err = restore_source(
        state.as_ref(),
        remote.as_ref() as &dyn RemoteStore,
        &src,
        None,
        dest_dir.path(),
        &RestoreOptions::default(),
    )
    .await
    .expect_err("an encrypted source with no suite must be refused");
    assert!(err.to_string().contains("encrypted"), "{err}");

    // The mirror image: handing a suite to a PLAINTEXT source would decrypt
    // objects that carry no crypto header, so it is refused too.
    let plain = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&plain).await.unwrap();
    let suite: Arc<dyn SourceCryptoSuite> = Arc::new(DrivenCryptoSuite::new(SourceKey::generate()));
    let err = restore_source(
        state.as_ref(),
        remote.as_ref() as &dyn RemoteStore,
        &plain,
        Some(suite.as_ref()),
        dest_dir.path(),
        &RestoreOptions::default(),
    )
    .await
    .expect_err("a plaintext source with a suite must be refused");
    assert!(err.to_string().contains("NOT encrypted"), "{err}");
}

/// A destination that already holds the file is NOT clobbered unless the caller
/// asks. A restore aimed at a populated folder should have to say so.
#[tokio::test]
async fn an_existing_destination_file_is_preserved_unless_overwrite_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let dest_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    write_file(src_dir.path(), "doc.txt", b"the backed-up bytes");
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    sync_once(
        &state,
        &remote,
        &src,
        None,
        driven_core::planner::BundleConfig::default(),
    )
    .await;

    std::fs::write(dest_dir.path().join("doc.txt"), b"PRE-EXISTING").unwrap();

    let report = restore_source(
        state.as_ref(),
        remote.as_ref() as &dyn RemoteStore,
        &src,
        None,
        dest_dir.path(),
        &RestoreOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(report.failed(), 1, "the collision is reported, not silent");
    assert_eq!(
        std::fs::read(dest_dir.path().join("doc.txt")).unwrap(),
        b"PRE-EXISTING",
        "the existing file must survive an overwrite-less restore"
    );

    let report = restore_source(
        state.as_ref(),
        remote.as_ref() as &dyn RemoteStore,
        &src,
        None,
        dest_dir.path(),
        &RestoreOptions { overwrite: true },
    )
    .await
    .unwrap();
    assert!(report.ok(), "{report:?}");
    assert_eq!(
        std::fs::read(dest_dir.path().join("doc.txt")).unwrap(),
        b"the backed-up bytes"
    );
}
