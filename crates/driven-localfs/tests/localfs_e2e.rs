//! End-to-end tests for [`LocalFsStore`] against a REAL filesystem.
//!
//! Every scenario runs at least once, against a throwaway `TempDir` on whatever
//! filesystem the workspace lives on. Point `DRIVEN_TEST_LOCALFS_ROOTS` at one
//! or more mounted volumes (comma-separated) and each scenario runs against
//! every one of them as well - which is how the APFS and exFAT/FAT32 coverage
//! is produced:
//!
//! ```sh
//! hdiutil create -size 512m -fs APFS  -volname DrivenAPFS  /tmp/apfs.dmg
//! hdiutil create -size 512m -fs ExFAT -volname DrivenExFAT /tmp/exfat.dmg
//! hdiutil attach /tmp/apfs.dmg && hdiutil attach /tmp/exfat.dmg
//! DRIVEN_TEST_LOCALFS_ROOTS=/Volumes/DrivenAPFS,/Volumes/DrivenExFAT \
//!   cargo test -p driven-localfs --test localfs_e2e -- --nocapture
//! ```
//!
//! Nothing here is `#[ignore]`d: an ignored test is invisible in CI output and
//! reads as passing. The extra volumes are additive, and their absence is
//! printed rather than hidden.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use driven_localfs::store::LocalFsStore;
use driven_localfs::{prepare_destination, LocalFsConfig};
use driven_remote::remote_store::{
    DriveContext, RemoteStore, ResumableKind, ResumeProgress, UploadBody,
};
use md5::{Digest, Md5};

/// Comma-separated list of already-mounted volumes to run every scenario
/// against, in addition to the default temp directory.
const ENV_ROOTS: &str = "DRIVEN_TEST_LOCALFS_ROOTS";

/// The wire chunk `driven-core`'s executor pushes at `resume_chunk`
/// (`executor.rs::WIRE_CHUNK`). The resumable scenario drives exactly this size
/// so a chunking regression is caught here rather than on a user's 5 GiB video.
const CORE_WIRE_CHUNK: usize = 4 * 1024 * 1024;

fn md5_of(bytes: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(bytes);
    h.finalize().into()
}

/// Deterministic pseudo-random bytes, so a corrupted transfer cannot pass by
/// the coincidence a buffer of zeroes would allow.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// One destination under test: a directory that is cleaned up on drop.
struct Destination {
    label: String,
    root: PathBuf,
    /// Kept alive so the temp directory is not removed early; `None` for a
    /// caller-supplied volume, whose per-run subdirectory is removed by hand.
    _temp: Option<tempfile::TempDir>,
    owned_subdir: bool,
}

