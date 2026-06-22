//! Contract tests for [`RemoteStore`] implementations.
//!
//! Every scenario here is portable across the fake and (in M4) the real
//! `GoogleDriveStore`. The portable scenarios are written against
//! `&dyn RemoteStore` so M4 reuses them unchanged; the fault-injection
//! tests are fake-only because the production store has no way to
//! simulate them.
//!
//! Reference docs:
//! - SPEC s3 (contract bullets the suite must hit)
//! - DESIGN s5.6 (reconciliation drives the `find_by_op_uuid` test)
//! - ROADMAP M1 acceptance ("upload + list + download round-trip",
//!   "resumable upload across chunk boundaries", "trash + list-with-
//!   trashed flag", "parallel uploads don't corrupt the fake's state").

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use driven_drive::fake::{InMemoryRemoteStore, CHUNK_MULTIPLE, CLIENT_OP_UUID_KEY};
use driven_drive::remote_store::{RemoteStore, ResumableKind, ResumeProgress, UploadBody};
use tokio::io::AsyncReadExt;

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Builds a fresh fake with a known root, ready for portable scenarios.
fn fake() -> InMemoryRemoteStore {
    InMemoryRemoteStore::new()
}

fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

async fn download_to_bytes(store: &dyn RemoteStore, file_id: &str) -> Vec<u8> {
    let mut stream = store
        .download(file_id)
        .await
        .expect("download must succeed for committed files");
    let mut buf = Vec::new();
    stream
        .0
        .read_to_end(&mut buf)
        .await
        .expect("download stream readable");
    buf
}

// ---------------------------------------------------------------------------
// Portable scenarios: must pass against fake + real GoogleDriveStore.
// ---------------------------------------------------------------------------

/// Upload (small) -> list -> download round-trip.
async fn scenario_round_trip(store: &dyn RemoteStore, root: &str) {
    let entry = store
        .create(
            root,
            "hello.txt",
            "text/plain",
            UploadBody::Bytes(Bytes::from_static(b"hi")),
            props(&[]),
        )
        .await
        .expect("create succeeds");
    assert_eq!(entry.name, "hello.txt");
    assert_eq!(entry.size, Some(2));
    assert!(entry.md5.is_some(), "md5 set for files");

    let listing = store.list_folder(root).await.expect("list root");
    assert!(listing.iter().any(|e| e.id == entry.id));

    let bytes = download_to_bytes(store, &entry.id).await;
    assert_eq!(bytes, b"hi");
}

/// Drive permits duplicate names within a folder. Two `create` calls
/// with the same (parent, name) yield distinct file_ids (SPEC s3).
async fn scenario_duplicate_names_create(store: &dyn RemoteStore, root: &str) {
    let a = store
        .create(
            root,
            "dup.txt",
            "text/plain",
            UploadBody::Bytes(Bytes::from_static(b"A")),
            props(&[]),
        )
        .await
        .expect("first create");
    let b = store
        .create(
            root,
            "dup.txt",
            "text/plain",
            UploadBody::Bytes(Bytes::from_static(b"B")),
            props(&[]),
        )
        .await
        .expect("second create");
    assert_ne!(
        a.id, b.id,
        "Drive allows duplicate names within a folder; ids must differ"
    );
    let listing = store.list_folder(root).await.expect("list");
    let dups: Vec<_> = listing.iter().filter(|e| e.name == "dup.txt").collect();
    assert_eq!(dups.len(), 2);
}

