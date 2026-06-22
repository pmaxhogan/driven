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
