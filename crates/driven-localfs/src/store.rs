//! [`LocalFsStore`] - the `RemoteStore` implementation for a plain directory:
//! a USB drive, a NAS mount, an external SSD, or any folder you can restore
//! from without a network.
//!
//! ## Integrity (read this before touching the upload paths)
//!
//! The executor verifies every upload by comparing the `RemoteEntry.md5` this
//! store returns against the md5 it computed locally over the exact bytes it
//! sent (`executor.rs`, "md5 verify over the exact bytes sent"). On an HTTP
//! backend that check is meaningful because the digest comes back from the
//! server. Here there is no server, so the naive implementation - return the
//! digest we accumulated while writing - would make the executor compare a
//! number against itself and silently disable corruption detection for every
//! file.
//!
//! So [`LocalFsStore`] never returns its in-memory digest. Every write:
//!
//! 1. streams into a temp file in the target's own directory, hashing as it
//!    goes;
//! 2. `F_FULLFSYNC`es that file (see [`crate::fsx`] - a plain `fsync` on macOS
//!    does NOT flush the drive's own write cache, which is the entire question
//!    on removable media);
//! 3. atomically `rename`s it over the target and syncs the directory entry;
//! 4. **re-opens the committed file and hashes it back off the destination**,
//!    and returns THAT digest.
//!
//! Step 4 is what makes the executor's check real. Its honest limitation: the
//! re-read can be served from the page cache, so on platforms without a
//! cache-bypass hint it proves the bytes were correctly assembled and correctly
//! named rather than that the physical medium holds them. On macOS the verify
//! handle sets `F_NOCACHE`, which does push the read to the device.
//!
//! **Removing step 4 turns every upload check into `x == x`.** It is
//! load-bearing, not belt-and-braces.
//!
//! ## Durability
//!
//! Nothing is ever written into a live object's file. Temp-then-rename means a
//! crash leaves the PREVIOUS version intact, never a file that is half old and
//! half new while the index calls it complete. The commit ordering between the
//! data file and its metadata sidecar is spelled out in [`crate::meta`].
//!
//! ## Removable-media realities
//!
//! - Every operation first re-reads the destination's identity marker
//!   ([`crate::config`]). Missing or different means `drive.dest_folder_missing`
//!   and nothing is written - which is what stops Driven backing up into an
//!   empty, unmounted NAS mount point on the boot disk.
//! - Per-object I/O faults are classified by errno ([`crate::error`]): a full
//!   disk pauses the account, a read-only mount surfaces a permission error, and
//!   a flapping mount is retried. One `EIO` never gets promoted to "the drive is
//!   gone".
//!
//! ## Trash
//!
//! **A plain filesystem has no trash**, and Driven does not simulate one by
//! moving objects into a hidden folder: nothing would ever empty it, so a
//! backup destination would grow without bound and fill the very stick it lives
//! on. `trash` is therefore a permanent delete, identical to
//! `delete_permanent`, exactly as the S3 backend does - and the setup UI says
//! so rather than pretending otherwise.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use driven_remote::remote_store::{
    AboutInfo, DownloadStream, DriveContext, RemoteEntry, RemoteStore, ResumableKind,
    ResumableSession, ResumeProgress, UploadBody,
};
use driven_remote::DriveError;
use futures::StreamExt;
use md5::{Digest, Md5};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::{DestinationMarker, LocalFsConfig};
use crate::error::{dest_missing, io_err, not_found};
use crate::layout::{self, ClaimGuard, NameClaims};
use crate::meta::{self, EntryKind, Sidecar};
use crate::names;

/// MIME type reported for a directory entry.
const FOLDER_MIME: &str = "application/x-directory";

/// Default MIME type for an object whose sidecar records none.
const DEFAULT_MIME: &str = "application/octet-stream";

/// Scheme marking a [`ResumableSession::url`] as this backend's encoded handle
/// rather than a real URL.
const SESSION_URL_SCHEME: &str = "driven-localfs:";

/// How long an abandoned temp file may sit before the sweep removes it.
///
/// Matches the trait's own resumable-session window (Driven discards sessions
/// older than 6 days), so the sweep can never delete the temp file of a session
/// the executor might still resume.
const TMP_SWEEP_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// The persisted handle for one resumable upload.
///
/// Serialized into [`ResumableSession::url`], which the executor stores in
/// `pending_ops.payload_json` - so this is the ONLY copy that survives a process
/// restart and it must carry everything needed to finish the upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionHandle {
    /// Folder id the object lands in.
    dir: String,
    /// Destination filename chosen (and claimed) when the session opened.
    stored: String,
    /// The original name, for the sidecar and for re-claiming after a restart.
    name: String,
    /// Temp filename inside `dir` accumulating the bytes.
    temp: String,
    /// MIME type to record.
    mime: String,
    /// `app_properties` to attach on completion.
    props: HashMap<String, String>,
}

/// In-process state for one live resumable upload.
struct SessionState {
    /// Bytes accepted so far, which is always the temp file's length on disk.
    consumed: u64,
    /// Running digest over those bytes.
    md5: Md5,
    /// Keeps the destination filename reserved against a colliding parallel
    /// upload for as long as the session is open in this process.
    _claim: ClaimGuard,
}

/// The local / removable-folder [`RemoteStore`].
pub struct LocalFsStore {
    config: LocalFsConfig,
    root: PathBuf,
    claims: Arc<NameClaims>,
    sessions: Mutex<HashMap<String, SessionState>>,
}

impl LocalFsStore {
    /// Build a store for `config`.
    ///
    /// Does NOT require the destination to be present: a removable drive is
    /// routinely unplugged, and failing here would leave the account unable to
    /// start until the user happened to have the stick in. Availability is
    /// checked per operation instead ([`Self::guard_root`]).
    pub fn new(config: &LocalFsConfig) -> anyhow::Result<Self> {
        let config = config.clone().normalized()?;
        let root = config.root_path();
        let store = Self {
            config,
            root,
            claims: Arc::new(NameClaims::default()),
            sessions: Mutex::new(HashMap::new()),
        };
        store.sweep_stale_temp_files();
        Ok(store)
    }