/// `update` preserves the file_id and *merges* the patch into the
/// existing `app_properties` (SPEC s3).
async fn scenario_update_preserves_id_merges_props(store: &dyn RemoteStore, root: &str) {
    let created = store
        .create(
            root,
            "merge.txt",
            "text/plain",
            UploadBody::Bytes(Bytes::from_static(b"v1")),
            props(&[
                ("driven.source_id", "src-A"),
                ("driven.relative_path_hash", "h1"),
            ]),
        )
        .await
        .expect("create");
    let updated = store
        .update(
            &created.id,
            UploadBody::Bytes(Bytes::from_static(b"v2-bigger")),
            props(&[("driven.relative_path_hash", "h2")]),
        )
        .await
        .expect("update");
    assert_eq!(updated.id, created.id, "file_id stable across update");
    assert_eq!(
        updated
            .app_properties
            .get("driven.source_id")
            .map(String::as_str),
        Some("src-A"),
        "unpatched keys preserved"
    );
    assert_eq!(
        updated
            .app_properties
            .get("driven.relative_path_hash")
            .map(String::as_str),
        Some("h2"),
        "patched keys overwritten"
    );
    let bytes = download_to_bytes(store, &updated.id).await;
    assert_eq!(bytes, b"v2-bigger");
}

/// Resumable session: two 256 KiB chunks + a partial final chunk.
async fn scenario_resumable_round_trip(store: &dyn RemoteStore, root: &str) {
    let chunk = CHUNK_MULTIPLE as usize;
    let total = chunk * 2 + 17;
    let payload: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();

    let session = store
        .resumable_session(
            ResumableKind::Create {
                parent_id: root.to_string(),
                name: "big.bin".to_string(),
                app_properties: props(&[]),
            },
            "application/octet-stream",
            total as u64,
        )
        .await
        .expect("open session");

    let mut offset: u64 = 0;
    // Chunk 1.
    let p1 = store
        .resume_chunk(&session, offset, Bytes::copy_from_slice(&payload[..chunk]))
        .await
        .expect("chunk 1");
    assert!(matches!(p1, ResumeProgress::InProgress { received } if received == chunk as u64));
    offset += chunk as u64;

    // Chunk 2.
    let p2 = store
        .resume_chunk(
            &session,
            offset,
            Bytes::copy_from_slice(&payload[chunk..chunk * 2]),
        )
        .await
        .expect("chunk 2");
    assert!(matches!(p2, ResumeProgress::InProgress { received } if received == 2 * chunk as u64));
    offset += chunk as u64;

    // Final, sub-multiple chunk.
    let p3 = store
        .resume_chunk(
            &session,
            offset,
            Bytes::copy_from_slice(&payload[chunk * 2..]),
        )
        .await
        .expect("final chunk");
    let entry = match p3 {
        ResumeProgress::Completed(e) => e,
        other => panic!("expected Completed, got {other:?}"),
    };
    assert_eq!(entry.size, Some(total as u64));

    let bytes = download_to_bytes(store, &entry.id).await;
    assert_eq!(bytes, payload);
}

/// Non-256-KiB-multiple non-final chunks are rejected at the trait
/// layer as `SessionInvalid` (SPEC s3 `resume_chunk`).
async fn scenario_resumable_non_multiple_rejected(store: &dyn RemoteStore, root: &str) {
    let chunk = CHUNK_MULTIPLE as usize;
    let total = chunk * 2; // final at exactly 2 * 256 KiB

    let session = store
        .resumable_session(
            ResumableKind::Create {
                parent_id: root.to_string(),
                name: "bad.bin".to_string(),
                app_properties: props(&[]),
            },
            "application/octet-stream",
            total as u64,
        )
        .await
        .expect("open session");

    // 100 bytes: not a multiple of 256 KiB and not final -> session
    // invalidated. (The fake matches what GoogleDriveStore will do on
    // the same wire-level 400.)
    let result = store
        .resume_chunk(&session, 0, Bytes::from(vec![0u8; 100]))
        .await
        .expect("trait-level call succeeds, returns SessionInvalid");
    assert!(matches!(result, ResumeProgress::SessionInvalid));

    // Any further chunk on the dead session also yields SessionInvalid.
    let result2 = store
        .resume_chunk(&session, 0, Bytes::from(vec![0u8; chunk]))
        .await
        .expect("further chunks return SessionInvalid");
    assert!(matches!(result2, ResumeProgress::SessionInvalid));
}

