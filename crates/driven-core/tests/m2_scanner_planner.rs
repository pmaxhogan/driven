//! M2 ACCEPTANCE snapshot tests - one per ROADMAP M2 row.
//!
//! Each test drives the real M2 pipeline `scan -> plan` (SPEC s6 / s7)
//! against a real temp tree (built with `driven_test_fixtures::tree!`) and a
//! real [`SqliteStateRepo`] on a throwaway temp DB. The `file_state` rows the
//! scanner diffs against are seeded through the production
//! [`StateRepo::upsert_file_state`] path so these tests exercise the actual
//! SQLite encode/decode round-trip, not an in-memory fake.
//!
//! ## Why no `RemoteStore` here
//!
//! `scan(source, state, mode)` and `plan(source, scan, state, now, bundle)` are both
//! remote-free: the scanner reads only the local tree + `file_state`, and the
//! planner is a pure fold over the [`ScanResult`] plus two `file_state`
//! lookups. A Drive `file_id` is just a `String` carried on a seeded
//! `file_state` row - no live remote object backs it. The remote is not
//! consumed until M3 execution, so the task brief's "InMemoryRemoteStore
//! where a remote is needed" is vacuous for the M2 scanner+planner rows and
//! the `driven-drive` fake is deliberately not pulled in (see this crate's
//! `[dev-dependencies]` note).
//!
//! ## Map of ROADMAP M2 rows -> tests
//!
//! - First scan, empty remote -> all uploads, no deletes:
//!   [`first_scan_empty_remote_all_uploads`]
//! - Unchanged scan -> empty plan: [`unchanged_scan_empty_plan`]
//! - Single mtime change -> one upload: [`single_mtime_change_one_upload`]
//! - Single local delete -> one trash: [`single_local_delete_one_trash`]
//! - Rename -> one upload + one trash (no-detect):
//!   [`rename_one_upload_one_trash`]
//! - gitignore respected (`node_modules/foo.js` excluded):
//!   [`gitignore_respected_node_modules_excluded`]
//! - `!.env` override re-includes `.env`: [`env_override_reincludes_dotenv`]
//! - gitignore `!Thumbs.db` beats the default exclude (F5):
//!   [`gitignore_reinclude_beats_default_exclude`]
//! - `*.log` exclude wins: [`exclude_pattern_log_wins`]
//! - Excluded-orphan: a now-ignored backed-up file is NOT trashed (DESIGN
//!   s5.5, F1): [`ignore_change_yields_excluded_orphan_no_trash`]
//! - Deep-verify catches bit-rot: [`deep_verify_catches_bit_rot`]
//! - Symlink skipped (policy doc): [`symlink_skipped`] (unix-only)

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use driven_core::planner::{plan, BundleConfig};
use driven_core::scanner::scan;
use driven_core::state::{AccountRow, FileStateRow, SourceRow, SqliteStateRepo, StateRepo};
use driven_core::types::{
    AccountId, AccountState, FileStateStatus, Op, RelativePath, ScanMode, SourceId,
};
use driven_test_fixtures::tree;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Build a `RelativePath` for a test literal (M2 validator is live).
fn rel(s: &str) -> RelativePath {
    RelativePath::try_from(s.to_string()).expect("valid relative path")
}

/// Open a fresh `SqliteStateRepo` on a throwaway temp DB. The returned
/// `TempDir` must outlive the repo (dropping it removes the DB file).
async fn temp_repo() -> (SqliteStateRepo, TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("state.db");
    let repo = SqliteStateRepo::open(&path).await.expect("open state db");
    (repo, dir)
}

/// An `accounts` row the seeded source can FK to.
fn account() -> AccountRow {
    AccountRow {
        id: AccountId::new_v4(),
        email: "m2@example.com".into(),
        display_name: Some("M2".into()),
        state: AccountState::Ok,
        encryption_master_key_id: None,
        created_at: 1_700_000_000_000,
        last_synced_at: None,
    }
}

