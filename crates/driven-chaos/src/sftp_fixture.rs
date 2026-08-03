//! An SSH (SFTP) destination fixture, and its fault-free oracle
//! (STRESS_HARNESS s5.2).
//!
//! The SFTP backend sits between the two shapes the harness already had. Like
//! the S3 rows, its faults live BELOW the `RemoteStore` trait - a mid-transfer
//! transport cut, a rejected credential, a host key that changed, a full remote
//! disk, an enumeration the server abandons half way - and none of them exist
//! at the trait seam at all. Like the local-folder rows, its destination is a
//! directory this process can read, because
//! [`driven_sftp::test_support::TestSftpServer`] serves a temp directory over a
//! real socket.
//!
//! That combination is what makes these rows honest: the faults go in at the
//! layer the hazard actually occupies (the TCP stream, the auth handler, the
//! server's host key, the `write` and `readdir` handlers), while the ground
//! truth is read straight off the served directory with `std::fs`.
//!
//! ## Why the oracle does not use the store
//!
//! [`SftpOracle`] never goes through `SftpStore`, and that is a requirement
//! rather than a convenience. Two of the faults here are LATCHED - a swapped
//! host key and a marker that no longer names this account's destination -
//! and both make every client-side read fail. An oracle built on the client
//! would then report "could not verify" for a run whose invariants the harness
//! is specifically there to check, and the s6.3 sweep would go green on a
//! destination nobody looked at. Reading the directory directly is the same
//! discipline the fake's `descendant_files_with_trashed`, the S3 server's
//! `oracle_entries` and [`crate::localfs_fixture::LocalFsOracle`] follow.
//!
//! ## What the oracle must filter, and why it matters
//!
//! Reading the directory directly means reproducing the backend's own notion of
//! what is NOT an object. Rather than re-derive it, the oracle delegates to
//! `driven_sftp`'s own [`names::is_reserved_control_name`], so the two cannot
//! drift:
//!
//! - `.driven-destination.json` is the identity marker.
//! - `.<stored>.driven-meta` is the per-object sidecar. Unlike the local-folder
//!   backend these are SIBLINGS of their objects rather than files in a
//!   subdirectory (one round trip per lookup instead of a `mkdir` + `stat` per
//!   directory), so they share the object namespace and MUST be filtered.
//! - `.driven-tmp-*` are in-flight or abandoned upload temp files. Counting one
//!   as an object is the bug the interrupted-transfer row hunts.
//! - `._*` are macOS AppleDouble shadow files, which appear on a share whose
//!   far end is exFAT/FAT32.
//!
//! `md5` comes from the bytes ACTUALLY on disk rather than the sidecar's
//! recorded digest, for the reason the sidecar's own docs give: the sidecar is
//! committed AFTER the data, so inside that window its digest can legitimately
//! be stale, and reporting it would fail the s6.3 data-loss check on a state
//! the backend defines as benign.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use driven_remote::remote_store::{RemoteEntry, RemoteStore};
use driven_sftp::config::{SftpAuthKind, SftpConfig};
use driven_sftp::test_support::TestSftpServer;
use driven_sftp::{meta, names, SftpStore};

use driven_core::state::{AccountRow, BackendKind};

use crate::handle::{DrivenHandle, DrivenHandleBuilder};

/// The destination sub-folder every SFTP row backs up into.
///
/// Non-empty on purpose, matching the local-folder fixture: the wizard derives
/// a per-source sub-folder from the source folder's name, so a row rooted at the
/// bare destination would exercise a layout the app never produces. It also
/// means the rows enumerate a directory BELOW the root, which is where a
/// truncated walk is interesting.
pub const SFTP_SUBFOLDER: &str = "backups";