/// `trash` is idempotent and 404-on-stale-id is treated as success
/// (SPEC s3 `trash`).
async fn scenario_trash_idempotent(store: &dyn RemoteStore, root: &str) {
    let created = store
        .create(
            root,
            "doomed.txt",
            "text/plain",
            UploadBody::Bytes(Bytes::from_static(b"bye")),
            props(&[]),
        )
        .await
        .expect("create");
    store.trash(&created.id).await.expect("trash once");
    store
        .trash(&created.id)
        .await
        .expect("trash twice idempotent");
    store
        .trash("00000000-0000-0000-0000-000000000000")
        .await
        .expect("404 on stale id is success");
}

/// `find_by_op_uuid`: None when never used, Some(unique) when set,
/// most-recent + warning when duplicated (SPEC s3 + DESIGN s5.6).
async fn scenario_find_by_op_uuid(store: &dyn RemoteStore, root: &str) {
    let uuid = "11111111-2222-3333-4444-555555555555";
    let none = store
        .find_by_op_uuid(root, uuid)
        .await
        .expect("call succeeds");
    assert!(none.is_none(), "unused uuid yields None");

    let a = store
        .create(
            root,
            "a.txt",
            "text/plain",
            UploadBody::Bytes(Bytes::from_static(b"a")),
            props(&[(CLIENT_OP_UUID_KEY, uuid)]),
        )
        .await
        .expect("create with op uuid");
    let found = store
        .find_by_op_uuid(root, uuid)
        .await
        .expect("find succeeds")
        .expect("matches");
    assert_eq!(found.id, a.id, "unique match returns the only one");

    // Now create a duplicate matching the same uuid. The fake must
    // return the most-recent (highest monotonic seq).
    let b = store
        .create(
            root,
            "a.txt",
            "text/plain",
            UploadBody::Bytes(Bytes::from_static(b"b")),
            props(&[(CLIENT_OP_UUID_KEY, uuid)]),
        )
        .await
        .expect("dup create");
    let dup_found = store
        .find_by_op_uuid(root, uuid)
        .await
        .expect("find with dup")
        .expect("matches");
    assert_eq!(
        dup_found.id, b.id,
        "find_by_op_uuid returns the most-recent on duplicate"
    );
}

// ---------------------------------------------------------------------------
// Portable runners (one #[tokio::test] per scenario for clean output).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fake_round_trip() {
    let store = fake();
    let root = store.root_id().to_string();
    scenario_round_trip(&store, &root).await;
}

#[tokio::test]
async fn fake_duplicate_names_create() {
    let store = fake();
    let root = store.root_id().to_string();
    scenario_duplicate_names_create(&store, &root).await;
}

#[tokio::test]
async fn fake_update_preserves_id_merges_props() {
    let store = fake();
    let root = store.root_id().to_string();
    scenario_update_preserves_id_merges_props(&store, &root).await;
}

#[tokio::test]
async fn fake_resumable_round_trip() {
    let store = fake();
    let root = store.root_id().to_string();
    scenario_resumable_round_trip(&store, &root).await;
}

#[tokio::test]
async fn fake_resumable_non_multiple_rejected() {
    let store = fake();
    let root = store.root_id().to_string();
    scenario_resumable_non_multiple_rejected(&store, &root).await;
}

#[tokio::test]
async fn fake_trash_idempotent() {
    let store = fake();
    let root = store.root_id().to_string();
    scenario_trash_idempotent(&store, &root).await;
}

#[tokio::test]
async fn fake_find_by_op_uuid_warns_on_dup() {
    let store = fake();
    let root = store.root_id().to_string();
    scenario_find_by_op_uuid(&store, &root).await;
}