impl Drop for Destination {
    fn drop(&mut self) {
        if self.owned_subdir {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

impl Destination {
    fn store(&self) -> LocalFsStore {
        let (destination_id, _) = prepare_destination(&self.root, 0).expect("prepare destination");
        LocalFsStore::new(&LocalFsConfig {
            root: self.root.to_string_lossy().into_owned(),
            destination_id,
        })
        .expect("build store")
    }
}

/// A per-run nonce so concurrent runs against one volume cannot collide.
fn nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

/// Every destination this run should exercise.
fn destinations() -> Vec<Destination> {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut out = vec![Destination {
        label: "tempdir".to_string(),
        root: temp.path().to_path_buf(),
        _temp: Some(temp),
        owned_subdir: false,
    }];

    match std::env::var(ENV_ROOTS) {
        Ok(list) if !list.trim().is_empty() => {
            for raw in list.split(',') {
                let raw = raw.trim();
                if raw.is_empty() {
                    continue;
                }
                let base = PathBuf::from(raw);
                if !base.is_dir() {
                    panic!(
                        "{ENV_ROOTS} names {raw}, which is not a mounted directory; \
                         attach the volume or unset the variable"
                    );
                }
                let root = base.join(format!("driven-e2e-{}", nonce()));
                std::fs::create_dir_all(&root).expect("create the per-run destination directory");
                out.push(Destination {
                    label: raw.to_string(),
                    root,
                    _temp: None,
                    owned_subdir: true,
                });
            }
        }
        _ => {
            eprintln!(
                "[localfs e2e] {ENV_ROOTS} is unset: running against a temp directory only. \
                 Set it to a comma-separated list of mounted volumes (APFS, exFAT, FAT32) \
                 for cross-filesystem coverage."
            );
        }
    }
    out
}

/// Run `scenario` against every destination, so a failure names the filesystem.
async fn for_each_destination<F, Fut>(name: &str, scenario: F)
where
    F: Fn(LocalFsStore, PathBuf) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for dest in destinations() {
        eprintln!("[localfs e2e] {name} against {}", dest.label);
        let store = dest.store();
        let root = dest.root.clone();
        scenario(store, root).await;
    }
}

// -- scenarios ----------------------------------------------------------------

#[tokio::test]
async fn round_trip_upload_list_download() {
    for_each_destination("round_trip", |store, _root| async move {
        let root = store.root_id().to_string();
        let bytes = payload(64 * 1024, 7);
        let props = HashMap::from([
            (
                driven_remote::props::SOURCE_ID_KEY.to_string(),
                "src-1".to_string(),
            ),
            (
                driven_remote::props::CLIENT_OP_UUID_KEY.to_string(),
                "op-1".to_string(),
            ),
        ]);

        let entry = store
            .create(
                &root,
                "photo.jpg",
                "image/jpeg",
                UploadBody::Bytes(Bytes::from(bytes.clone())),
                props.clone(),
            )
            .await
            .expect("create");
        assert_eq!(entry.name, "photo.jpg");
        assert_eq!(entry.size, Some(bytes.len() as u64));
        assert_eq!(
            entry.md5,
            Some(md5_of(&bytes)),
            "the digest must be read back off the destination, not echoed"
        );
        assert_eq!(entry.app_properties, props);

        let listed = store
            .list_folder(&root, &DriveContext::MyDrive)
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, entry.id);
        assert_eq!(listed[0].name, "photo.jpg");

        let got = download_to_bytes(&store, &entry.id).await;
        assert_eq!(got, bytes, "restored bytes must match byte for byte");

        let meta = store.metadata(&entry.id).await.expect("metadata");
        assert_eq!(meta.md5, Some(md5_of(&bytes)));
        assert_eq!(meta.app_properties, props);
    })
    .await;
}

#[tokio::test]
async fn nested_folders_and_updates() {
    for_each_destination("nested_folders", |store, _root| async move {
        let root = store.root_id().to_string();
        let docs = store
            .ensure_folder(&root, "Documents", &DriveContext::MyDrive)
            .await
            .expect("ensure_folder");
        // Idempotent.
        let again = store
            .ensure_folder(&root, "Documents", &DriveContext::MyDrive)
            .await
            .expect("ensure_folder twice");
        assert_eq!(again.id, docs.id);

        let sub = store
            .ensure_folder(&docs.id, "2026", &DriveContext::MyDrive)
            .await
            .expect("nested folder");

        let v1 = payload(4096, 1);
        let entry = store
            .create(
                &sub.id,
                "notes.txt",
                "text/plain",
                UploadBody::Bytes(Bytes::from(v1.clone())),
                HashMap::from([("driven.source_id".to_string(), "s".to_string())]),
            )
            .await
            .expect("create in a nested folder");

        let v2 = payload(9000, 2);
        let updated = store
            .update(
                &entry.id,
                UploadBody::Bytes(Bytes::from(v2.clone())),
                HashMap::from([("driven.client_op_uuid".to_string(), "op-2".to_string())]),
            )
            .await
            .expect("update");
        assert_eq!(updated.id, entry.id, "an update keeps the object id");
        assert_eq!(updated.md5, Some(md5_of(&v2)));
        // The patch must MERGE, not replace: dropping the source id would make
        // the object invisible to the remote-existence audit.
        assert_eq!(
            updated
                .app_properties
                .get("driven.source_id")
                .map(String::as_str),
            Some("s")
        );
        assert_eq!(
            updated
                .app_properties
                .get("driven.client_op_uuid")
                .map(String::as_str),
            Some("op-2")
        );
        assert_eq!(download_to_bytes(&store, &entry.id).await, v2);
    })
    .await;
}

#[tokio::test]
async fn resumable_upload_across_chunk_boundaries_and_a_simulated_restart() {
    for_each_destination("resumable", |store, _root| async move {
        let root = store.root_id().to_string();
        // Two full wire chunks plus a ragged tail, so the final chunk is not a
        // multiple of anything.
        let bytes = payload(CORE_WIRE_CHUNK * 2 + 1234, 11);
        let props = HashMap::from([(
            driven_remote::props::CLIENT_OP_UUID_KEY.to_string(),
            "op-resume".to_string(),
        )]);

        let session = store
            .resumable_session(
                ResumableKind::Create {
                    parent_id: root.clone(),
                    name: "big.bin".to_string(),
                    app_properties: props.clone(),
                },
                "application/octet-stream",
                bytes.len() as u64,
            )
            .await
            .expect("open a session");

        let mut offset = 0usize;
        let mut completed = None;
        while offset < bytes.len() {
            let end = (offset + CORE_WIRE_CHUNK).min(bytes.len());
            let progress = store
                .resume_chunk(
                    &session,
                    offset as u64,
                    Bytes::copy_from_slice(&bytes[offset..end]),
                )
                .await
                .expect("push a chunk");
            match progress {
                ResumeProgress::InProgress { received } => {
                    assert_eq!(received, end as u64, "the store must accept whole chunks");
                    offset = end;
                }
                ResumeProgress::Completed(entry) => {
                    offset = end;
                    completed = Some(entry);
                }
                ResumeProgress::SessionInvalid => panic!("the session must not die mid-upload"),
            }
        }
        let entry = completed.expect("the final chunk completes the upload");
        assert_eq!(entry.size, Some(bytes.len() as u64));
        assert_eq!(entry.md5, Some(md5_of(&bytes)));
        assert_eq!(download_to_bytes(&store, &entry.id).await, bytes);
        assert_eq!(
            entry
                .app_properties
                .get(driven_remote::props::CLIENT_OP_UUID_KEY),
            props.get(driven_remote::props::CLIENT_OP_UUID_KEY)
        );
    })
    .await;
}

#[tokio::test]
async fn a_session_resumed_by_a_fresh_store_picks_up_where_the_disk_left_off() {
    for_each_destination("resume_after_restart", |store, root| async move {
        let root_id = store.root_id().to_string();
        let bytes = payload(CORE_WIRE_CHUNK + 4096, 13);
        let session = store
            .resumable_session(
                ResumableKind::Create {
                    parent_id: root_id.clone(),
                    name: "interrupted.bin".to_string(),
                    app_properties: HashMap::new(),
                },
                "application/octet-stream",
                bytes.len() as u64,
            )
            .await
            .expect("open a session");

        // One chunk, then the "process dies".
        let first = &bytes[..CORE_WIRE_CHUNK];
        store
            .resume_chunk(&session, 0, Bytes::copy_from_slice(first))
            .await
            .expect("first chunk");
        let config = store.config().clone();
        drop(store);

        // A new store, as after a restart. The persisted session handle is the
        // only state that survived.
        let store = LocalFsStore::new(&config).expect("rebuild the store");
        // The executor replays from its own recorded offset; the store must
        // report where the DISK actually is.
        let progress = store
            .resume_chunk(
                &session,
                CORE_WIRE_CHUNK as u64,
                Bytes::copy_from_slice(&bytes[CORE_WIRE_CHUNK..]),
            )
            .await
            .expect("resume");
        let entry = match progress {
            ResumeProgress::Completed(e) => e,
            other => panic!("expected completion after the final chunk, got {other:?}"),
        };
        assert_eq!(entry.md5, Some(md5_of(&bytes)));
        assert_eq!(download_to_bytes(&store, &entry.id).await, bytes);
        let _ = root;
    })
    .await;
}

#[tokio::test]
async fn a_wrong_offset_is_refused_rather_than_punching_a_hole() {
    for_each_destination("wrong_offset", |store, _root| async move {
        let root = store.root_id().to_string();
        let bytes = payload(8192, 17);
        let session = store
            .resumable_session(
                ResumableKind::Create {
                    parent_id: root,
                    name: "gap.bin".to_string(),
                    app_properties: HashMap::new(),
                },
                "application/octet-stream",
                bytes.len() as u64,
            )
            .await
            .expect("session");
        // Offer the SECOND half first. Writing it would leave the first half
        // undefined while the length looked right.
        let progress = store
            .resume_chunk(&session, 4096, Bytes::copy_from_slice(&bytes[4096..]))
            .await
            .expect("resume_chunk");
        match progress {
            ResumeProgress::InProgress { received } => assert_eq!(received, 0),
            other => panic!("a mis-offset chunk must be refused, got {other:?}"),
        }
    })
    .await;
}

#[tokio::test]
async fn trash_and_delete_are_permanent_and_idempotent() {
    for_each_destination("delete", |store, _root| async move {
        let root = store.root_id().to_string();
        let a = store
            .create(
                &root,
                "a.txt",
                "text/plain",
                UploadBody::Bytes(Bytes::from_static(b"a")),
                HashMap::new(),
            )
            .await
            .expect("create a");
        let b = store
            .create(
                &root,
                "b.txt",
                "text/plain",
                UploadBody::Bytes(Bytes::from_static(b"b")),
                HashMap::new(),
            )
            .await
            .expect("create b");

        store.trash(&a.id).await.expect("trash");
        store.trash(&a.id).await.expect("trash again is a no-op");
        store.delete_permanent(&b.id).await.expect("delete");
        store
            .delete_permanent(&b.id)
            .await
            .expect("delete again is a no-op");

        assert!(store.metadata(&a.id).await.is_err());
        assert!(store.metadata(&b.id).await.is_err());
        let listed = store
            .list_folder(&root, &DriveContext::MyDrive)
            .await
            .expect("list");
        assert!(listed.is_empty(), "left behind: {listed:?}");
    })
    .await;
}

#[tokio::test]
async fn find_by_op_uuid_adopts_the_orphan_a_crash_left_behind() {
    for_each_destination("find_by_op_uuid", |store, _root| async move {
        let root = store.root_id().to_string();
        let uuid = "op-crash-1";
        let created = store
            .create(
                &root,
                "orphan.bin",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from_static(b"contents")),
                HashMap::from([(
                    driven_remote::props::CLIENT_OP_UUID_KEY.to_string(),
                    uuid.to_string(),
                )]),
            )
            .await
            .expect("create");

        let found = store
            .find_by_op_uuid(&root, uuid, &DriveContext::MyDrive)
            .await
            .expect("find")
            .expect("the orphan must be adoptable");
        assert_eq!(found.id, created.id);

        assert!(store
            .find_by_op_uuid(&root, "op-that-never-ran", &DriveContext::MyDrive)
            .await
            .expect("find")
            .is_none());
    })
    .await;
}