    /// The destination root "folder" id: the empty relative path.
    pub fn root_id(&self) -> &str {
        ""
    }

    /// The configuration this store was built from.
    pub fn config(&self) -> &LocalFsConfig {
        &self.config
    }

    // -- availability --------------------------------------------------------

    /// Prove the destination is present and is the RIGHT destination, before
    /// touching anything.
    ///
    /// `root.exists()` is not the check. An unmounted NAS mount point is an
    /// ordinary empty directory, so existence alone would let Driven write a
    /// whole backup onto the boot disk underneath the mount, where it vanishes
    /// on the next remount while `file_state` still calls every file synced.
    /// The identity marker is what tells the two apart - and it also catches
    /// "a different stick mounted at the same path".
    fn guard_root(&self) -> anyhow::Result<()> {
        let marker_path = self.config.marker_path();
        let raw = match std::fs::read(&marker_path) {
            Ok(raw) => raw,
            Err(e) => {
                return Err(dest_missing(&format!(
                    "cannot read the destination marker at {}: {e}",
                    marker_path.display()
                )))
            }
        };
        let marker: DestinationMarker = match serde_json::from_slice(&raw) {
            Ok(m) => m,
            Err(e) => {
                return Err(dest_missing(&format!(
                    "the destination marker at {} is unreadable: {e}",
                    marker_path.display()
                )))
            }
        };
        if marker.destination_id != self.config.destination_id {
            return Err(dest_missing(&format!(
                "{} holds a different Driven destination (expected {}, found {})",
                self.root.display(),
                self.config.destination_id,
                marker.destination_id
            )));
        }
        Ok(())
    }