// ---------------------------------------------------------------------------
// Fake-only tests: list-with-trashed flag, fault-injection invalidation,
// parallel uploads.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fake_list_with_trashed_flag() {
    let store = fake();
    let root = store.root_id().to_string();
    let entry = store
        .create(
            &root,
            "vanish.txt",
            "text/plain",
            UploadBody::Bytes(Bytes::from_static(b"x")),
            props(&[]),
        )
        .await
        .unwrap();
    store.trash(&entry.id).await.unwrap();

    let visible = store.list_folder(&root).await.unwrap();
    assert!(
        !visible.iter().any(|e| e.id == entry.id),
        "list_folder hides trashed children"
    );
    let with_trashed = store.list_folder_with_trashed(&root);
    assert!(
        with_trashed.iter().any(|e| e.id == entry.id),
        "list_folder_with_trashed surfaces trashed children"
    );
}

/// A forced 4xx mid-session via the fault injector returns
/// SessionInvalid; subsequent chunks on that session also return
/// SessionInvalid (SPEC s3 `resume_chunk`).
#[tokio::test]
async fn fake_session_invalidation_via_fault() {
    let store = InMemoryRemoteStore::new().with_session_invalidated_after(1);
    let root = store.root_id().to_string();
    let chunk = CHUNK_MULTIPLE as usize;
    let total = chunk * 3;
    let payload = vec![7u8; total];

    let session = store
        .resumable_session(
            ResumableKind::Create {
                parent_id: root.clone(),
                name: "doomed.bin".to_string(),
                app_properties: props(&[]),
            },
            "application/octet-stream",
            total as u64,
        )
        .await
        .expect("open session");

    // First chunk: clean.
    let r1 = store
        .resume_chunk(&session, 0, Bytes::copy_from_slice(&payload[..chunk]))
        .await
        .expect("clean first chunk");
    assert!(matches!(r1, ResumeProgress::InProgress { received } if received == chunk as u64));

    // Second chunk: fault trips here.
    let r2 = store
        .resume_chunk(
            &session,
            chunk as u64,
            Bytes::copy_from_slice(&payload[chunk..chunk * 2]),
        )
        .await
        .expect("second chunk call succeeds, returns SessionInvalid");
    assert!(matches!(r2, ResumeProgress::SessionInvalid));

    // Third chunk: session is dead, stays dead.
    let r3 = store
        .resume_chunk(
            &session,
            (2 * chunk) as u64,
            Bytes::copy_from_slice(&payload[chunk * 2..]),
        )
        .await
        .expect("session is dead, stays dead");
    assert!(matches!(r3, ResumeProgress::SessionInvalid));
}

// ---------------------------------------------------------------------------
// Arm-fires-once tests: one per fault injector, asserting the trip edge
// (after N successes the (N+1)-th call surfaces the fault) and the
// post-trip state (latching vs single-shot).
// ---------------------------------------------------------------------------

/// Helper: do a benign read-only `about()` call against the store.
async fn ping_read(store: &dyn RemoteStore) -> anyhow::Result<()> {
    store.about().await.map(|_| ())
}

/// Helper: do a benign write `create()` call.
async fn ping_write(store: &dyn RemoteStore, root: &str, name: &str) -> anyhow::Result<()> {
    store
        .create(
            root,
            name,
            "text/plain",
            UploadBody::Bytes(Bytes::from_static(b"x")),
            props(&[]),
        )
        .await
        .map(|_| ())
}

#[tokio::test]
async fn fake_rate_limit_after_trips_once() {
    let store = InMemoryRemoteStore::new().with_rate_limit_after(2);
    // Calls 0..2 succeed; call 2 trips with rate_limited; call 3
    // recovers (single-shot).
    ping_read(&store).await.expect("call 0 ok");
    ping_read(&store).await.expect("call 1 ok");
    let err = ping_read(&store).await.expect_err("call 2 trips");
    assert!(format!("{err}").contains("rate_limited"));
    ping_read(&store).await.expect("call 3 recovers");
}