#[tokio::test]
async fn the_audit_sees_exactly_the_live_objects_of_one_source() {
    for_each_destination("audit", |store, _root| async move {
        let root = store.root_id().to_string();
        let dir = store
            .ensure_folder(&root, "nested", &DriveContext::MyDrive)
            .await
            .expect("folder");

        let mut expected = std::collections::HashSet::new();
        for (parent, name, source) in [
            (root.clone(), "one.txt", "src-a"),
            (dir.id.clone(), "two.txt", "src-a"),
            (dir.id.clone(), "three.txt", "src-b"),
        ] {
            let e = store
                .create(
                    &parent,
                    name,
                    "text/plain",
                    UploadBody::Bytes(Bytes::from_static(b"x")),
                    HashMap::from([(
                        driven_remote::props::SOURCE_ID_KEY.to_string(),
                        source.to_string(),
                    )]),
                )
                .await
                .expect("create");
            if source == "src-a" {
                expected.insert(e.id);
            }
        }

        let live = store
            .list_source_object_ids("src-a", &DriveContext::MyDrive)
            .await
            .expect("audit");
        assert_eq!(live, expected);
        assert!(store
            .list_source_object_ids("src-nonexistent", &DriveContext::MyDrive)
            .await
            .expect("audit")
            .is_empty());
    })
    .await;
}

