//! A local-folder destination fixture, and its fault-free oracle
//! (STRESS_HARNESS s5.2).
//!
//! The local-folder backend needs no protocol server: its destination IS a
//! directory, so the harness can construct any on-disk state a crash would
//! leave and read the ground truth back with `std::fs`. That is a real
//! advantage over the HTTP backends - the post-crash states this module builds
//! (an abandoned temp file, a rename that cannot land, a marker belonging to a
//! different volume) are byte-for-byte what the failure actually produces,
//! rather than a model of it.
//!
//! ## What the oracle must filter, and why it matters
//!
//! [`LocalFsOracle`] reads the destination tree DIRECTLY, never through
//! `LocalFsStore` - the same fault-free discipline the fake's
//! `descendant_files_with_trashed` and the S3 server's `oracle_entries` follow,
//! so a scenario that ends with the destination "unplugged" can still have its
//! s6.3 invariants verified.
//!
//! Reading a directory directly means reproducing the backend's own notion of
//! what is NOT an object, which is not cosmetic:
//!
//! - `.driven-meta/` holds the per-object sidecars.
//! - `.driven-destination.json` is the identity marker.
//! - `.driven-tmp-*` are in-flight or abandoned upload temp files. **Counting
//!   one as an object is exactly the bug this module's crash row hunts**, so the
//!   oracle must agree with the backend that it is not one.
//! - `._*` are macOS AppleDouble shadow files, which the OS writes on
//!   exFAT/FAT32 (the format a USB backup stick actually has) for any file
//!   carrying xattrs - including `._.driven-meta`. PR #212 found these doubling
//!   every listing and, worse, entering the remote-existence audit as objects
//!   Driven owns with no `file_state` row, which the audit then tries to heal
//!   forever. On a tempdir they never appear, which is precisely why an oracle
//!   that forgot them would look correct here and lie on real hardware.
//!
//! Rather than re-deriving that list, the oracle delegates to the backend's own
//! [`driven_localfs::layout::is_control_entry`], so the two cannot drift.

use std::path::{Path, PathBuf};

use driven_localfs::{layout, meta, names, LocalFsConfig, LocalFsStore};
use driven_remote::remote_store::{RemoteEntry, RemoteStore};

use crate::handle::{DrivenHandle, DrivenHandleBuilder};

/// The destination sub-folder every local-folder row backs up into.
///
/// Non-empty on purpose: the wizard derives a per-source sub-folder from the
/// source folder's name, so a row rooted at the bare destination would exercise
/// a layout the app never produces.
pub const CHAOS_SUBFOLDER: &str = "backups";

/// A local-folder destination fixture: the destination directory, the source
/// tree, and the hermetic state DB - everything that must SURVIVE a simulated
/// crash.
///
/// The handle is deliberately not a field, for the same reason as the S3
/// fixture: a crash-recovery row has to DROP the orchestrator and then boot a
/// fresh one over the same state DB and the same destination.
pub struct LocalFsFixture {
    dest_dir: tempfile::TempDir,
    src_dir: tempfile::TempDir,
    state_dir: tempfile::TempDir,
    config: LocalFsConfig,
}

impl LocalFsFixture {
    /// Create a destination directory, stamp its identity marker the way
    /// account creation does, and prepare the source tree + state DB.
    pub fn new() -> anyhow::Result<Self> {
        let dest_dir = tempfile::tempdir()?;
        // `prepare_destination` is the real account-creation path: it proves the
        // folder is writable by actually writing, syncing and removing a probe
        // file, then stamps (or adopts) the marker durably.
        let (destination_id, _prepared) = driven_localfs::prepare_destination(dest_dir.path(), 0)?;
        let config = LocalFsConfig {
            root: dest_dir.path().to_string_lossy().into_owned(),
            destination_id,
        }
        .normalized()?;
        Ok(Self {
            dest_dir,
            src_dir: tempfile::tempdir()?,
            state_dir: tempfile::tempdir()?,
            config,
        })
    }