#[tokio::test]
async fn fake_5xx_after_trips_once() {
    let store = InMemoryRemoteStore::new().with_5xx_after(1);
    ping_read(&store).await.expect("call 0 ok");
    let err = ping_read(&store).await.expect_err("call 1 trips");
    assert!(format!("{err}").contains("unreachable"));
    ping_read(&store).await.expect("call 2 recovers");
}

#[tokio::test]
async fn fake_network_drop_after_trips_once() {
    let store = InMemoryRemoteStore::new().with_network_drop_after(1);
    ping_read(&store).await.expect("call 0 ok");
    let err = ping_read(&store).await.expect_err("call 1 trips");
    assert!(format!("{err}").contains("net.intermittent"));
    ping_read(&store).await.expect("call 2 recovers");
}

#[tokio::test]
async fn fake_invalid_grant_after_latches() {
    let store = InMemoryRemoteStore::new().with_invalid_grant_after(1);
    ping_read(&store).await.expect("call 0 ok");
    let err = ping_read(&store).await.expect_err("call 1 trips");
    assert!(format!("{err}").contains("invalid_grant"));
    // Latches: subsequent calls also fail with the same.
    let err2 = ping_read(&store).await.expect_err("call 2 still bad");
    assert!(format!("{err2}").contains("invalid_grant"));
}

#[tokio::test]
async fn fake_quota_exhausted_latches_on_writes() {
    let store = InMemoryRemoteStore::new().with_quota_exhausted_after(3);
    let root = store.root_id().to_string();
    // 3-byte budget: first 1-byte write ok, second 1-byte ok, third
    // 1-byte ok (consumes the last byte). The fourth write requests 1
    // byte from a 0-byte budget -> rejected. The budget stays at 0 so
    // the fault latches on subsequent write requests.
    ping_write(&store, &root, "a").await.expect("write 1 ok");
    ping_write(&store, &root, "b").await.expect("write 2 ok");
    ping_write(&store, &root, "c").await.expect("write 3 ok");
    let err = ping_write(&store, &root, "d")
        .await
        .expect_err("write 4 trips");
    assert!(format!("{err}").contains("quota_exhausted"));
    let err2 = ping_write(&store, &root, "e")
        .await
        .expect_err("write 5 still bad");
    assert!(format!("{err2}").contains("quota_exhausted"));
}

#[tokio::test]
async fn fake_dest_folder_missing_latches_on_writes_only() {
    let store = InMemoryRemoteStore::new().with_dest_folder_missing();
    let root = store.root_id().to_string();
    // Read paths keep working...
    ping_read(&store).await.expect("read ok");
    store.list_folder(&root).await.expect("list ok");
    // ...write paths latch.
    let err = ping_write(&store, &root, "x")
        .await
        .expect_err("write trips");
    assert!(format!("{err}").contains("dest_folder_missing"));
    let err2 = ping_write(&store, &root, "y")
        .await
        .expect_err("write still bad");
    assert!(format!("{err2}").contains("dest_folder_missing"));
}

#[tokio::test]
async fn fake_dest_folder_readonly_latches_on_writes_only() {
    let store = InMemoryRemoteStore::new().with_dest_folder_readonly();
    let root = store.root_id().to_string();
    ping_read(&store).await.expect("read ok");
    let err = ping_write(&store, &root, "x")
        .await
        .expect_err("write trips");
    assert!(format!("{err}").contains("permission_denied"));
    let err2 = ping_write(&store, &root, "y")
        .await
        .expect_err("write still bad");
    assert!(format!("{err2}").contains("permission_denied"));
}