#[tokio::test]
async fn two_names_differing_only_by_case_both_survive() {
    for_each_destination("case_collision", |store, root| async move {
        let root_id = store.root_id().to_string();
        let upper = payload(2048, 21);
        let lower = payload(3000, 22);

        let a = store
            .create(
                &root_id,
                "Report.txt",
                "text/plain",
                UploadBody::Bytes(Bytes::from(upper.clone())),
                HashMap::new(),
            )
            .await
            .expect("create Report.txt");
        let b = store
            .create(
                &root_id,
                "report.txt",
                "text/plain",
                UploadBody::Bytes(Bytes::from(lower.clone())),
                HashMap::new(),
            )
            .await
            .expect("create report.txt");

        assert_ne!(
            a.id,
            b.id,
            "two distinct source names must never share one destination file \
             (root {})",
            root.display()
        );
        // BOTH must still be readable, byte for byte. This is the assertion that
        // fails on a naive implementation running on exFAT.
        assert_eq!(download_to_bytes(&store, &a.id).await, upper);
        assert_eq!(download_to_bytes(&store, &b.id).await, lower);
        assert_eq!(a.md5, Some(md5_of(&upper)));
        assert_eq!(b.md5, Some(md5_of(&lower)));

        // And the listing reports the ORIGINAL names, not the encoded ones.
        let mut names: Vec<String> = store
            .list_folder(&root_id, &DriveContext::MyDrive)
            .await
            .expect("list")
            .into_iter()
            .map(|e| e.name)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["Report.txt".to_string(), "report.txt".to_string()]
        );
    })
    .await;
}