    /// The destination root.
    pub fn dest_root(&self) -> &Path {
        self.dest_dir.path()
    }

    /// The source tree the rows write fixture files into.
    pub fn src_root(&self) -> &Path {
        self.src_dir.path()
    }

    /// The backend config this fixture's stores are built from.
    pub fn config(&self) -> &LocalFsConfig {
        &self.config
    }

    /// The destination directory objects land in (`<dest>/backups`).
    pub fn subfolder_path(&self) -> PathBuf {
        self.dest_root().join(CHAOS_SUBFOLDER)
    }

    /// Boot (or RE-boot, after a crash) a headless handle over a real
    /// [`LocalFsStore`] pointed at this destination.
    ///
    /// Constructing the store is itself meaningful: `LocalFsStore::new` runs the
    /// abandoned-temp-file sweep, which is the recovery mechanism the crash row
    /// asserts on.
    pub async fn boot(&self) -> anyhow::Result<DrivenHandle> {
        let store: std::sync::Arc<dyn RemoteStore> =
            std::sync::Arc::new(LocalFsStore::new(&self.config)?);
        DrivenHandleBuilder::new(self.state_dir.path().join("state.db"))
            .remote(store)
            .boot()
            .await
    }

    /// A store on its own, for the assertions that must ask the BACKEND (rather
    /// than the oracle) what it can see.
    pub fn store(&self) -> anyhow::Result<LocalFsStore> {
        LocalFsStore::new(&self.config)
    }

    /// The fault-free view of this destination.
    pub fn oracle(&self) -> LocalFsOracle {
        LocalFsOracle {
            root: self.dest_root().to_path_buf(),
        }
    }

    // -- faults --------------------------------------------------------------

    /// Rewrite the identity marker with a DIFFERENT destination id.
    ///
    /// Models "a different stick is mounted at the same path" - which is the
    /// case an existence check cannot catch and the marker exists for. Strictly
    /// nastier than deleting the marker: the path exists, is a directory, is
    /// writable, and even looks like a Driven destination.
    pub fn swap_marker_identity(&self) -> anyhow::Result<()> {
        let marker = driven_localfs::DestinationMarker::new("a-different-volume-entirely", 0);
        let json = serde_json::to_vec_pretty(&marker)?;
        std::fs::write(self.config.marker_path(), json)?;
        Ok(())
    }

    /// Place an object-shaped DIRECTORY at the path an object is about to be
    /// committed to, so the commit's `rename` cannot land.
    ///
    /// This is how the harness reaches the window between "the temp file is
    /// written and `F_FULLFSYNC`ed" and "the temp file is renamed over the
    /// target" WITHOUT a fault seam in production code: the store really does
    /// write and sync its temp file, and the real `fsx::commit_rename` really
    /// does fail. What must hold afterwards is the same thing that must hold
    /// after a power cut at that instant - nothing readable at the target path,
    /// no sidecar claiming otherwise, nothing recorded `synced`.
    pub fn block_target_path(&self, encoded_name: &str) -> anyhow::Result<PathBuf> {
        let blocked = self.subfolder_path().join(encoded_name);
        std::fs::create_dir_all(&blocked)?;
        Ok(blocked)
    }

    /// Plant a temp file holding `bytes` in the destination sub-folder, exactly
    /// as a process killed between the sync and the rename leaves one.
    ///
    /// `age` backdates its mtime. The backend's sweep uses the trait's own
    /// 6-day session window as the cutoff, so a FRESH temp file must survive (it
    /// may belong to a session the executor can still resume) while a stale one
    /// must be reaped. Both halves are asserted.
    pub fn plant_orphan_temp_file(
        &self,
        bytes: &[u8],
        age: std::time::Duration,
    ) -> anyhow::Result<PathBuf> {
        let dir = self.subfolder_path();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(driven_localfs::fsx::temp_name());
        std::fs::write(&path, bytes)?;
        set_mtime_ago(&path, age)?;
        Ok(path)
    }
}