/// An SSH (SFTP) destination fixture: the running server, the temp source tree,
/// and the hermetic state DB - everything that must SURVIVE a simulated crash.
///
/// The handle is deliberately NOT a field, for the same reason as the S3 and
/// local-folder fixtures: a row that has to DROP the orchestrator and boot a
/// fresh one over the same DB and the same destination cannot do that while the
/// fixture it is borrowing owns the handle. Two SFTP rows need exactly that -
/// [`crate::scenarios::backends`]'s host-key row (a pin is verified per
/// CONNECTION, so a swap is only observable at the next one) and its
/// truncated-listing row (the remote-existence audit is gated on a per-source
/// latch held in the orchestrator's memory, so only a fresh one re-audits).
pub struct SftpFixture {
    server: TestSftpServer,
    config: SftpConfig,
    state_dir: tempfile::TempDir,
    src_dir: tempfile::TempDir,
}

impl SftpFixture {
    /// Start a server, mark its root as a Driven destination, and prepare the
    /// temp source tree + state DB.
    pub async fn new() -> anyhow::Result<Self> {
        let server = TestSftpServer::spawn().await?;
        // `prepared_config` stamps the destination marker the store proves
        // before every mutating operation. Without it every upload in every row
        // would be refused - correctly - by `SftpStore::guard_root`.
        let config = server.prepared_config(SftpAuthKind::Password);
        Ok(Self {
            server,
            config,
            state_dir: tempfile::tempdir()?,
            src_dir: tempfile::tempdir()?,
        })
    }

    /// The running server, for arming faults and reading their counters.
    pub fn server(&self) -> &TestSftpServer {
        &self.server
    }

    /// The destination root as a local path (the directory the server serves).
    pub fn dest_root(&self) -> &Path {
        self.server.root()
    }

    /// The source tree the rows write fixture files into.
    pub fn src_root(&self) -> &Path {
        self.src_dir.path()
    }

    /// The destination sub-folder objects land in.
    pub fn folder(&self) -> &str {
        SFTP_SUBFOLDER
    }

    /// A store on its own, for the assertions that must ask the BACKEND (rather
    /// than the oracle) what it can see.
    pub fn store(&self) -> anyhow::Result<SftpStore> {
        SftpStore::new(&self.config, &self.server.password_credential())
    }

    /// Boot (or RE-boot, after a crash) a headless handle over a real
    /// [`SftpStore`] pointed at this fixture's server.
    ///
    /// `DrivenHandleBuilder::boot` ADOPTS the account already in the DB rather
    /// than minting a new one, so a rebooted orchestrator drives the same
    /// account - and therefore the same sources - the dead one did.
    pub async fn boot(&self) -> anyhow::Result<DrivenHandle> {
        let store: Arc<dyn RemoteStore> = Arc::new(self.store()?);
        let handle = DrivenHandleBuilder::new(self.state_dir.path().join("state.db"))
            .remote(store)
            .boot()
            .await?;
        self.stamp_sftp_account(&handle).await?;
        Ok(handle)
    }

    /// Rewrite the seeded account row as an SFTP account carrying this
    /// fixture's real `SftpConfig` blob.
    ///
    /// The orchestrator takes its store by injection and never reads
    /// `backend_kind`, so this is honesty rather than plumbing - and it proves
    /// nothing in the core trips over an account row that is neither Drive nor
    /// S3, which is the seam Task 5 widened.
    async fn stamp_sftp_account(&self, handle: &DrivenHandle) -> anyhow::Result<()> {
        let existing = handle
            .state
            .list_accounts()
            .await?
            .into_iter()
            .find(|a| a.id == handle.account_id)
            .ok_or_else(|| anyhow::anyhow!("the booted handle has no account row"))?;
        handle
            .state
            .upsert_account(&AccountRow {
                backend_kind: BackendKind::Sftp,
                backend_config_json: Some(self.config.to_json()?),
                ..existing
            })
            .await?;
        Ok(())
    }

    /// Persist a source rooted at this fixture's temp tree, backing up into
    /// [`SFTP_SUBFOLDER`].
    pub async fn add_source(
        &self,
        handle: &DrivenHandle,
    ) -> anyhow::Result<driven_core::state::SourceRow> {
        let src = crate::scenarios::backends::source_in(
            handle.account_id,
            self.src_root(),
            SFTP_SUBFOLDER,
        );
        handle.state.upsert_source(&src).await?;
        Ok(src)
    }

    /// The fault-free view of this destination.
    pub fn oracle(&self) -> SftpOracle {
        SftpOracle {
            root: self.dest_root().to_path_buf(),
        }
    }