/// A `backup_sources` row rooted at `root` with the given rule knobs.
fn source_at(
    account_id: AccountId,
    root: &Path,
    respect_gitignore: bool,
    include: &[&str],
    exclude: &[&str],
) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id,
        display_name: "M2 source".into(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: "folder-m2".into(),
        drive_folder_path: "/Driven/M2".into(),
        encryption_enabled: false,
        wrapped_source_key: None,
        respect_gitignore,
        include_patterns: include.iter().map(|s| s.to_string()).collect(),
        exclude_patterns: exclude.iter().map(|s| s.to_string()).collect(),
        placeholder_policy: Default::default(),
        schedule_json_v2_reserved: None,
        deep_verify_interval_secs: 604_800,
        last_full_scan_at: None,
        last_deep_verify_at: None,
        created_at: 1_700_000_000_000,
    }
}

/// Seed an account + source so a `file_state` row can be inserted under the
/// schema's FK chain. Returns the persisted [`SourceRow`].
async fn seed_source(
    repo: &SqliteStateRepo,
    root: &Path,
    respect_gitignore: bool,
    include: &[&str],
    exclude: &[&str],
) -> SourceRow {
    let acct = account();
    repo.upsert_account(&acct).await.expect("upsert account");
    let src = source_at(acct.id, root, respect_gitignore, include, exclude);
    repo.upsert_source(&src).await.expect("upsert source");
    src
}

/// The on-disk `(size, mtime_ns)` the scanner will observe for `path`, so a
/// seeded `file_state` row can be made to match (or deliberately differ).
fn stat_of(path: &Path) -> (u64, i64) {
    let meta = fs::metadata(path).expect("stat");
    let size = meta.len();
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    (size, mtime_ns)
}

/// A `file_state` row. `drive_file_id` present => "reached Drive" (a delete
/// of this path plans a trash); absent => "never uploaded" (a delete drops
/// the row, no op). `hash` is the stored BLAKE3 the deep-verify pass compares.
fn file_row(
    source_id: SourceId,
    rel_path: &str,
    size: u64,
    mtime_ns: i64,
    hash: [u8; 32],
    drive_file_id: Option<&str>,
) -> FileStateRow {
    FileStateRow {
        source_id,
        relative_path: rel(rel_path),
        size,
        mtime_ns,
        hash_blake3: hash,
        drive_file_id: drive_file_id.map(|s| s.to_string()),
        drive_md5: None,
        encrypted_remote_path: None,
        status: if drive_file_id.is_some() {
            FileStateStatus::Synced
        } else {
            FileStateStatus::Pending
        },
        last_uploaded_at: drive_file_id.map(|_| 1),
        last_verified_at: None,
    }
}