/// Backdate a file's mtime by `age`.
///
/// Uses `utimensat` via `libc` on Unix and `SetFileTime` on Windows rather than
/// a crate, because this is the only place the harness needs it.
fn set_mtime_ago(path: &Path, age: std::time::Duration) -> anyhow::Result<()> {
    let target = std::time::SystemTime::now()
        .checked_sub(age)
        .ok_or_else(|| anyhow::anyhow!("age {age:?} predates the epoch"))?;
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_modified(target)?;
    Ok(())
}

/// The fault-free, read-directly-off-disk view of a local-folder destination.
pub struct LocalFsOracle {
    root: PathBuf,
}

impl LocalFsOracle {
    /// Every object at or below the `folder_id` sub-tree, shaped as
    /// [`RemoteEntry`].
    ///
    /// `md5` is the true digest of the bytes at rest rather than the sidecar's
    /// recorded one: the sidecar is committed AFTER the data (PR #212's
    /// deliberate ordering), so during the window between the two the sidecar's
    /// digest can legitimately be stale, and reporting it would make the s6.3
    /// data-loss check fail on a state the backend defines as benign.
    pub fn entries(&self, folder_id: &str) -> Vec<RemoteEntry> {
        let prefix = layout::folder_prefix(folder_id);
        let start = if prefix.is_empty() {
            self.root.clone()
        } else {
            self.root.join(prefix.trim_end_matches('/'))
        };
        let mut out = Vec::new();
        self.walk(&start, prefix.trim_end_matches('/'), &mut out);
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    fn walk(&self, dir: &Path, dir_id: &str, out: &mut Vec<RemoteEntry>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Delegate to the BACKEND's own control-entry rule so the oracle
            // and the store cannot disagree about what is an object (see the
            // module docs on `._*` and `.driven-tmp-*`).
            if layout::is_control_entry(&name) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let child_id = if dir_id.is_empty() {
                name.clone()
            } else {
                format!("{dir_id}/{name}")
            };
            if file_type.is_dir() {
                self.walk(&entry.path(), &child_id, out);
                continue;
            }
            let Ok(bytes) = std::fs::read(entry.path()) else {
                continue;
            };
            let sidecar = meta::read_sidecar(dir, &name);
            let mut hasher = <md5::Md5 as md5::Digest>::new();
            md5::Digest::update(&mut hasher, &bytes);
            let digest: [u8; 16] = md5::Digest::finalize(hasher).into();
            out.push(RemoteEntry {
                id: child_id.clone(),
                name: sidecar
                    .as_ref()
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| names::decode(&name)),
                parents: vec![layout::parent_of(&child_id)],
                size: Some(bytes.len() as u64),
                md5: Some(digest),
                mime_type: sidecar
                    .as_ref()
                    .and_then(|s| s.mime.clone())
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                modified_time: sidecar.as_ref().map(|s| s.modified_ms).unwrap_or(0),
                // A filesystem has no trash, and the backend does not simulate
                // one: `trash` is a permanent delete.
                trashed: false,
                app_properties: sidecar.map(|s| s.props).unwrap_or_default(),
            });
        }
    }

    /// One object's bytes, or `None` if nothing is readable at that id.
    pub fn object_bytes(&self, id: &str) -> Option<Vec<u8>> {
        let path = layout::path_for_id(&self.root, id).ok()?;
        if !path.is_file() {
            return None;
        }
        std::fs::read(path).ok()
    }

    /// Every `.driven-tmp-*` file anywhere under the destination.
    ///
    /// A row asserts on this directly: an abandoned temp file is the on-disk
    /// residue of a crash between the write and the rename, and the whole point
    /// is that it is never reachable as an object and is eventually swept.
    pub fn temp_files(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(entry.path());
                    continue;
                }
                if name.starts_with(names::TMP_PREFIX) {
                    found.push(entry.path());
                }
            }
        }
        found.sort();
        found
    }

    /// Every sidecar recorded in one destination directory, for the assertion
    /// that a failed commit annotated nothing.
    pub fn sidecars_in(&self, folder_id: &str) -> Vec<String> {
        let prefix = layout::folder_prefix(folder_id);
        let dir = if prefix.is_empty() {
            self.root.clone()
        } else {
            self.root.join(prefix.trim_end_matches('/'))
        };
        let mut names: Vec<String> = meta::list_sidecars(&dir)
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.stored)
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use driven_remote::remote_store::DriveContext;

    /// The fixture must produce a destination a REAL `LocalFsStore` accepts, and
    /// the oracle must agree with that store about what is an object. If the two
    /// disagree, every local-folder row is measuring the oracle rather than the
    /// backend.
    #[tokio::test]
    async fn the_oracle_agrees_with_the_backend_about_what_is_an_object() {
        let fx = LocalFsFixture::new().expect("fixture");
        let store = fx.store().expect("store");

        let folder = store
            .ensure_folder(store.root_id(), CHAOS_SUBFOLDER, &DriveContext::MyDrive)
            .await
            .expect("ensure_folder");

        let mut props = HashMap::new();
        props.insert(
            driven_remote::props::CLIENT_OP_UUID_KEY.to_string(),
            "op-1".to_string(),
        );
        store
            .create(
                &folder.id,
                "notes.txt",
                "text/plain",
                driven_remote::remote_store::UploadBody::Bytes(bytes::Bytes::from_static(
                    b"hello local folder",
                )),
                props,
            )
            .await
            .expect("create");

        // Plant every kind of NON-object the backend filters, including the
        // AppleDouble shadow that only appears on real exFAT/FAT32 hardware.
        fx.plant_orphan_temp_file(b"partial", std::time::Duration::from_secs(1))
            .expect("temp file");
        std::fs::write(fx.subfolder_path().join("._notes.txt"), b"appledouble")
            .expect("appledouble");

        let listed = store
            .list_folder(&folder.id, &DriveContext::MyDrive)
            .await
            .expect("list_folder");
        let oracle = fx.oracle();
        let seen = oracle.entries(CHAOS_SUBFOLDER);

        assert_eq!(
            listed.len(),
            1,
            "the backend must see exactly one object: {:?}",
            listed.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
        assert_eq!(
            seen.len(),
            1,
            "the oracle must see exactly one object too: {:?}",
            seen.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
        assert_eq!(seen[0].id, listed[0].id);
        assert_eq!(
            seen[0]
                .app_properties
                .get(driven_remote::props::CLIENT_OP_UUID_KEY)
                .map(String::as_str),
            Some("op-1"),
            "the oracle must read app_properties off the sidecar"
        );
        assert_eq!(
            oracle.object_bytes(&seen[0].id).as_deref(),
            Some(&b"hello local folder"[..])
        );
        assert_eq!(oracle.temp_files().len(), 1, "the temp file is still there");
    }

    /// The marker swap must make the destination unusable, because that is the
    /// removable-media hazard the whole check exists for.
    #[tokio::test]
    async fn a_swapped_marker_makes_the_destination_refuse_every_operation() {
        let fx = LocalFsFixture::new().expect("fixture");
        let store = fx.store().expect("store");
        store
            .list_folder("", &DriveContext::MyDrive)
            .await
            .expect("a healthy destination lists");

        fx.swap_marker_identity().expect("swap");
        let err = store
            .list_folder("", &DriveContext::MyDrive)
            .await
            .expect_err("a different volume at the same path must be refused");
        // Assert the STABLE typed error, not the message text (STRESS_HARNESS
        // s9): the backend deliberately collapses the identity mismatch into
        // `DriveError::DestFolderMissing` so the app can offer one remedy -
        // "reconnect your drive" - rather than leaking a diagnostic string.
        assert!(
            matches!(
                err.downcast_ref::<driven_remote::DriveError>(),
                Some(driven_remote::DriveError::DestFolderMissing)
            ),
            "a different volume at the same path must raise DestFolderMissing, got {err:#}"
        );
    }
}