    /// Remove temp files older than [`TMP_SWEEP_AGE`], anywhere under the root.
    ///
    /// A resumable upload that is abandoned (the app is killed, the source file
    /// disappears) leaves its partial temp file behind. On a 32 GB stick those
    /// add up, and nothing else would ever collect them. Best-effort and
    /// entirely silent about a missing destination: this runs at construction,
    /// when the drive may well be unplugged.
    fn sweep_stale_temp_files(&self) {
        let mut removed = 0usize;
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if file_type.is_dir() {
                    stack.push(entry.path());
                    continue;
                }
                if !name.starts_with(names::TMP_PREFIX) {
                    continue;
                }
                let stale = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|m| SystemTime::now().duration_since(m).ok())
                    .map(|age| age > TMP_SWEEP_AGE)
                    .unwrap_or(false);
                if stale && std::fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        if removed > 0 {
            tracing::info!(
                target: crate::TARGET,
                removed,
                "swept abandoned upload temp files from the destination"
            );
        }
    }

    // -- paths ---------------------------------------------------------------

    fn path_for(&self, id: &str) -> anyhow::Result<PathBuf> {
        layout::path_for_id(&self.root, id)
    }

    /// Build a [`RemoteEntry`] for the object at `id`, joining its sidecar on.
    ///
    /// Driven by the DATA file: an entry exists because the file exists, and the
    /// sidecar only annotates it. The reverse (list sidecars, report them as
    /// objects) would report a deleted file as live and the remote-existence
    /// audit would never re-upload it.
    fn entry_for(&self, id: &str) -> anyhow::Result<RemoteEntry> {
        let path = self.path_for(id)?;
        let meta_fs = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(not_found(id)),
            Err(e) => return Err(io_err(&format!("stat {}", path.display()), e)),
        };
        Ok(self.entry_from_fs(id, &meta_fs))
    }

    fn entry_from_fs(&self, id: &str, meta_fs: &std::fs::Metadata) -> RemoteEntry {
        let stored = layout::base_name(id).to_string();
        let dir_path = self
            .path_for(&layout::parent_of(id))
            .unwrap_or_else(|_| self.root.clone());
        let sidecar = meta::read_sidecar(&dir_path, &stored);
        let modified_time = meta_fs
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        if meta_fs.is_dir() {
            return RemoteEntry {
                name: sidecar
                    .map(|s| s.name)
                    .unwrap_or_else(|| names::decode(&stored)),
                parents: vec![layout::parent_of(id)],
                id: layout::folder_prefix(id),
                size: None,
                md5: None,
                mime_type: FOLDER_MIME.to_string(),
                modified_time,
                trashed: false,
                app_properties: HashMap::new(),
            };
        }

        let size = meta_fs.len();
        RemoteEntry {
            name: sidecar
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| names::decode(&stored)),
            parents: vec![layout::parent_of(id)],
            id: id.to_string(),
            size: Some(size),
            md5: sidecar.as_ref().and_then(|s| s.md5_for(size)),
            mime_type: sidecar
                .as_ref()
                .and_then(|s| s.mime.clone())
                .unwrap_or_else(|| DEFAULT_MIME.to_string()),
            modified_time,
            trashed: false,
            app_properties: sidecar.map(|s| s.props).unwrap_or_default(),
        }
    }

    // -- writing -------------------------------------------------------------

    /// Stream `body` into `temp`, returning `(size, md5)` over the bytes
    /// actually written.
    async fn write_temp(&self, temp: &Path, body: UploadBody) -> anyhow::Result<(u64, [u8; 16])> {
        match body {
            UploadBody::Bytes(bytes) => {
                let temp = temp.to_path_buf();
                tokio::task::spawn_blocking(move || -> anyhow::Result<(u64, [u8; 16])> {
                    use std::io::Write as _;
                    let mut f = std::fs::File::create(&temp)
                        .map_err(|e| io_err(&format!("create {}", temp.display()), e))?;
                    f.write_all(&bytes)
                        .map_err(|e| io_err(&format!("write {}", temp.display()), e))?;
                    f.flush()
                        .map_err(|e| io_err(&format!("flush {}", temp.display()), e))?;
                    let mut h = Md5::new();
                    h.update(&bytes);
                    Ok((bytes.len() as u64, h.finalize().into()))
                })
                .await
                .map_err(|e| anyhow::anyhow!("localfs.internal: write task panicked: {e}"))?
            }
            UploadBody::Stream { len, mut stream } => {
                use tokio::io::AsyncWriteExt as _;
                let mut f = tokio::fs::File::create(temp)
                    .await
                    .map_err(|e| io_err(&format!("create {}", temp.display()), e))?;
                let mut h = Md5::new();
                let mut written: u64 = 0;
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    h.update(&chunk);
                    written += chunk.len() as u64;
                    f.write_all(&chunk)
                        .await
                        .map_err(|e| io_err(&format!("write {}", temp.display()), e))?;
                }
                f.flush()
                    .await
                    .map_err(|e| io_err(&format!("flush {}", temp.display()), e))?;
                // The declared length is what the executor hashed and what it
                // will verify against; a stream that produced a different number
                // of bytes is a bug we must not commit.
                if written != len {
                    anyhow::bail!(
                        "localfs.length_mismatch: the upload body declared {len} bytes but \
                         produced {written}"
                    );
                }
                Ok((written, h.finalize().into()))
            }
        }
    }

    /// Commit `temp` as the object `dir_id/stored`, write its sidecar, and
    /// return the digest READ BACK off the destination.
    ///
    /// See the module docs: returning `expected` instead of the re-read digest
    /// would make the executor's post-upload check compare a value against
    /// itself.
    #[allow(clippy::too_many_arguments)]
    async fn commit_object(
        &self,
        dir_path: PathBuf,
        dir_id: String,
        stored: String,
        original: String,
        mime: String,
        props: HashMap<String, String>,
        temp: PathBuf,
        size: u64,
        expected: [u8; 16],
        is_create: bool,
    ) -> anyhow::Result<RemoteEntry> {
        let target = dir_path.join(&stored);
        let now = now_ms();
        let stored_for_task = stored.clone();
        let verified = tokio::task::spawn_blocking(move || -> anyhow::Result<[u8; 16]> {
            // 1. The temp file's bytes reach the medium.
            let handle = std::fs::OpenOptions::new()
                .write(true)
                .open(&temp)
                .map_err(|e| io_err(&format!("reopen {}", temp.display()), e))?;
            crate::fsx::sync_file(&handle)
                .map_err(|e| io_err(&format!("sync {}", temp.display()), e))?;
            drop(handle);

            // 2. Atomic publish, then the directory entry itself becomes durable.
            if let Err(e) = crate::fsx::commit_rename(&temp, &target) {
                let _ = std::fs::remove_file(&temp);
                return Err(io_err(&format!("commit {}", target.display()), e));
            }

            // 3. Read the committed bytes BACK and hash them.
            let actual = hash_committed_file(&target)
                .map_err(|e| io_err(&format!("verify {}", target.display()), e))?;

            // 4. The sidecar is committed only once the data is proven good, and
            //    always AFTER the data (see `crate::meta`).
            if actual == expected {
                let sidecar = Sidecar {
                    version: 1,
                    kind: EntryKind::File,
                    name: original,
                    stored: stored_for_task,
                    size: Some(size),
                    md5: Some(hex::encode(actual)),
                    mime: Some(mime),
                    modified_ms: now,
                    props,
                };
                meta::write_sidecar(&dir_path, &sidecar)
                    .map_err(|e| io_err(&format!("write sidecar for {}", target.display()), e))?;
            }
            Ok(actual)
        })
        .await
        .map_err(|e| anyhow::anyhow!("localfs.internal: commit task panicked: {e}"))??;

        let id = layout::join_id(&dir_id, &stored);
        if verified != expected {
            return Err(self.checksum_mismatch(&id, is_create).await);
        }
        self.entry_for(&id)
    }

    /// Build the SPEC s24 `drive.checksum_mismatch` error, removing the corrupt
    /// object first when it was a CREATE.
    ///
    /// A create's target is an object that did not exist a moment ago, so
    /// deleting it restores the destination to its previous state. An UPDATE's
    /// target is the user's existing backed-up file: the executor's contract is
    /// that its `file_id` is never trashed on a mismatch, and the next cycle
    /// re-writes it.
    async fn checksum_mismatch(&self, id: &str, is_create: bool) -> anyhow::Error {
        if !is_create {
            return anyhow::Error::new(DriveError::ChecksumMismatch {
                stranded_file_id: None,
            });
        }
        let stranded = match self.delete_object(id).await {
            Ok(()) => None,
            Err(err) => {
                tracing::error!(
                    target: crate::TARGET,
                    %id,
                    %err,
                    "could not remove the corrupt object after a checksum mismatch; \
                     keeping the op so reconcile retries the delete"
                );
                Some(id.to_string())
            }
        };
        anyhow::Error::new(DriveError::ChecksumMismatch {
            stranded_file_id: stranded,
        })
    }

    /// Remove an object and then its sidecar. Idempotent.
    ///
    /// Data first: a dangling sidecar is inert, while a live data file with no
    /// sidecar is invisible to `list_source_object_ids` and would be re-uploaded
    /// beside itself forever.
    async fn delete_object(&self, id: &str) -> anyhow::Result<()> {
        let path = self.path_for(id)?;
        if path.is_dir() {
            anyhow::bail!(
                "localfs.not_a_file: refusing to delete {id:?}, which is a directory on the \
                 destination"
            );
        }
        let dir_path = self.path_for(&layout::parent_of(id))?;
        let stored = layout::base_name(id).to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            crate::fsx::remove_if_present(&path)
                .map_err(|e| io_err(&format!("delete {}", path.display()), e))?;
            meta::remove_sidecar(&dir_path, &stored)
                .map_err(|e| io_err(&format!("delete the sidecar for {stored}"), e))?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("localfs.internal: delete task panicked: {e}"))?
    }

    // -- enumeration ---------------------------------------------------------

    /// Every annotated FILE under `dir_path`, paired with its object id.
    ///
    /// Sidecars whose data file is gone are dropped: reporting one as live would
    /// tell the remote-existence audit that a deleted object is still on the
    /// drive, and the file would never be re-uploaded.
    fn live_annotated_files(&self, dir_path: &Path, dir_id: &str) -> anyhow::Result<Vec<Sidecar>> {
        let sidecars = meta::list_sidecars(dir_path).map_err(|e| {
            io_err(
                &format!("enumerate the sidecars in {}", dir_path.display()),
                e,
            )
        })?;
        let _ = dir_id;
        Ok(sidecars
            .into_iter()
            .filter(|s| s.kind == EntryKind::File && dir_path.join(&s.stored).is_file())
            .collect())
    }

    /// Every directory under the root, breadth-first, including the root.
    ///
    /// Returns `Err` on any enumeration failure - callers compute complete sets
    /// from this and a silently short walk would read as a mass deletion.
    fn walk_dirs(&self) -> anyhow::Result<Vec<(String, PathBuf)>> {
        let mut out = vec![(String::new(), self.root.clone())];
        let mut i = 0;
        while i < out.len() {
            let (dir_id, dir_path) = out[i].clone();
            i += 1;
            let entries = std::fs::read_dir(&dir_path)
                .map_err(|e| io_err(&format!("list {}", dir_path.display()), e))?;
            for entry in entries {
                let entry =
                    entry.map_err(|e| io_err(&format!("list {}", dir_path.display()), e))?;
                let name = entry.file_name();
                let name = name.to_string_lossy().into_owned();
                if layout::is_control_entry(&name) {
                    continue;
                }
                let file_type = entry
                    .file_type()
                    .map_err(|e| io_err(&format!("stat {}", entry.path().display()), e))?;
                if file_type.is_dir() {
                    out.push((layout::folder_id(&dir_id, &name), entry.path()));
                }
            }
        }
        Ok(out)
    }
}