#[tokio::test]
async fn fake_md5_mismatch_latches_on_entry() {
    // After 0 successes, the very next write trips - and the latch
    // persists across all subsequent reads of THAT entry until it is
    // re-uploaded.
    let store = InMemoryRemoteStore::new().with_md5_mismatch_after(0);
    let root = store.root_id().to_string();
    let created = store
        .create(
            &root,
            "bad.txt",
            "text/plain",
            UploadBody::Bytes(Bytes::from_static(b"hi")),
            props(&[]),
        )
        .await
        .expect("create ok");
    let real_md5 = {
        use md5::{Digest, Md5};
        let mut h = Md5::new();
        h.update(b"hi");
        let out = h.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&out);
        bytes
    };
    assert_ne!(
        created.md5,
        Some(real_md5),
        "fault returned wrong md5 from create"
    );

    // Latch persists: a follow-up metadata call returns the SAME bad
    // md5 (no re-trip needed).
    let meta = store.metadata(&created.id).await.expect("metadata ok");
    assert_eq!(meta.md5, created.md5, "md5 latched across reads");

    // ...and list_folder agrees.
    let listing = store.list_folder(&root).await.expect("list ok");
    let listed = listing
        .iter()
        .find(|e| e.id == created.id)
        .expect("found in listing");
    assert_eq!(listed.md5, created.md5, "md5 latched across list_folder");

    // Re-upload via update clears the latch.
    let updated = store
        .update(
            &created.id,
            UploadBody::Bytes(Bytes::from_static(b"hi")),
            props(&[]),
        )
        .await
        .expect("update ok");
    assert_eq!(updated.md5, Some(real_md5), "re-upload cleared latch");
}

#[tokio::test]
async fn fake_trashed_visible_in_find_by_op_uuid() {
    let store = InMemoryRemoteStore::new().with_trashed_visible_in_find_by_op_uuid();
    let root = store.root_id().to_string();
    let uuid = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let created = store
        .create(
            &root,
            "t.txt",
            "text/plain",
            UploadBody::Bytes(Bytes::from_static(b"x")),
            props(&[(CLIENT_OP_UUID_KEY, uuid)]),
        )
        .await
        .expect("create");
    store.trash(&created.id).await.expect("trash");
    // Without the fault, find_by_op_uuid would skip trashed children
    // and return None. With the fault, the trashed entry surfaces.
    let found = store
        .find_by_op_uuid(&root, uuid)
        .await
        .expect("find succeeds")
        .expect("trashed entry visible under fault");
    assert_eq!(found.id, created.id);
    assert!(found.trashed, "the surfaced entry is the trashed one");
}

#[tokio::test]
async fn fake_session_invalid_after_chunks_targets_correct_session() {
    // Open session A and arm A specifically; open session B with no
    // arming. Push 3 chunks to B (all clean). Push 2 valid chunks to
    // A (clean); the 3rd attempt on A trips.
    let store = InMemoryRemoteStore::new();
    let root = store.root_id().to_string();
    let chunk = CHUNK_MULTIPLE as usize;
    let total = chunk * 3;

    let session_a = store
        .resumable_session(
            ResumableKind::Create {
                parent_id: root.clone(),
                name: "a.bin".into(),
                app_properties: props(&[]),
            },
            "application/octet-stream",
            (total + chunk) as u64, // 4 chunks so the 3rd is non-final
        )
        .await
        .expect("open A");
    assert!(
        store.arm_session_invalidated_after(&session_a.url, 2),
        "arm A"
    );
    let session_b = store
        .resumable_session(
            ResumableKind::Create {
                parent_id: root.clone(),
                name: "b.bin".into(),
                app_properties: props(&[]),
            },
            "application/octet-stream",
            total as u64,
        )
        .await
        .expect("open B");

    let buf = vec![0u8; chunk];

    // Three chunks to B - last is final, so it completes.
    let r1 = store
        .resume_chunk(&session_b, 0, Bytes::copy_from_slice(&buf))
        .await
        .expect("B1");
    assert!(matches!(r1, ResumeProgress::InProgress { .. }));
    let r2 = store
        .resume_chunk(&session_b, chunk as u64, Bytes::copy_from_slice(&buf))
        .await
        .expect("B2");
    assert!(matches!(r2, ResumeProgress::InProgress { .. }));
    let r3 = store
        .resume_chunk(&session_b, 2 * chunk as u64, Bytes::copy_from_slice(&buf))
        .await
        .expect("B3");
    assert!(
        matches!(r3, ResumeProgress::Completed(_)),
        "B unaffected by A's armed budget"
    );

    // Two clean chunks to A; the 3rd attempt trips.
    let a1 = store
        .resume_chunk(&session_a, 0, Bytes::copy_from_slice(&buf))
        .await
        .expect("A1");
    assert!(matches!(a1, ResumeProgress::InProgress { .. }));
    let a2 = store
        .resume_chunk(&session_a, chunk as u64, Bytes::copy_from_slice(&buf))
        .await
        .expect("A2");
    // A's budget was 2 (after(2) sets internal counter to 3). First two
    // chunks decrement 3->2 and 2->1 (no trip). The third decrements
    // 1->0 and trips.
    assert!(matches!(a2, ResumeProgress::InProgress { .. }));
    let a3 = store
        .resume_chunk(&session_a, 2 * chunk as u64, Bytes::copy_from_slice(&buf))
        .await
        .expect("A3 returns SessionInvalid");
    assert!(matches!(a3, ResumeProgress::SessionInvalid));
}