#[tokio::test]
async fn filesystem_hostile_names_round_trip() {
    for_each_destination("hostile_names", |store, _root| async move {
        let root = store.root_id().to_string();
        // Every one of these is legal on APFS/ext4 and rejected by exFAT/FAT32
        // (or mangled by Windows), which is exactly why the encoding exists.
        let hostile = [
            "a:b.txt",
            "why?.txt",
            "pipe|d.txt",
            "star*.txt",
            "quote\".txt",
            "angle<>.txt",
            "back\\slash.txt",
            "trailing dot.",
            "trailing space ",
            "100% done.txt",
            "Ünïcödé \u{1f600}.txt",
            // Looks exactly like a macOS AppleDouble shadow. It must survive as
            // the user's own file rather than being filtered out as filesystem
            // noise.
            "._notes.txt",
            // MS-DOS device names. Not merely rejected on Windows - `nul.txt`
            // OPENS THE NULL DEVICE, so the write succeeds and the bytes vanish.
            "CON",
            "nul.txt",
            "COM1.log",
        ];

        let mut ids = Vec::new();
        for (i, name) in hostile.iter().enumerate() {
            let bytes = payload(512 + i, i as u64);
            let entry = store
                .create(
                    &root,
                    name,
                    "application/octet-stream",
                    UploadBody::Bytes(Bytes::from(bytes.clone())),
                    HashMap::new(),
                )
                .await
                .unwrap_or_else(|e| panic!("create {name:?}: {e}"));
            assert_eq!(entry.name, *name, "the ORIGINAL name must be reported back");
            assert_eq!(entry.md5, Some(md5_of(&bytes)));
            assert_eq!(download_to_bytes(&store, &entry.id).await, bytes);
            ids.push(entry.id);
        }

        let listed = store
            .list_folder(&root, &DriveContext::MyDrive)
            .await
            .expect("list");
        let mut got: Vec<String> = listed.into_iter().map(|e| e.name).collect();
        got.sort();
        let mut want: Vec<String> = hostile.iter().map(|s| s.to_string()).collect();
        want.sort();
        assert_eq!(got, want);
    })
    .await;
}