/// Hash a committed file by reading it back off the destination.
///
/// On macOS the handle sets `F_NOCACHE`, so the read goes to the DEVICE rather
/// than being answered out of the page cache - which is the difference between
/// "the bytes were assembled correctly" and "the bytes are on the stick".
fn hash_committed_file(path: &Path) -> std::io::Result<[u8; 16]> {
    use std::io::Read as _;

    let mut file = std::fs::File::open(path)?;
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `fd` is owned by `file` and valid for the call. A failure is
        // not fatal - it only means the read may be answered from cache.
        unsafe {
            libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
        }
    }
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn encode_session_url(handle: &SessionHandle) -> anyhow::Result<String> {
    let json = serde_json::to_string(handle)
        .map_err(|e| anyhow::anyhow!("localfs.session_invalid: {e}"))?;
    Ok(format!("{SESSION_URL_SCHEME}{json}"))
}

fn decode_session_url(url: &str) -> anyhow::Result<SessionHandle> {
    let json = url.strip_prefix(SESSION_URL_SCHEME).ok_or_else(|| {
        anyhow::anyhow!("localfs.session_invalid: not a local-folder session handle")
    })?;
    serde_json::from_str(json).map_err(|e| anyhow::anyhow!("localfs.session_invalid: {e}"))
}

// -- the trait ----------------------------------------------------------------

#[async_trait]
impl RemoteStore for LocalFsStore {
    /// `mkdir -p` plus a sidecar recording the directory's ORIGINAL name.
    ///
    /// Unlike the S3 backend (where a folder is a key prefix and needs no
    /// request) a directory must really exist before a file can be written into
    /// it, and its directory entry must be durable. The sidecar is what lets a
    /// later `Docs` versus `docs` collision be detected on a case-insensitive
    /// volume - and what stops a directory and a file fighting over one name,
    /// which Drive permits and no filesystem does.
    ///
    /// Idempotent: an existing directory of the same original name is adopted.
    async fn ensure_folder(
        &self,
        parent_id: &str,
        name: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<RemoteEntry> {
        self.guard_root()?;
        let dir_id = layout::folder_prefix(parent_id);
        let dir_path = self.path_for(&dir_id)?;
        let claims = Arc::clone(&self.claims);
        let name = name.to_string();
        let dir_id_for_task = dir_id.clone();

        let stored = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            std::fs::create_dir_all(&dir_path)
                .map_err(|e| io_err(&format!("create {}", dir_path.display()), e))?;
            let (stored, _claim) = layout::resolve_stored_name(
                &claims,
                &dir_path,
                &dir_id_for_task,
                &name,
                EntryKind::Dir,
            )?;
            let target = dir_path.join(&stored);
            std::fs::create_dir_all(&target)
                .map_err(|e| io_err(&format!("create {}", target.display()), e))?;
            crate::fsx::sync_dir(&dir_path)
                .map_err(|e| io_err(&format!("sync {}", dir_path.display()), e))?;
            meta::write_sidecar(
                &dir_path,
                &Sidecar {
                    version: 1,
                    kind: EntryKind::Dir,
                    name,
                    stored: stored.clone(),
                    size: None,
                    md5: None,
                    mime: Some(FOLDER_MIME.to_string()),
                    modified_ms: now_ms(),
                    props: HashMap::new(),
                },
            )
            .map_err(|e| io_err(&format!("write the sidecar for {}", target.display()), e))?;
            Ok(stored)
        })
        .await
        .map_err(|e| anyhow::anyhow!("localfs.internal: ensure_folder task panicked: {e}"))??;