    // -- faults ---------------------------------------------------------------

    /// Rewrite the destination's identity marker with a DIFFERENT destination
    /// id - the server-side analogue of a different stick at the same mount
    /// point.
    ///
    /// Deliberately nastier than deleting the marker: the root exists, is a
    /// directory, is writable, authenticates fine, and even looks like a Driven
    /// destination. Only the identity differs. On a NAS this is the shape an
    /// unmounted external volume takes - the mount point is an ordinary
    /// directory, and a whole backup written into it disappears on the next
    /// remount while `file_state` still calls every file synced.
    ///
    /// Written through the served directory rather than over SFTP because the
    /// USURPER is not Driven: another machine, or the NAS itself, is what
    /// changes this file.
    pub fn swap_marker_identity(&self) -> anyhow::Result<()> {
        let marker = driven_sftp::DestinationMarker::new("a-different-server-entirely", 0);
        std::fs::write(
            self.dest_root().join(names::MARKER_FILE),
            serde_json::to_vec_pretty(&marker)?,
        )?;
        Ok(())
    }

    /// Overwrite an object's metadata sidecar with a TORN one - bytes that stop
    /// mid-JSON, exactly as a crash between two writes leaves them.
    ///
    /// Returns the sidecar's path. See
    /// [`crate::scenarios::backends::SftpTornSidecarResidue`] for what this
    /// documents.
    pub fn tear_sidecar(&self, folder_id: &str, stored: &str) -> anyhow::Result<PathBuf> {
        let name = meta::sidecar_name(stored)
            .ok_or_else(|| anyhow::anyhow!("{stored:?} is not a nameable object"))?;
        let path = self.dest_root().join(folder_id).join(name);
        let whole = std::fs::read(&path)?;
        anyhow::ensure!(
            whole.len() > 8,
            "the sidecar at {} is too small to tear meaningfully",
            path.display()
        );
        std::fs::write(&path, &whole[..whole.len() / 2])?;
        Ok(path)
    }
}

/// The fault-free, read-directly-off-the-served-directory view of an SFTP
/// destination.
pub struct SftpOracle {
    root: PathBuf,
}

impl SftpOracle {
    /// Every object at or below the `folder_id` sub-tree, shaped as
    /// [`RemoteEntry`].
    pub fn entries(&self, folder_id: &str) -> Vec<RemoteEntry> {
        let prefix = folder_id.trim_end_matches('/');
        let start = if prefix.is_empty() {
            self.root.clone()
        } else {
            self.root.join(prefix)
        };
        let mut out = Vec::new();
        self.walk(&start, prefix, &mut out);
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    fn walk(&self, dir: &Path, dir_id: &str, out: &mut Vec<RemoteEntry>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Delegate to the BACKEND's own control-entry rule so the oracle and
            // the store cannot disagree about what is an object.
            if names::is_reserved_control_name(&name) {
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
            let sidecar = self.sidecar_for(dir, &name);
            let mut hasher = <md5::Md5 as md5::Digest>::new();
            md5::Digest::update(&mut hasher, &bytes);
            let digest: [u8; 16] = md5::Digest::finalize(hasher).into();
            let parent = match child_id.rfind('/') {
                Some(i) => child_id[..=i].to_string(),
                None => String::new(),
            };
            out.push(RemoteEntry {
                id: child_id.clone(),
                name: sidecar
                    .as_ref()
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| names::decode(&name)),
                parents: vec![parent],
                size: Some(bytes.len() as u64),
                md5: Some(digest),
                mime_type: sidecar
                    .as_ref()
                    .and_then(|s| s.mime.clone())
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                modified_time: sidecar.as_ref().map(|s| s.modified_ms).unwrap_or(0),
                // SSH has no trash, and the backend does not simulate one:
                // `trash` is a permanent delete.
                trashed: false,
                app_properties: sidecar.map(|s| s.props).unwrap_or_default(),
            });
        }
    }

    /// One object's sidecar, read the way the store reads it - including the
    /// leniency that an unparseable one is `None` rather than an error.
    fn sidecar_for(&self, dir: &Path, stored: &str) -> Option<meta::Sidecar> {
        let raw = std::fs::read(dir.join(meta::sidecar_name(stored)?)).ok()?;
        meta::parse("chaos oracle", &raw)
    }

