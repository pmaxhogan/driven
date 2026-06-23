//! Portable [`RemoteStore`] contract scenarios shared by the fake and the
//! real `GoogleDriveStore` (SPEC s3).
//!
//! Every `scenario_*` fn is written against `&dyn RemoteStore` so both
//! `fake_contract.rs` (against `InMemoryRemoteStore`) and `google_e2e.rs`
//! (against the real Drive, gated on `DRIVEN_E2E_*`) reuse them unchanged.
//! The fault-injection tests stay in `fake_contract.rs` because the
//! production store has no way to simulate them.
//!
//! Reference docs:
//! - SPEC s3 (contract bullets the suite must hit)
//! - DESIGN s5.6 (reconciliation drives the `find_by_op_uuid` test)
//! - ROADMAP M1 acceptance (round-trip, resumable across chunk boundaries,
//!   trash + list-with-trashed, parallel uploads).

#![allow(dead_code)]

use std::collections::HashMap;

use bytes::Bytes;
use driven_drive::fake::{CHUNK_MULTIPLE, CLIENT_OP_UUID_KEY};
use driven_drive::remote_store::{RemoteStore, ResumableKind, ResumeProgress, UploadBody};
use tokio::io::AsyncReadExt;

/// Builds an `app_properties` map from `(key, value)` pairs.
pub fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// Downloads a file by id into a fully-buffered `Vec<u8>` for assertions.
pub async fn download_to_bytes(store: &dyn RemoteStore, file_id: &str) -> Vec<u8> {
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

/// Upload (small) -> list -> download round-trip.
pub async fn scenario_round_trip(store: &dyn RemoteStore, root: &str) {
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
pub async fn scenario_duplicate_names_create(store: &dyn RemoteStore, root: &str) {
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
pub async fn scenario_update_preserves_id_merges_props(store: &dyn RemoteStore, root: &str) {
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
pub async fn scenario_resumable_round_trip(store: &dyn RemoteStore, root: &str) {
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
pub async fn scenario_resumable_non_multiple_rejected(store: &dyn RemoteStore, root: &str) {
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
pub async fn scenario_trash_idempotent(store: &dyn RemoteStore, root: &str) {
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
pub async fn scenario_find_by_op_uuid(store: &dyn RemoteStore, root: &str) {
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