        self.entry_for(&layout::folder_id(&dir_id, &stored))
    }

    /// Direct children of a directory.
    ///
    /// Driven by the real directory entries, not by sidecars, so the destination
    /// picker can browse folders the user already had - and so a dangling
    /// sidecar never appears as a file. Driven's own control entries
    /// (`.driven-meta`, temp files, the destination marker) are filtered out.
    async fn list_folder(
        &self,
        folder_id: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<Vec<RemoteEntry>> {
        self.guard_root()?;
        let dir_id = layout::folder_prefix(folder_id);
        let dir_path = self.path_for(&dir_id)?;
        let entries = match std::fs::read_dir(&dir_path) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(&format!("list {}", dir_path.display()), e)),
        };

        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_err(&format!("list {}", dir_path.display()), e))?;
            let name = entry.file_name();
            let name = name.to_string_lossy().into_owned();
            if layout::is_control_entry(&name) {
                continue;
            }
            let meta_fs = match entry.metadata() {
                Ok(m) => m,
                // A file removed between the readdir and the stat is simply not
                // a child any more.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io_err(&format!("stat {}", entry.path().display()), e)),
            };
            let id = if meta_fs.is_dir() {
                layout::folder_id(&dir_id, &name)
            } else {
                layout::join_id(&dir_id, &name)
            };
            out.push(self.entry_from_fs(&id, &meta_fs));
        }
        Ok(out)
    }

    /// Write a new object at `<parent_id>/<encoded name>`.
    ///
    /// Unlike Drive, a filesystem cannot hold two files of one name in one
    /// directory: a `create` over an existing path OVERWRITES rather than
    /// producing a duplicate. That is strictly safer than the semantics the
    /// trait documents, and the caller-side "do not create over an existing
    /// `file_state.drive_file_id`" discipline still holds.
    ///
    /// The destination filename is chosen by [`layout::resolve_stored_name`], so
    /// two source files whose names differ only in case land on two different
    /// files even on exFAT.
    async fn create(
        &self,
        parent_id: &str,
        name: &str,
        mime: &str,
        body: UploadBody,
        app_properties: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        self.guard_root()?;
        let dir_id = layout::folder_prefix(parent_id);
        let dir_path = self.path_for(&dir_id)?;
        std::fs::create_dir_all(&dir_path)
            .map_err(|e| io_err(&format!("create {}", dir_path.display()), e))?;

        let (stored, claim) = {
            let claims = Arc::clone(&self.claims);
            let dir_path = dir_path.clone();
            let dir_id = dir_id.clone();
            let name = name.to_string();
            tokio::task::spawn_blocking(move || {
                layout::resolve_stored_name(&claims, &dir_path, &dir_id, &name, EntryKind::File)
            })
            .await
            .map_err(|e| anyhow::anyhow!("localfs.internal: claim task panicked: {e}"))??
        };

        let temp = dir_path.join(crate::fsx::temp_name());
        let written = self.write_temp(&temp, body).await;
        let (size, expected) = match written {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                return Err(e);
            }
        };
        let entry = self
            .commit_object(
                dir_path,
                dir_id,
                stored,
                name.to_string(),
                mime.to_string(),
                app_properties,
                temp,
                size,
                expected,
                true,
            )
            .await;
        drop(claim);
        entry
    }

    /// Overwrite the object at `file_id` (which IS its relative path).
    ///
    /// The existing sidecar is read first so the original name, MIME type and
    /// the rest of the identity stamp are carried forward - a patch that names
    /// one property must not drop the others.
    ///
    /// ## A missing target is NOT an error here
    ///
    /// On Drive a `file_id` is an opaque handle: once the object is gone the id
    /// can never be revived, which is why the executor has a dedicated
    /// `update_target_is_missing` self-heal. A local path is not opaque - it is
    /// derived from the relative path, and writing to it revives it - so an
    /// update against a deleted object correctly RE-CREATES it at exactly the
    /// path a re-plan would have chosen, leaving `file_state.drive_file_id`
    /// valid. Surfacing a not-found here would send the executor round a heal
    /// cycle to reach the same end state.
    async fn update(
        &self,
        file_id: &str,
        body: UploadBody,
        app_properties_patch: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        self.guard_root()?;
        let dir_id = layout::parent_of(file_id);
        let dir_path = self.path_for(&dir_id)?;
        let stored = layout::base_name(file_id).to_string();
        std::fs::create_dir_all(&dir_path)
            .map_err(|e| io_err(&format!("create {}", dir_path.display()), e))?;

        let existing = meta::read_sidecar(&dir_path, &stored);
        let original = existing
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_else(|| names::decode(&stored));
        let mime = existing
            .as_ref()
            .and_then(|s| s.mime.clone())
            .unwrap_or_else(|| DEFAULT_MIME.to_string());
        let mut props = existing.map(|s| s.props).unwrap_or_default();
        props.extend(app_properties_patch);

        // The caller carried the id, so it already owns this name; re-probing
        // could hand it away while this write is in flight.
        let claim = layout::claim_exact(&self.claims, &dir_id, &stored, &original);

        let temp = dir_path.join(crate::fsx::temp_name());
        let (size, expected) = match self.write_temp(&temp, body).await {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                return Err(e);
            }
        };
        let entry = self
            .commit_object(
                dir_path, dir_id, stored, original, mime, props, temp, size, expected, false,
            )
            .await;
        drop(claim);
        entry
    }

    /// Open a resumable upload: a temp file in the target's own directory that
    /// bytes are appended to and that is renamed into place on the final chunk.
    ///
    /// The handle - directory, chosen filename, temp filename, properties - is
    /// encoded into [`ResumableSession::url`], which the executor persists, so a
    /// session survives a process restart with everything needed to finish.
    async fn resumable_session(
        &self,
        kind: ResumableKind,
        mime: &str,
        size: u64,
    ) -> anyhow::Result<ResumableSession> {
        self.guard_root()?;
        let (dir_id, stored, original, props, claim) = match &kind {
            ResumableKind::Create {
                parent_id,
                name,
                app_properties,
            } => {
                let dir_id = layout::folder_prefix(parent_id);
                let dir_path = self.path_for(&dir_id)?;
                std::fs::create_dir_all(&dir_path)
                    .map_err(|e| io_err(&format!("create {}", dir_path.display()), e))?;
                let claims = Arc::clone(&self.claims);
                let name_for_task = name.clone();
                let dir_id_for_task = dir_id.clone();
                let (stored, claim) = tokio::task::spawn_blocking(move || {
                    layout::resolve_stored_name(
                        &claims,
                        &dir_path,
                        &dir_id_for_task,
                        &name_for_task,
                        EntryKind::File,
                    )
                })
                .await
                .map_err(|e| anyhow::anyhow!("localfs.internal: claim task panicked: {e}"))??;
                (dir_id, stored, name.clone(), app_properties.clone(), claim)
            }
            ResumableKind::Update { file_id } => {
                let dir_id = layout::parent_of(file_id);
                let dir_path = self.path_for(&dir_id)?;
                std::fs::create_dir_all(&dir_path)
                    .map_err(|e| io_err(&format!("create {}", dir_path.display()), e))?;
                let stored = layout::base_name(file_id).to_string();
                // A resumable update rewrites the whole object, so the existing
                // identity stamp has to be carried forward or it is lost.
                let existing = meta::read_sidecar(&dir_path, &stored);
                let original = existing
                    .as_ref()
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| names::decode(&stored));
                let props = existing.map(|s| s.props).unwrap_or_default();
                let claim = layout::claim_exact(&self.claims, &dir_id, &stored, &original);
                (dir_id, stored, original, props, claim)
            }
        };

        let handle = SessionHandle {
            dir: dir_id.clone(),
            stored,
            name: original,
            temp: crate::fsx::temp_name(),
            mime: mime.to_string(),
            props,
        };
        let url = encode_session_url(&handle)?;
        let dir_path = self.path_for(&dir_id)?;
        let temp_path = dir_path.join(&handle.temp);
        tokio::fs::File::create(&temp_path)
            .await
            .map_err(|e| io_err(&format!("create {}", temp_path.display()), e))?;
        self.sessions.lock().insert(
            url.clone(),
            SessionState {
                consumed: 0,
                md5: Md5::new(),
                _claim: claim,
            },
        );
        Ok(ResumableSession {
            url,
            issued_at: now_ms(),
            size,
            kind,
        })
    }

    /// Append one chunk to a resumable upload, committing on the final one.
    ///
    /// ## Resuming after a restart
    ///
    /// A session opened by a PREVIOUS process has no in-memory state, so the
    /// temp file on disk is the authority: it is re-read, re-hashed, and its
    /// length becomes `received`. That is strictly better than the rewind the
    /// HTTP backends need - the executor replays only the bytes that are
    /// genuinely missing - and it is safe because the digest is derived from the
    /// bytes that ACTUALLY survived the crash rather than from a remembered
    /// count.
    ///
    /// This relies on the executor treating `received` as AUTHORITATIVE in both
    /// directions, not merely as a rewind signal. It does: `executor.rs`'s
    /// `push_chunks` (the buffered path, and the one the crash-resume path uses)
    /// re-slices the body with `offset = received` on every `InProgress`, so a
    /// `received` that is behind the executor's persisted `acked_offset` simply
    /// replays the missing bytes. The STREAMED path never hydrates - it always
    /// opens a fresh session and starts at offset 0 - so `received` there is
    /// always exactly `offset + chunk.len()`.
    ///
    /// Chunk sizes are unconstrained. The 256 KiB multiple rule in the trait doc
    /// is a Drive protocol requirement with no analogue here; a `write` of any
    /// size is legal.
    async fn resume_chunk(
        &self,
        session: &ResumableSession,
        offset: u64,
        chunk: Bytes,
    ) -> anyhow::Result<ResumeProgress> {
        self.guard_root()?;
        let handle = decode_session_url(&session.url)?;
        let dir_path = self.path_for(&handle.dir)?;
        let temp_path = dir_path.join(&handle.temp);

        // Hydrate a session this process did not open.
        if !self.sessions.lock().contains_key(&session.url) {
            let temp_for_task = temp_path.clone();
            let hydrated = tokio::task::spawn_blocking(move || -> Option<(u64, Md5)> {
                let mut f = std::fs::File::open(&temp_for_task).ok()?;
                let mut hasher = Md5::new();
                let mut buf = vec![0u8; 1024 * 1024];
                let mut total = 0u64;
                loop {
                    use std::io::Read as _;
                    let n = f.read(&mut buf).ok()?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                    total += n as u64;
                }
                Some((total, hasher))
            })
            .await
            .map_err(|e| anyhow::anyhow!("localfs.internal: hydrate task panicked: {e}"))?;

            let Some((consumed, md5)) = hydrated else {
                tracing::warn!(
                    target: crate::TARGET,
                    path = %temp_path.display(),
                    "the temp file of a persisted resumable session is gone; invalidating it"
                );
                return Ok(ResumeProgress::SessionInvalid);
            };
            let claim =
                layout::claim_exact(&self.claims, &handle.dir, &handle.stored, &handle.name);
            self.sessions.lock().insert(
                session.url.clone(),
                SessionState {
                    consumed,
                    md5,
                    _claim: claim,
                },
            );
        }

        // Refuse to write at the wrong offset rather than punch a hole in the
        // middle of the object.
        {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.url)
                .ok_or_else(|| anyhow::anyhow!("localfs.session_invalid: state vanished"))?;
            if offset != state.consumed {
                return Ok(ResumeProgress::InProgress {
                    received: state.consumed,
                });
            }
        }

        if !chunk.is_empty() {
            let temp_for_task = temp_path.clone();
            let data = chunk.clone();
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                use std::io::Write as _;
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&temp_for_task)
                    .map_err(|e| io_err(&format!("open {}", temp_for_task.display()), e))?;
                f.write_all(&data)
                    .map_err(|e| io_err(&format!("write {}", temp_for_task.display()), e))?;
                f.flush()
                    .map_err(|e| io_err(&format!("flush {}", temp_for_task.display()), e))
            })
            .await
            .map_err(|e| anyhow::anyhow!("localfs.internal: chunk task panicked: {e}"))??;

            let mut sessions = self.sessions.lock();
            let state = sessions
                .get_mut(&session.url)
                .ok_or_else(|| anyhow::anyhow!("localfs.session_invalid: state vanished"))?;
            state.md5.update(&chunk);
            state.consumed += chunk.len() as u64;
        }

        let (consumed, expected) = {
            let sessions = self.sessions.lock();
            let state = sessions
                .get(&session.url)
                .ok_or_else(|| anyhow::anyhow!("localfs.session_invalid: state vanished"))?;
            (state.consumed, state.md5.clone().finalize().into())
        };
        if consumed < session.size {
            return Ok(ResumeProgress::InProgress { received: consumed });
        }

        let expected: [u8; 16] = expected;
        let entry = self
            .commit_object(
                dir_path,
                handle.dir.clone(),
                handle.stored.clone(),
                handle.name.clone(),
                handle.mime.clone(),
                handle.props.clone(),
                temp_path,
                consumed,
                expected,
                matches!(session.kind, ResumableKind::Create { .. }),
            )
            .await;
        // The claim is released with the session state, whatever the outcome.
        self.sessions.lock().remove(&session.url);
        Ok(ResumeProgress::Completed(entry?))
    }

    /// A plain filesystem has NO trash: this permanently deletes the object,
    /// exactly like [`Self::delete_permanent`]. Driven deliberately does not
    /// simulate a trash by moving objects aside - nothing would ever empty it,
    /// and a backup destination that only grows fills the drive it lives on. A
    /// missing object is success (idempotent).
    async fn trash(&self, file_id: &str) -> anyhow::Result<()> {
        self.guard_root()?;
        self.delete_object(file_id).await
    }

    async fn delete_permanent(&self, file_id: &str) -> anyhow::Result<()> {
        self.guard_root()?;
        self.delete_object(file_id).await
    }

    /// Metadata for one object.
    ///
    /// `md5` comes from the sidecar, and only when the sidecar's recorded size
    /// still matches the file's - see [`Sidecar::md5_for`] for why a mismatch
    /// reports nothing rather than a stale digest.
    async fn metadata(&self, file_id: &str) -> anyhow::Result<RemoteEntry> {
        self.guard_root()?;
        self.entry_for(file_id)
    }

    async fn download(&self, file_id: &str) -> anyhow::Result<DownloadStream> {
        self.guard_root()?;
        let path = self.path_for(file_id)?;
        let file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(not_found(file_id)),
            Err(e) => return Err(io_err(&format!("open {}", path.display()), e)),
        };
        Ok(DownloadStream(Box::new(file)))
    }

    /// Find an object under `parent_id` carrying `op_uuid`.
    ///
    /// One directory's sidecars are read; there is no index to consult and no
    /// listing to page. Scope keeps it cheap - reconciliation calls this for a
    /// single crashed op, on a single directory.
    async fn find_by_op_uuid(
        &self,
        parent_id: &str,
        op_uuid: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<Option<RemoteEntry>> {
        self.guard_root()?;
        let dir_id = layout::folder_prefix(parent_id);
        let dir_path = self.path_for(&dir_id)?;
        let mut matches: Vec<Sidecar> = self
            .live_annotated_files(&dir_path, &dir_id)?
            .into_iter()
            .filter(|s| {
                s.props
                    .get(driven_remote::props::CLIENT_OP_UUID_KEY)
                    .map(|v| v == op_uuid)
                    .unwrap_or(false)
            })
            .collect();
        if matches.len() > 1 {
            tracing::warn!(
                target: crate::TARGET,
                count = matches.len(),
                "multiple objects carry the same client op uuid; adopting the most recent"
            );
            matches.sort_by_key(|s| s.modified_ms);
        }
        match matches.pop() {
            Some(s) => Ok(Some(self.entry_for(&layout::join_id(&dir_id, &s.stored))?)),
            None => Ok(None),
        }
    }

    /// Every LIVE object id belonging to `source_id`.
    ///
    /// Walks the destination tree once and reads each directory's sidecars. An
    /// annotated object whose DATA file is gone is excluded, which is the point:
    /// the caller heals `recorded - live`, so a deleted file must read as dead
    /// and be re-uploaded, and a dangling sidecar must not make it look alive.
    ///
    /// # Completeness
    ///
    /// Every enumeration failure propagates. This never returns a partial set -
    /// a truncated answer would read as a mass deletion and churn the whole
    /// source.
    async fn list_source_object_ids(
        &self,
        source_id: &str,
        _drive_context: &DriveContext,
    ) -> anyhow::Result<HashSet<String>> {
        self.guard_root()?;
        let mut out = HashSet::new();
        for (dir_id, dir_path) in self.walk_dirs()? {
            for sidecar in self.live_annotated_files(&dir_path, &dir_id)? {
                if sidecar
                    .props
                    .get(driven_remote::props::SOURCE_ID_KEY)
                    .map(|v| v == source_id)
                    .unwrap_or(false)
                {
                    out.insert(layout::join_id(&dir_id, &sidecar.stored));
                }
            }
        }
        Ok(out)
    }

    /// Capacity of the destination VOLUME, plus what Driven's tree occupies.
    ///
    /// Unlike an object store, a local destination has a hard ceiling the user
    /// cares about, so `limit` is the filesystem's real size and `usage` is what
    /// is already used on it (by anything, not just Driven) - which is the
    /// number that predicts running out of room. `usage_in_drive` is Driven's
    /// own footprint. `usage_in_drive_trash` is always 0: there is no trash.
    ///
    /// A platform that cannot report volume capacity yields `limit: None`
    /// (unknown) rather than a guess.
    async fn about(&self) -> anyhow::Result<AboutInfo> {
        self.guard_root()?;
        let capacity = crate::fsx::volume_capacity(&self.root);
        let mut used_by_driven: u64 = 0;
        for (_, dir_path) in self.walk_dirs()? {
            let entries = std::fs::read_dir(&dir_path)
                .map_err(|e| io_err(&format!("list {}", dir_path.display()), e))?;
            for entry in entries {
                let entry =
                    entry.map_err(|e| io_err(&format!("list {}", dir_path.display()), e))?;
                let name = entry.file_name();
                if layout::is_control_entry(&name.to_string_lossy()) {
                    continue;
                }
                if let Ok(m) = entry.metadata() {
                    if m.is_file() {
                        used_by_driven = used_by_driven.saturating_add(m.len());
                    }
                }
            }
        }
        let (limit, usage) = match capacity {
            Some((total, available)) => (Some(total), total.saturating_sub(available)),
            None => (None, used_by_driven),
        };
        Ok(AboutInfo {
            limit,
            usage,
            usage_in_drive: used_by_driven,
            usage_in_drive_trash: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(root: &Path) -> LocalFsStore {
        let (destination_id, _) = crate::config::prepare_destination(root, now_ms()).unwrap();
        LocalFsStore::new(&LocalFsConfig {
            root: root.to_string_lossy().into_owned(),
            destination_id,
        })
        .unwrap()
    }

    #[test]
    fn session_urls_round_trip() {
        let handle = SessionHandle {
            dir: "a/".to_string(),
            stored: "b.txt".to_string(),
            name: "b.txt".to_string(),
            temp: ".driven-tmp-1".to_string(),
            mime: "text/plain".to_string(),
            props: HashMap::from([("driven.source_id".to_string(), "s".to_string())]),
        };
        let url = encode_session_url(&handle).unwrap();
        assert!(url.starts_with(SESSION_URL_SCHEME));
        let back = decode_session_url(&url).unwrap();
        assert_eq!(back.stored, handle.stored);
        assert_eq!(back.props, handle.props);
        assert!(decode_session_url("https://drive.google.com/x").is_err());
    }

    #[tokio::test]
    async fn every_operation_refuses_a_destination_whose_marker_is_missing() {
        // The unmounted-NAS-mount-point case: the directory is right there and
        // perfectly writable, and writing into it would put the backup on the
        // boot disk where the next mount hides it forever.
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        std::fs::remove_file(dir.path().join(names::MARKER_FILE)).unwrap();

        let err = store
            .create(
                "",
                "a.txt",
                "text/plain",
                UploadBody::Bytes(Bytes::from_static(b"x")),
                HashMap::new(),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("drive.dest_folder_missing"),
            "{err}"
        );

        for result in [
            store.list_folder("", &DriveContext::MyDrive).await.err(),
            store.metadata("a.txt").await.err(),
            store.trash("a.txt").await.err(),
            store.about().await.err(),
        ] {
            let e = result.expect("every operation must refuse an absent destination");
            assert!(e.to_string().contains("drive.dest_folder_missing"), "{e}");
        }
    }

    #[tokio::test]
    async fn a_different_drive_at_the_same_path_is_not_this_destination() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        // Simulate a DIFFERENT stick mounted where the configured one used to
        // be: same path, same shape, different identity.
        let other = DestinationMarker::new("some-other-destination", 0);
        std::fs::write(
            dir.path().join(names::MARKER_FILE),
            serde_json::to_vec(&other).unwrap(),
        )
        .unwrap();
        let err = store.about().await.unwrap_err();
        assert!(
            err.to_string().contains("drive.dest_folder_missing"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn control_files_never_appear_as_backed_up_objects() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        store
            .create(
                "",
                "real.txt",
                "text/plain",
                UploadBody::Bytes(Bytes::from_static(b"hi")),
                HashMap::new(),
            )
            .await
            .unwrap();
        // A leftover temp file from an interrupted upload.
        std::fs::write(dir.path().join(crate::fsx::temp_name()), b"partial").unwrap();

        let listed = store.list_folder("", &DriveContext::MyDrive).await.unwrap();
        let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["real.txt"], "got {names:?}");
    }

    #[tokio::test]
    async fn deleting_an_object_makes_it_dead_to_the_audit_even_though_the_sidecar_lingers() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let props = HashMap::from([(
            driven_remote::props::SOURCE_ID_KEY.to_string(),
            "src-1".to_string(),
        )]);
        let entry = store
            .create(
                "",
                "a.txt",
                "text/plain",
                UploadBody::Bytes(Bytes::from_static(b"hi")),
                props,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .list_source_object_ids("src-1", &DriveContext::MyDrive)
                .await
                .unwrap(),
            HashSet::from([entry.id.clone()])
        );

        // Delete the DATA only, leaving the sidecar - the exact state a crash
        // between the two removals produces. The audit must call it dead, or the
        // file is never re-uploaded.
        std::fs::remove_file(dir.path().join("a.txt")).unwrap();
        assert!(store
            .list_source_object_ids("src-1", &DriveContext::MyDrive)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .find_by_op_uuid("", "whatever", &DriveContext::MyDrive)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn an_object_id_from_a_tampered_state_db_cannot_write_outside_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let err = store
            .update(
                "../escaped.txt",
                UploadBody::Bytes(Bytes::from_static(b"x")),
                HashMap::new(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("localfs.id_invalid"), "{err}");
        assert!(!dir.path().parent().unwrap().join("escaped.txt").exists());
    }
}