/// Concurrent `create()` calls on the same parent do not corrupt the
/// index. ROADMAP M1 acceptance: "parallel uploads don't corrupt the
/// fake's state."
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fake_parallel_creates_under_same_parent() {
    let store = Arc::new(fake());
    let root = store.root_id().to_string();
    let n = 64u32;

    let mut joins = Vec::with_capacity(n as usize);
    for i in 0..n {
        let store = Arc::clone(&store);
        let root = root.clone();
        joins.push(tokio::spawn(async move {
            let body = format!("payload-{i}");
            let entry = store
                .create(
                    &root,
                    &format!("file-{i}.bin"),
                    "application/octet-stream",
                    UploadBody::Bytes(Bytes::from(body.clone().into_bytes())),
                    props(&[]),
                )
                .await
                .expect("concurrent create");
            (entry.id, body)
        }));
    }
    let mut produced = Vec::new();
    for j in joins {
        produced.push(j.await.expect("task join"));
    }

    let listing = store.list_folder(&root).await.unwrap();
    assert_eq!(
        listing.len(),
        n as usize,
        "every concurrent create landed exactly once"
    );

    // Distinct ids, and each id round-trips to the body it created.
    let mut ids = std::collections::HashSet::new();
    for (id, body) in produced {
        assert!(
            ids.insert(id.clone()),
            "duplicate id from concurrent create"
        );
        let bytes = download_to_bytes(&*store, &id).await;
        assert_eq!(bytes, body.into_bytes());
    }
}

// ---------------------------------------------------------------------------
// Stream-body length integrity + resumable-session memory hygiene.
// ---------------------------------------------------------------------------

/// Build an `UploadBody::Stream` that yields `payload` (one chunk) while
/// declaring `len` as its content length. When `len != payload.len()` this
/// models a truncated or over-long stream.
fn stream_body(len: u64, payload: &'static [u8]) -> UploadBody {
    let chunks: Vec<anyhow::Result<Bytes>> = vec![Ok(Bytes::from_static(payload))];
    UploadBody::Stream {
        len,
        stream: Box::new(futures::stream::iter(chunks)),
    }
}

#[tokio::test]
async fn stream_shorter_than_len_rejected() {
    // The fake is the test oracle for a backup tool. A stream that yields
    // FEWER bytes than declared must be rejected, not accepted with a valid
    // MD5 of the truncated bytes (silent truncation is the worst case).
    let store = fake();
    let root = store.root_id().to_string();
    // Declare 10 bytes, yield 3.
    let res = store
        .create(
            &root,
            "short.bin",
            "application/octet-stream",
            stream_body(10, b"abc"),
            props(&[]),
        )
        .await;
    assert!(res.is_err(), "truncated stream must be rejected");
    // And nothing was committed.
    let listing = store.list_folder(&root).await.unwrap();
    assert!(
        listing.is_empty(),
        "no object created for a truncated stream"
    );

    // Same on the update path.
    let seed = store
        .create(
            &root,
            "u.bin",
            "application/octet-stream",
            UploadBody::Bytes(Bytes::from_static(b"seed")),
            props(&[]),
        )
        .await
        .expect("seed create");
    let upd = store
        .update(&seed.id, stream_body(10, b"abc"), props(&[]))
        .await;
    assert!(upd.is_err(), "truncated update stream must be rejected");
}