    /// One object's bytes, or `None` if nothing is readable at that id.
    pub fn object_bytes(&self, id: &str) -> Option<Vec<u8>> {
        let mut path = self.root.clone();
        for segment in id.split('/') {
            if segment.is_empty() {
                continue;
            }
            if segment == "." || segment == ".." || segment.contains('\\') {
                return None;
            }
            path.push(segment);
        }
        if !path.is_file() {
            return None;
        }
        std::fs::read(path).ok()
    }

    /// Every `.driven-tmp-*` file anywhere under the destination.
    ///
    /// An abandoned temp file is the residue an interrupted upload leaves, and
    /// a row asserts directly that it is never reachable as an object.
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

    /// The `stored` names annotated by a sidecar in one destination directory,
    /// whether or not that sidecar still PARSES.
    ///
    /// Deliberately not filtered by parseability: the torn-sidecar row needs to
    /// tell "the annotation is gone" from "the annotation is unreadable", and
    /// those are very different states.
    pub fn sidecars_in(&self, folder_id: &str) -> Vec<String> {
        let dir = self.root.join(folder_id.trim_end_matches('/'));
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                meta::stored_from_sidecar_name(&name).map(str::to_string)
            })
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use driven_remote::remote_store::{DriveContext, UploadBody};

    /// The fixture must produce a destination a REAL `SftpStore` accepts, and
    /// the oracle must agree with that store about what is an object. If the
    /// two disagree, every SFTP row is measuring the oracle rather than the
    /// backend.
    #[tokio::test]
    async fn the_oracle_agrees_with_the_backend_about_what_is_an_object() {
        let fx = SftpFixture::new().await.expect("fixture");
        let store = fx.store().expect("store");

        let folder = store
            .ensure_folder("", SFTP_SUBFOLDER, &DriveContext::MyDrive)
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
                UploadBody::Bytes(bytes::Bytes::from_static(b"hello sftp")),
                props,
            )
            .await
            .expect("create");

        // Plant the kinds of NON-object the backend filters, including the
        // AppleDouble shadow that only appears on real exFAT/FAT32 hardware.
        let dir = fx.dest_root().join(SFTP_SUBFOLDER);
        std::fs::write(dir.join(format!("{}abc", names::TMP_PREFIX)), b"partial")
            .expect("temp file");
        std::fs::write(dir.join("._notes.txt"), b"appledouble").expect("appledouble");

        let listed = store
            .list_folder(&folder.id, &DriveContext::MyDrive)
            .await
            .expect("list_folder");
        let oracle = fx.oracle();
        let seen = oracle.entries(SFTP_SUBFOLDER);

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
            Some(&b"hello sftp"[..])
        );
        assert_eq!(oracle.temp_files().len(), 1, "the temp file is still there");
    }

    /// The marker swap must make the destination unusable, because that is the
    /// removable-media hazard the whole check exists for.
    #[tokio::test]
    async fn a_swapped_marker_makes_the_destination_refuse_every_mutation() {
        let fx = SftpFixture::new().await.expect("fixture");
        let store = fx.store().expect("store");
        store
            .ensure_folder("", SFTP_SUBFOLDER, &DriveContext::MyDrive)
            .await
            .expect("a healthy destination accepts a folder");

        fx.swap_marker_identity().expect("swap");
        let err = store
            .ensure_folder("", "Later", &DriveContext::MyDrive)
            .await
            .expect_err("a different destination at the same path must be refused");
        // Assert the STABLE typed error, not the message text (STRESS_HARNESS
        // s9): the backend deliberately collapses the identity mismatch into
        // `DriveError::DestFolderMissing` so the app can offer one remedy.
        assert!(
            matches!(
                driven_remote::classification_of(&err),
                Some(driven_remote::remote_store::DriveErrorClassification::Other)
            ),
            "{err:#}"
        );
        assert!(
            format!("{err:?}").contains("dest_marker_mismatch"),
            "{err:?}"
        );
    }
}