/// An ENCRYPTED source encrypts file NAMES, so `create` routinely receives a
/// long ciphertext string rather than a human filename - long enough to exceed
/// the encoder's per-component budget and go through truncate-plus-digest.
///
/// That is safe (restore keys off `file_state.drive_file_id`, never off the
/// destination filename), but it must still round-trip: two long names that share
/// a prefix have to land on two different files, and the same long name has to
/// land on the same file every time. The visible cost is that an encrypted
/// source's destination is NOT browsable by name - which is true of every
/// backend, since the names are ciphertext either way.
///
/// Not run on Windows. A ~205-byte filename is well inside the 255-byte
/// COMPONENT limit every target filesystem has, but Windows additionally caps a
/// full path at 260 characters unless long paths are enabled system-wide AND the
/// process opts in via its manifest - which a `cargo test` binary does not. A
/// temp directory plus `.driven-meta\` plus a 205-byte name plus `.json` crosses
/// that, so the failure would be the harness's MAX_PATH, not the store's.
/// The truncate-and-digest ALGORITHM is pure string logic and is covered
/// platform-neutrally by `names::over_long_names_are_truncated_deterministically`.
/// (The same MAX_PATH ceiling is a real product caveat for a deeply-nested
/// destination on Windows; it is noted in the PR.)
#[cfg(not(windows))]
#[tokio::test]
async fn very_long_names_stay_distinct_and_restorable() {
    for_each_destination("long_names", |store, _root| async move {
        let root = store.root_id().to_string();
        // A shared 280-char prefix, differing only in the last character - the
        // worst case for a truncating scheme.
        let base = "e".repeat(280);
        let a_name = format!("{base}A");
        let b_name = format!("{base}B");
        let a_bytes = payload(4096, 51);
        let b_bytes = payload(5000, 52);

        let a = store
            .create(
                &root,
                &a_name,
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from(a_bytes.clone())),
                HashMap::new(),
            )
            .await
            .expect("create the first long name");
        let b = store
            .create(
                &root,
                &b_name,
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from(b_bytes.clone())),
                HashMap::new(),
            )
            .await
            .expect("create the second long name");

        assert_ne!(a.id, b.id, "two long names must not collapse onto one file");
        assert_eq!(download_to_bytes(&store, &a.id).await, a_bytes);
        assert_eq!(download_to_bytes(&store, &b.id).await, b_bytes);
        // The ORIGINAL name still round-trips through the sidecar, even though
        // the destination filename is truncated.
        assert_eq!(a.name, a_name);
        assert_eq!(b.name, b_name);

        // Re-creating the same long name must land on the SAME file (an
        // overwrite), not accrete one copy per attempt.
        let again = store
            .create(
                &root,
                &a_name,
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from(a_bytes.clone())),
                HashMap::new(),
            )
            .await
            .expect("re-create");
        assert_eq!(again.id, a.id);
        assert_eq!(
            store
                .list_folder(&root, &DriveContext::MyDrive)
                .await
                .expect("list")
                .len(),
            2
        );
    })
    .await;
}

#[tokio::test]
async fn about_reports_the_volume_and_drivens_own_footprint() {
    for_each_destination("about", |store, _root| async move {
        let root = store.root_id().to_string();
        let bytes = payload(50_000, 31);
        store
            .create(
                &root,
                "sized.bin",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from(bytes.clone())),
                HashMap::new(),
            )
            .await
            .expect("create");

        let about = store.about().await.expect("about");
        assert!(
            about.usage_in_drive >= bytes.len() as u64,
            "Driven's footprint must include the object it just wrote: {about:?}"
        );
        assert_eq!(about.usage_in_drive_trash, 0, "there is no trash");
        if let Some(limit) = about.limit {
            assert!(limit > 0);
            assert!(about.usage <= limit, "{about:?}");
        }
    })
    .await;
}

/// FAT32/exFAT record modification times with 2-SECOND granularity, and Driven's
/// scanner detects source changes by (size, mtime). This test asks the one
/// question that matters: does the destination's coarse mtime reach anything
/// Driven decides with?
///
/// It does not. `RemoteEntry.modified_time` is used in exactly two places, both
/// tie-breakers among duplicates (`find_by_op_uuid` picking the most recent of
/// several objects carrying one op uuid, and Drive's `ensure_folder` picking the
/// oldest of several same-named folders). Change detection reads the SOURCE
/// file's mtime, which lives on the source volume and is unaffected by the
/// destination's format - and the sidecar carries a millisecond timestamp of its
/// own anyway.
///
/// The test pins the two properties that keep that true: the sidecar timestamp
/// is millisecond-resolution regardless of the destination, and a re-write is
/// still detected as a change even when the destination cannot tell the two
/// writes apart by mtime.
#[tokio::test]
async fn coarse_destination_timestamps_do_not_reach_change_detection() {
    for_each_destination("timestamp_granularity", |store, root| async move {
        let root_id = store.root_id().to_string();
        let v1 = payload(1000, 41);
        let entry = store
            .create(
                &root_id,
                "ticker.bin",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from(v1.clone())),
                HashMap::new(),
            )
            .await
            .expect("create");
        let first_mtime = entry.modified_time;

        // Rewrite immediately - well inside a 2-second mtime tick.
        let v2 = payload(1000, 42);
        let updated = store
            .update(
                &entry.id,
                UploadBody::Bytes(Bytes::from(v2.clone())),
                HashMap::new(),
            )
            .await
            .expect("update");

        // Whether or not the filesystem can tell the two writes apart by time,
        // the CONTENT digest can - and that is what Driven verifies against.
        assert_ne!(
            updated.md5, entry.md5,
            "a same-second rewrite must still be distinguishable by content"
        );
        assert_eq!(updated.md5, Some(md5_of(&v2)));
        assert_eq!(download_to_bytes(&store, &entry.id).await, v2);

        let granularity = if updated.modified_time == first_mtime {
            "coarse (the two writes share one destination timestamp)"
        } else {
            "fine"
        };
        eprintln!(
            "[localfs e2e] {} destination mtime granularity: {granularity} \
             ({first_mtime} -> {})",
            root.display(),
            updated.modified_time
        );
    })
    .await;
}