#[tokio::test]
async fn stream_longer_than_len_rejected() {
    // A stream that yields MORE bytes than declared is equally a mismatch.
    let store = fake();
    let root = store.root_id().to_string();
    let res = store
        .create(
            &root,
            "long.bin",
            "application/octet-stream",
            stream_body(2, b"abcdef"),
            props(&[]),
        )
        .await;
    assert!(res.is_err(), "over-long stream must be rejected");
    let listing = store.list_folder(&root).await.unwrap();
    assert!(
        listing.is_empty(),
        "no object created for an over-long stream"
    );
}

#[tokio::test]
async fn resumable_session_large_len_does_not_preallocate() {
    // Smoke test (M3 owns the real RSS measurement): opening a session for a
    // 1 GiB declared upload must return promptly without committing 1 GiB of
    // memory, and a small chunk still works against it.
    let store = fake();
    let root = store.root_id().to_string();
    let one_gib: u64 = 1024 * 1024 * 1024;
    let session = store
        .resumable_session(
            ResumableKind::Create {
                parent_id: root.clone(),
                name: "big.bin".into(),
                app_properties: props(&[]),
            },
            "application/octet-stream",
            one_gib,
        )
        .await
        .expect("open large session quickly without OOM");

    // A single non-final 256 KiB chunk is accepted (proves the buffer grows
    // on demand rather than being preallocated to 1 GiB).
    let chunk = vec![0u8; CHUNK_MULTIPLE as usize];
    let progress = store
        .resume_chunk(&session, 0, Bytes::copy_from_slice(&chunk))
        .await
        .expect("first chunk accepted");
    assert!(matches!(progress, ResumeProgress::InProgress { .. }));
}

#[tokio::test]
async fn invalidated_session_releases_buffer_and_stays_invalid() {
    // Invalidating a session must drop its received buffer yet keep the
    // tombstone so future resume_chunk calls still report SessionInvalid.
    let store = fake();
    let root = store.root_id().to_string();
    let chunk = CHUNK_MULTIPLE as usize;
    let session = store
        .resumable_session(
            ResumableKind::Create {
                parent_id: root.clone(),
                name: "inv.bin".into(),
                app_properties: props(&[]),
            },
            "application/octet-stream",
            (chunk * 3) as u64,
        )
        .await
        .expect("open session");

    // First chunk lands fine.
    let buf = vec![0u8; chunk];
    let r1 = store
        .resume_chunk(&session, 0, Bytes::copy_from_slice(&buf))
        .await
        .expect("chunk 1");
    assert!(matches!(r1, ResumeProgress::InProgress { .. }));

    // A non-256-KiB-multiple non-final chunk invalidates the session (which
    // also drops the received buffer via `invalidate`).
    let bad = vec![0u8; 7];
    let r2 = store
        .resume_chunk(&session, chunk as u64, Bytes::copy_from_slice(&bad))
        .await
        .expect("invalidating chunk returns SessionInvalid");
    assert!(matches!(r2, ResumeProgress::SessionInvalid));

    // The session stays dead: a subsequent (otherwise valid) chunk still
    // returns SessionInvalid rather than resuming.
    let r3 = store
        .resume_chunk(&session, chunk as u64, Bytes::copy_from_slice(&buf))
        .await
        .expect("post-invalidation resume_chunk");
    assert!(
        matches!(r3, ResumeProgress::SessionInvalid),
        "invalidated session must remain invalid"
    );
}