/// Collect the relative-path strings of every `HashThenUpload` op in a plan.
fn upload_paths(ops: &[Op]) -> HashSet<String> {
    ops.iter()
        .filter_map(|op| match op {
            Op::HashThenUpload { relative_path, .. } => Some(relative_path.as_str().to_string()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ROADMAP M2 rows
// ---------------------------------------------------------------------------

/// First scan, empty remote: every observed file is new -> plan is all
/// uploads, no trashes (ROADMAP M2 "First scan, empty remote").
#[tokio::test]
async fn first_scan_empty_remote_all_uploads() {
    let dir = tree! {
        "a.txt" => "hello",
        "sub" => { "b.txt" => "world" },
    };
    let (repo, _db) = temp_repo().await;
    // No file_state rows seeded => empty remote.
    let src = seed_source(&repo, dir.path(), true, &[], &[]).await;

    let scanned = scan(&src, &repo, ScanMode::FastPath).await.unwrap();
    let p = plan(&src, &scanned, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();

    let summary = p.summary();
    assert_eq!(summary.uploads, 2, "both files upload: {:?}", p.ops);
    assert_eq!(summary.trashes, 0, "nothing to trash on first scan");
    assert_eq!(
        upload_paths(&p.ops),
        HashSet::from(["a.txt".to_string(), "sub/b.txt".to_string()])
    );
}

/// Unchanged scan: every file's `(size, mtime_ns)` matches its seeded row ->
/// the scan diff is empty and the plan is empty (ROADMAP M2 "Unchanged
/// scan").
#[tokio::test]
async fn unchanged_scan_empty_plan() {
    let dir = tree! { "a.txt" => "hello" };
    let (repo, _db) = temp_repo().await;
    let src = seed_source(&repo, dir.path(), true, &[], &[]).await;

    let (size, mtime) = stat_of(&dir.path().join("a.txt"));
    repo.upsert_file_state(&file_row(
        src.id,
        "a.txt",
        size,
        mtime,
        *blake3::hash(b"hello").as_bytes(),
        Some("drive-a"),
    ))
    .await
    .unwrap();

    let scanned = scan(&src, &repo, ScanMode::FastPath).await.unwrap();
    let p = plan(&src, &scanned, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();

    assert!(
        p.ops.is_empty(),
        "unchanged tree => empty plan: {:?}",
        p.ops
    );
}

/// Single mtime change: the stored row's mtime differs by one ns, so the
/// fast path flags the file changed -> exactly one upload (ROADMAP M2
/// "Single file mtime change").
#[tokio::test]
async fn single_mtime_change_one_upload() {
    let dir = tree! { "a.txt" => "hello" };
    let (repo, _db) = temp_repo().await;
    let src = seed_source(&repo, dir.path(), true, &[], &[]).await;

    let (size, mtime) = stat_of(&dir.path().join("a.txt"));
    // Stored mtime off by one ns => changed under FastPath.
    repo.upsert_file_state(&file_row(
        src.id,
        "a.txt",
        size,
        mtime + 1,
        *blake3::hash(b"hello").as_bytes(),
        Some("drive-a"),
    ))
    .await
    .unwrap();

    let scanned = scan(&src, &repo, ScanMode::FastPath).await.unwrap();
    let p = plan(&src, &scanned, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();

    let summary = p.summary();
    assert_eq!(summary.uploads, 1, "{:?}", p.ops);
    assert_eq!(summary.trashes, 0);
    assert_eq!(upload_paths(&p.ops), HashSet::from(["a.txt".to_string()]));
}

/// Single local delete: a seeded row whose file is gone from disk and which
/// has a `drive_file_id` -> exactly one trash targeting that remote object
/// (ROADMAP M2 "Single file deleted locally").
#[tokio::test]
async fn single_local_delete_one_trash() {
    // `present.txt` stays; `gone.txt` is seeded but never written to disk.
    let dir = tree! { "present.txt" => "x" };
    let (repo, _db) = temp_repo().await;
    let src = seed_source(&repo, dir.path(), true, &[], &[]).await;

    let (size, mtime) = stat_of(&dir.path().join("present.txt"));
    repo.upsert_file_state(&file_row(
        src.id,
        "present.txt",
        size,
        mtime,
        *blake3::hash(b"x").as_bytes(),
        Some("drive-present"),
    ))
    .await
    .unwrap();
    repo.upsert_file_state(&file_row(
        src.id,
        "gone.txt",
        1,
        1,
        [0u8; 32],
        Some("drive-gone"),
    ))
    .await
    .unwrap();

    let scanned = scan(&src, &repo, ScanMode::FastPath).await.unwrap();
    let p = plan(&src, &scanned, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();

    let summary = p.summary();
    assert_eq!(summary.uploads, 0, "present.txt is unchanged: {:?}", p.ops);
    assert_eq!(summary.trashes, 1, "{:?}", p.ops);
    match p.ops.as_slice() {
        [Op::Trash {
            relative_path,
            drive_file_id,
            ..
        }] => {
            assert_eq!(relative_path, &rel("gone.txt"));
            assert_eq!(drive_file_id, "drive-gone");
        }
        other => panic!("expected one Trash op, got {other:?}"),
    }
}

/// Rename: a rename on disk surfaces as delete(old) + add(new). V1 does NOT
/// detect renames, so the plan is one upload (the new path) + one trash (the
/// old path's remote object) - the documented no-detect behaviour (ROADMAP
/// M2 "Rename ... we don't detect renames in V1").
#[tokio::test]
async fn rename_one_upload_one_trash() {
    // Only the renamed-to file exists on disk; the old name is seeded as a
    // synced row whose file is gone.
    let dir = tree! { "new-name.txt" => "same-bytes" };
    let (repo, _db) = temp_repo().await;
    let src = seed_source(&repo, dir.path(), true, &[], &[]).await;

    repo.upsert_file_state(&file_row(
        src.id,
        "old-name.txt",
        10,
        1,
        *blake3::hash(b"same-bytes").as_bytes(),
        Some("drive-old"),
    ))
    .await
    .unwrap();

    let scanned = scan(&src, &repo, ScanMode::FastPath).await.unwrap();
    let p = plan(&src, &scanned, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();

    let summary = p.summary();
    assert_eq!(summary.uploads, 1, "new path uploads: {:?}", p.ops);
    assert_eq!(summary.trashes, 1, "old path trashes: {:?}", p.ops);
    assert_eq!(
        upload_paths(&p.ops),
        HashSet::from(["new-name.txt".to_string()]),
        "no rename detection: the new path is re-uploaded whole"
    );
    assert!(
        p.ops.iter().any(|op| matches!(
            op,
            Op::Trash { relative_path, drive_file_id, .. }
                if relative_path == &rel("old-name.txt") && drive_file_id == "drive-old"
        )),
        "old remote object is trashed: {:?}",
        p.ops
    );
}

/// gitignore respected: a `.gitignore` listing `node_modules/` keeps
/// `node_modules/foo.js` out of the plan, while ordinary files still upload
/// (ROADMAP M2 "gitignore respected"). Asserts path presence/absence rather
/// than a raw count, since the `.gitignore` file itself is a real file the
/// scanner uploads.
#[tokio::test]
async fn gitignore_respected_node_modules_excluded() {
    let dir = tree! {
        ".gitignore" => "node_modules/\n",
        "keep.txt" => "x",
        "node_modules" => { "foo.js" => "noise" },
    };
    let (repo, _db) = temp_repo().await;
    let src = seed_source(&repo, dir.path(), true, &[], &[]).await;

    let scanned = scan(&src, &repo, ScanMode::FastPath).await.unwrap();
    let p = plan(&src, &scanned, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();

    let uploads = upload_paths(&p.ops);
    assert!(
        uploads.contains("keep.txt"),
        "ordinary file kept: {uploads:?}"
    );
    assert!(
        !uploads.contains("node_modules/foo.js"),
        "gitignored node_modules/foo.js must be excluded: {uploads:?}"
    );
}

/// `!.env` override: an `include_patterns` entry of the bare glob `.env`
/// re-includes a file the gitignore cascade would drop (ROADMAP M2 "`!.env`
/// override"). The user-facing effect is "`!.env`"; the stored glob is the
/// bare `.env`.
///
/// Now PASSES: the M2 matcher rework replaced the single `Override` with a
/// combined [`ignore::gitignore::Gitignore`] matcher where `include_patterns`
/// are added last as `!`-rules, so they re-include over the gitignore cascade
/// WITHOUT flipping to whitelist-only mode. The test also asserts `keep.txt`
/// survives, which would catch any regression that silently dropped unrelated
/// files.
#[tokio::test]
async fn env_override_reincludes_dotenv() {
    let dir = tree! {
        ".gitignore" => ".env\n",
        ".env" => "SECRET=1",
        "keep.txt" => "x",
    };
    let (repo, _db) = temp_repo().await;
    // include_patterns = [".env"] re-includes over the gitignore drop.
    let src = seed_source(&repo, dir.path(), true, &[".env"], &[]).await;

    let scanned = scan(&src, &repo, ScanMode::FastPath).await.unwrap();
    let p = plan(&src, &scanned, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();

    let uploads = upload_paths(&p.ops);
    // Strengthened: an unrelated ordinary file MUST still back up. If the
    // re-include were (mis)implemented via a whitelist glob, this would fail
    // - that is the data-loss regression we never want to ship silently.
    assert!(
        uploads.contains("keep.txt"),
        "an unrelated file must never be dropped by adding an include: {uploads:?}"
    );
    assert!(
        uploads.contains(".env"),
        ".env must be re-included by the override: {uploads:?}"
    );
}

/// `*.log` exclude wins: an `exclude_patterns` entry of `*.log` forces logs
/// out even when no gitignore rule covers them, while ordinary files still
/// upload (ROADMAP M2 "Exclude pattern wins").
#[tokio::test]
async fn exclude_pattern_log_wins() {
    let dir = tree! {
        "app.log" => "lines",
        "keep.txt" => "x",
    };
    let (repo, _db) = temp_repo().await;
    let src = seed_source(&repo, dir.path(), true, &[], &["*.log"]).await;

    let scanned = scan(&src, &repo, ScanMode::FastPath).await.unwrap();
    let p = plan(&src, &scanned, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();

    let uploads = upload_paths(&p.ops);
    assert!(uploads.contains("keep.txt"), "{uploads:?}");
    assert!(
        !uploads.contains("app.log"),
        "*.log exclude must force-out app.log: {uploads:?}"
    );
}

/// F5 / gitignore-wins-over-defaults: a source `.gitignore` with `!Thumbs.db`
/// re-includes Thumbs.db despite it being a DESIGN s5.2 DEFAULT exclude. The
/// old single-`Override` evaluated defaults ABOVE gitignore and inverted this;
/// the new last-match-wins matcher adds defaults BELOW the gitignore cascade.
#[tokio::test]
async fn gitignore_reinclude_beats_default_exclude() {
    let dir = tree! {
        ".gitignore" => "!Thumbs.db\n",
        "Thumbs.db" => "thumbs",
        "real.txt" => "x",
    };
    let (repo, _db) = temp_repo().await;
    let src = seed_source(&repo, dir.path(), true, &[], &[]).await;

    let scanned = scan(&src, &repo, ScanMode::FastPath).await.unwrap();
    let p = plan(&src, &scanned, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();

    let uploads = upload_paths(&p.ops);
    assert!(uploads.contains("real.txt"), "{uploads:?}");
    assert!(
        uploads.contains("Thumbs.db"),
        "gitignore !Thumbs.db must re-include over the default exclude: {uploads:?}"
    );
}

/// Excluded-orphan (DESIGN s5.5, F1): a file that was previously backed up
/// (has a `file_state` row with a `drive_file_id`) but is now EXCLUDED by a
/// later ignore-rule change must NOT be trashed - the local file may still
/// exist; only the rules changed. The scanner reports it in
/// `excluded_orphans` and the planner emits ZERO trash ops for it.
#[tokio::test]
async fn ignore_change_yields_excluded_orphan_no_trash() {
    // app.log is on disk AND has a synced file_state row (it was backed up
    // before *.log was added to exclude_patterns).
    let dir = tree! {
        "app.log" => "lines",
        "keep.txt" => "x",
    };
    let (repo, _db) = temp_repo().await;
    // *.log is now excluded - simulating a config change after app.log was
    // already backed up.
    let src = seed_source(&repo, dir.path(), true, &[], &["*.log"]).await;

    let (size, mtime) = stat_of(&dir.path().join("app.log"));
    repo.upsert_file_state(&file_row(
        src.id,
        "app.log",
        size,
        mtime,
        *blake3::hash(b"lines").as_bytes(),
        Some("drive-app-log"), // reached Drive => a naive delete would TRASH it
    ))
    .await
    .unwrap();

    let scanned = scan(&src, &repo, ScanMode::FastPath).await.unwrap();

    // The excluded backed-up file is an orphan, NOT a deletion.
    assert!(
        scanned.excluded_orphans.contains(&rel("app.log")),
        "app.log must be reported as an excluded-orphan: {:?}",
        scanned.excluded_orphans
    );
    assert!(
        !scanned.deleted.contains(&rel("app.log")),
        "an excluded backed-up file must never be classified as deleted: {:?}",
        scanned.deleted
    );

    let p = plan(&src, &scanned, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();
    assert_eq!(
        p.summary().trashes,
        0,
        "an ignore-rule change must NEVER trash a backed-up file (DESIGN s5.5): {:?}",
        p.ops
    );
}

/// Deep-verify catches bit-rot: the on-disk `(size, mtime_ns)` still matches
/// the seeded row (FastPath sees no change), but the bytes differ from the
/// stored hash. FastPath -> empty plan; DeepVerify -> one upload (ROADMAP M2
/// "Deep-verify catches bit-rot").
#[tokio::test]
async fn deep_verify_catches_bit_rot() {
    let dir = tree! { "a.txt" => "corrupted-bytes" };
    let (repo, _db) = temp_repo().await;
    let src = seed_source(&repo, dir.path(), true, &[], &[]).await;

    let (size, mtime) = stat_of(&dir.path().join("a.txt"));
    // Stored hash is of DIFFERENT content; size+mtime match disk so only the
    // hash distinguishes the rot.
    repo.upsert_file_state(&file_row(
        src.id,
        "a.txt",
        size,
        mtime,
        *blake3::hash(b"original-content").as_bytes(),
        Some("drive-a"),
    ))
    .await
    .unwrap();

    // FastPath: stat matches => unchanged => empty plan.
    let fast = scan(&src, &repo, ScanMode::FastPath).await.unwrap();
    let fast_plan = plan(&src, &fast, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();
    assert!(
        fast_plan.ops.is_empty(),
        "FastPath cannot see bit-rot: {:?}",
        fast_plan.ops
    );

    // DeepVerify: hash mismatch => one upload.
    let deep = scan(&src, &repo, ScanMode::DeepVerify).await.unwrap();
    let deep_plan = plan(&src, &deep, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();
    assert_eq!(deep_plan.summary().uploads, 1, "{:?}", deep_plan.ops);
    assert_eq!(
        upload_paths(&deep_plan.ops),
        HashSet::from(["a.txt".to_string()])
    );
}

/// Symlink handling (policy doc): `SymlinkPolicy::Skip` (DESIGN s5.2.1) -
/// the scanner never follows a symlink and never backs up the link itself,
/// so a link entry produces no upload while the real file does (ROADMAP M2
/// "Symlink handling - by default skipped").
///
/// Unix-only: creating a symlink on Windows needs a privilege the CI runner
/// may lack, and the policy under test is platform-independent.
#[cfg(unix)]
#[tokio::test]
async fn symlink_skipped() {
    use std::os::unix::fs::symlink;

    let dir = tree! { "real.txt" => "hello" };
    symlink(dir.path().join("real.txt"), dir.path().join("link.txt")).unwrap();

    let (repo, _db) = temp_repo().await;
    let src = seed_source(&repo, dir.path(), true, &[], &[]).await;

    let scanned = scan(&src, &repo, ScanMode::FastPath).await.unwrap();
    let p = plan(&src, &scanned, &repo, 0, &BundleConfig::default())
        .await
        .unwrap();

    let uploads = upload_paths(&p.ops);
    assert!(
        uploads.contains("real.txt"),
        "the real file backs up: {uploads:?}"
    );
    assert!(
        !uploads.contains("link.txt"),
        "a symlink must be skipped, not backed up: {uploads:?}"
    );
}