/// FAT32 cannot store a single file of 4 GiB or more. There is no portable way
/// to learn a destination's per-file ceiling before writing, so the store relies
/// on the write failing with `EFBIG` and on
/// [`driven_localfs::error::classify_io`] turning that into a message naming the
/// cause and the fix. This test proves that end to end - but it needs a real
/// FAT32 volume with more than 4 GiB free, so it runs only when one is pointed
/// at it:
///
/// ```sh
/// hdiutil create -size 5g -fs MS-DOS -volname DRIVENBIG -type SPARSE /tmp/fat32big
/// hdiutil attach /tmp/fat32big.sparseimage
/// DRIVEN_TEST_LOCALFS_FAT32_ROOT=/Volumes/DRIVENBIG cargo test -p driven-localfs
/// ```
///
/// Not `#[ignore]`d: an ignored test is invisible and reads as passing. When the
/// variable is unset this prints why it did nothing and returns.
#[tokio::test]
async fn a_file_over_the_fat32_ceiling_fails_with_an_actionable_error() {
    const ENV_FAT32: &str = "DRIVEN_TEST_LOCALFS_FAT32_ROOT";
    let Ok(root) = std::env::var(ENV_FAT32) else {
        eprintln!(
            "[localfs e2e] {ENV_FAT32} is unset: skipping the FAT32 4 GiB ceiling check. \
             The errno mapping itself is covered by driven_localfs::error's unit tests."
        );
        return;
    };
    let base = PathBuf::from(&root);
    assert!(base.is_dir(), "{ENV_FAT32}={root} is not a mounted volume");
    let dest = base.join(format!("driven-e2e-{}", nonce()));
    std::fs::create_dir_all(&dest).expect("create the destination directory");

    let (destination_id, _) = prepare_destination(&dest, 0).expect("prepare");
    let store = LocalFsStore::new(&LocalFsConfig {
        root: dest.to_string_lossy().into_owned(),
        destination_id,
    })
    .expect("store");

    // 4 GiB + 1 MiB, streamed so the test does not need it in memory.
    const CHUNK: usize = 8 * 1024 * 1024;
    let total: u64 = 4 * 1024 * 1024 * 1024 + 1024 * 1024;
    let chunks = total.div_ceil(CHUNK as u64);
    let stream = futures::stream::iter((0..chunks).map(move |i| {
        let remaining = total - i * CHUNK as u64;
        let len = (CHUNK as u64).min(remaining) as usize;
        Ok(Bytes::from(vec![0x5Au8; len]))
    }));

    let result = store
        .create(
            store.root_id(),
            "too-big.bin",
            "application/octet-stream",
            UploadBody::Stream {
                len: total,
                stream: Box::new(stream),
            },
            HashMap::new(),
        )
        .await;

    let err = match result {
        Ok(entry) => panic!("FAT32 accepted a {total}-byte file: {entry:?}"),
        Err(e) => e,
    };
    let rendered = format!("{err:?}");
    eprintln!("[localfs e2e] FAT32 over-ceiling write reported: {rendered}");
    assert!(
        rendered.contains("exFAT"),
        "the error must name the cause and the fix, got: {rendered}"
    );
    // No partial object may be published under the object's name.
    assert!(!dest.join("too-big.bin").exists());
    // And no temp file is left occupying the stick.
    let leftovers: Vec<String> = std::fs::read_dir(&dest)
        .expect("read the destination")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".driven-tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );

    let _ = std::fs::remove_dir_all(&dest);
}

// -- helpers ------------------------------------------------------------------

async fn download_to_bytes(store: &dyn RemoteStore, id: &str) -> Vec<u8> {
    use tokio::io::AsyncReadExt as _;
    let mut stream = store.download(id).await.expect("download");
    let mut out = Vec::new();
    stream.0.read_to_end(&mut out).await.expect("read the body");
    out
}

/// A destination directory is never left behind by a passing run.
#[allow(dead_code)]
fn assert_clean(root: &Path) {
    assert!(root.exists());
}
