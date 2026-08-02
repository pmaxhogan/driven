//! An in-process SSH + SFTP server for tests.
//!
//! [`TestSftpServer`] is a REAL server: `russh` speaks the SSH transport,
//! `russh-sftp` speaks the SFTP subsystem on top of it, and the whole thing
//! runs over a real TCP socket on a free `127.0.0.1` port. Nothing here is
//! stubbed or short-circuited - a test that talks to it exercises key
//! exchange, authentication, channel setup and SFTP framing exactly as a run
//! against a NAS would. That honesty is the point: a fake that answered
//! `SftpSession::connect` without a handshake could not tell us whether the
//! host-key pinning works.
//!
//! The server is deliberately **plain**. Fault injection (mid-stream
//! disconnects, auth flaps, host-key swaps, ENOSPC, truncated readdir) belongs
//! to the chaos harness's `FaultySftpServer`, which is a separate,
//! later-landing tool. Keep this one boring.
//!
//! ## Fidelity notes (read before writing a test that depends on behaviour)
//!
//! The handler is modelled on OpenSSH's `sftp-server` where the two could
//! differ, because the e2e harness runs against real `openssh-server` - a
//! test-only server that was *more* permissive would let backend code ship
//! that then fails in e2e:
//!
//! - **`readdir` includes `.` and `..`**, like every real server. A directory
//!   walk that does not filter them will recurse forever; that is a bug in the
//!   walker, and this server is where it should surface.
//! - **`rename` refuses to clobber an existing destination** (`SSH_FX_FAILURE`).
//!   SFTPv3 has no overwrite flag and OpenSSH's v3 handler fails the request,
//!   so rename-into-place must remove the target first.
//! - Paths from the client are **virtual**: rooted at `/`, resolved against
//!   the temp directory, and any `..` that would climb above the root is
//!   refused with `SSH_FX_PERMISSION_DENIED`.
//! - `setstat` / `fsetstat` are accepted no-ops, and symlink / hardlink /
//!   extended requests are left `unimplemented` (`SSH_FX_OP_UNSUPPORTED`).
//!
//! ## Credentials
//!
//! This repository is public, so the server invents its material at run time:
//! the host key and the client keypair are generated per `spawn()`, and the
//! only constant is an obviously-synthetic password. Nothing here is or
//! resembles a real secret.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use russh::keys::ssh_key::{Algorithm, HashAlg, LineEnding, PublicKey};
use russh::keys::PrivateKey;
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::task::JoinHandle;

use crate::config::{SftpAuthKind, SftpConfig, SftpCredential};

/// The one username the test server accepts. Obviously synthetic.
pub const TEST_USERNAME: &str = "driven-test-user";

/// The one password the test server accepts. Obviously synthetic, generated
/// nowhere, and useless off `127.0.0.1`.
pub const TEST_PASSWORD: &str = "not-a-real-password-just-a-test-fixture";

/// A running in-process SSH/SFTP server serving a temporary directory.
///
/// Dropping the server aborts its accept loop and every live connection task,
/// then deletes the temp directory.
pub struct TestSftpServer {
    addr: SocketAddr,
    host_key_fingerprint: String,
    client_private_key_pem: String,
    root: TempDir,
    tasks: Arc<StdMutex<Vec<JoinHandle<()>>>>,
    accept_task: JoinHandle<()>,
}

impl TestSftpServer {
    /// Bind a free `127.0.0.1` port, generate a fresh host key and client
    /// keypair, and start serving a fresh temp directory.
    pub async fn spawn() -> anyhow::Result<Self> {
        let root = TempDir::new()?;

        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let host_key_fingerprint = fingerprint(host_key.public_key());

        let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let client_private_key_pem = client_key.to_openssh(LineEnding::LF)?.to_string();
        let authorized_key = client_key.public_key().clone();

        let config = Arc::new(russh::server::Config {
            keys: vec![host_key],
            // A test server has no reason to pay the constant-time rejection
            // delay russh applies for real deployments - a wrong-password test
            // would sleep a second per run.
            auth_rejection_time: Duration::ZERO,
            auth_rejection_time_initial: Some(Duration::ZERO),
            inactivity_timeout: Some(Duration::from_secs(120)),
            ..Default::default()
        });

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;

        // A std (not tokio) mutex, so `Drop` can take it unconditionally: the
        // accept loop only ever holds it for a synchronous `push`, never
        // across an await, and `JoinHandle::abort` is not synchronous - the
        // accept task can still be mid-push when `Drop` runs. A `try_lock`
        // that lost that race would leave connection tasks alive with a path
        // into the temp directory being deleted.
        let tasks: Arc<StdMutex<Vec<JoinHandle<()>>>> = Arc::new(StdMutex::new(Vec::new()));
        let accept_task = tokio::spawn({
            let tasks = Arc::clone(&tasks);
            let root_path = root.path().to_path_buf();
            async move {
                loop {
                    let Ok((stream, _peer)) = listener.accept().await else {
                        break;
                    };
                    let config = Arc::clone(&config);
                    let handler = SshHandler::new(root_path.clone(), authorized_key.clone());
                    let connection = tokio::spawn(async move {
                        match russh::server::run_stream(config, stream, handler).await {
                            Ok(session) => {
                                if let Err(error) = session.await {
                                    tracing::debug!(%error, "test sftp server session ended");
                                }
                            }
                            Err(error) => {
                                tracing::debug!(%error, "test sftp server handshake failed");
                            }
                        }
                    });
                    tasks.lock().expect("task list poisoned").push(connection);
                }
            }
        });

        Ok(Self {
            addr,
            host_key_fingerprint,
            client_private_key_pem,
            root,
            tasks,
            accept_task,
        })
    }

    /// The address the server is listening on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The host the server is listening on (always the loopback literal).
    pub fn host(&self) -> String {
        self.addr.ip().to_string()
    }

    /// The port the server is listening on.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The server's host-key fingerprint, in the same OpenSSH-style
    /// `SHA256:<base64>` form the client pins.
    pub fn host_key_fingerprint(&self) -> &str {
        &self.host_key_fingerprint
    }

    /// The filesystem directory the server exposes as its virtual `/`.
    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// The OpenSSH-format private key the server accepts for public-key auth.
    /// Generated per `spawn()`; never a checked-in key.
    pub fn client_private_key_pem(&self) -> &str {
        &self.client_private_key_pem
    }

    /// An [`SftpConfig`] pointing at this server with the host key already
    /// pinned - the shape an account has after a successful creation probe.
    pub fn pinned_config(&self, auth: SftpAuthKind) -> SftpConfig {
        SftpConfig {
            host: self.host(),
            port: self.port(),
            root_path: "/".to_string(),
            username: TEST_USERNAME.to_string(),
            auth,
            host_key_fingerprint: Some(self.host_key_fingerprint.clone()),
        }
    }

    /// An [`SftpConfig`] pointing at this server with NO pinned fingerprint -
    /// the shape an account has *during* creation, before the probe records
    /// one.
    pub fn unpinned_config(&self, auth: SftpAuthKind) -> SftpConfig {
        SftpConfig {
            host_key_fingerprint: None,
            ..self.pinned_config(auth)
        }
    }

    /// The password credential this server accepts.
    pub fn password_credential(&self) -> SftpCredential {
        SftpCredential::Password {
            password: TEST_PASSWORD.to_string(),
        }
    }

    /// The private-key credential this server accepts.
    pub fn key_credential(&self) -> SftpCredential {
        SftpCredential::PrivateKey {
            pem: self.client_private_key_pem.clone(),
            passphrase: None,
        }
    }
}

impl Drop for TestSftpServer {
    fn drop(&mut self) {
        self.accept_task.abort();
        // The lock is never held across an await, so taking it here cannot
        // block on I/O even though `Drop` may run on a runtime thread. A
        // poisoned lock means a connection task panicked; abort what we can.
        let tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(poisoned) => poisoned.into_inner(),
        };
        for task in tasks.iter() {
            task.abort();
        }
    }
}

/// Format a public key the way OpenSSH (and the pinning client) does:
/// `SHA256:<unpadded base64 of the SHA-256 digest>`.
pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// The per-connection SSH handler: authenticates the one configured user and
/// hands an accepted `sftp` subsystem request to [`FsSftpHandler`].
struct SshHandler {
    root: PathBuf,
    authorized_key: PublicKey,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl SshHandler {
    fn new(root: PathBuf, authorized_key: PublicKey) -> Self {
        Self {
            root,
            authorized_key,
            channels: HashMap::new(),
        }
    }
}

impl russh::server::Handler for SshHandler {
    type Error = anyhow::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == TEST_USERNAME && password == TEST_PASSWORD {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        // Compare the key material, not the whole `PublicKey`, so a differing
        // trailing comment does not read as a different key.
        if user == TEST_USERNAME && public_key.key_data() == self.authorized_key.key_data() {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel_id)?;
            return Ok(());
        }
        let Some(channel) = self.channels.remove(&channel_id) else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        session.channel_success(channel_id)?;
        russh_sftp::server::run(channel.into_stream(), FsSftpHandler::new(self.root.clone())).await;
        Ok(())
    }
}

/// An open SFTP handle. Handles are opaque minted strings, NOT paths: two
/// concurrent handles on the same file must not share a cursor, and a
/// directory listing needs its own end-of-listing latch.
enum OpenHandle {
    File(tokio::fs::File),
    Dir { entries: Vec<File>, drained: bool },
}

/// A filesystem-backed SFTP subsystem handler rooted at a directory.
struct FsSftpHandler {
    root: PathBuf,
    handles: HashMap<String, OpenHandle>,
    next_handle: u64,
}

impl FsSftpHandler {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            handles: HashMap::new(),
            next_handle: 0,
        }
    }

    fn mint_handle(&mut self, open: OpenHandle) -> String {
        self.next_handle += 1;
        let id = format!("h{}", self.next_handle);
        self.handles.insert(id.clone(), open);
        id
    }

    fn file_mut(&mut self, handle: &str) -> Result<&mut tokio::fs::File, StatusCode> {
        match self.handles.get_mut(handle) {
            Some(OpenHandle::File(file)) => Ok(file),
            _ => Err(StatusCode::Failure),
        }
    }

    /// Resolve a client-supplied virtual path to a real path inside the root.
    fn resolve(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let mut real = self.root.clone();
        for segment in virtual_segments(path)? {
            real.push(segment);
        }
        // Belt and braces behind [`virtual_segments`]'s character rules. A
        // real runtime check rather than a `debug_assert`, because the
        // `test-server` feature can be built in release (the chaos harness),
        // and "the guard silently disappears in the build that runs unattended
        // for hours" is exactly the wrong tradeoff for a containment check.
        if !real.starts_with(&self.root) {
            return Err(StatusCode::PermissionDenied);
        }
        Ok(real)
    }
}

/// Split a client path into normalized segments, refusing anything that would
/// escape the virtual root. Relative paths (including `.`) are resolved
/// against `/`, which is what a client's `canonicalize(".")` expects.
///
/// Two distinct escapes are refused:
///
/// 1. A `..` that pops above the root.
/// 2. A segment containing `\` or `:`. On POSIX both are ordinary filename
///    characters, but this server also runs on Windows CI, where
///    `PathBuf::push` gives them path meaning: pushing `C:\Windows` REPLACES
///    the root entirely (`push` of an absolute path discards the base), and a
///    backslash nests into subdirectories instead of being one literal
///    character. So `/C:\Windows\system32` would serve the real system
///    directory. Refusing these characters on EVERY platform is deliberate -
///    it costs a filename shape no Driven object id uses, and it keeps the
///    server's behaviour identical across CI runners, so a test that passes on
///    macOS cannot fail on Windows.
fn virtual_segments(path: &str) -> Result<Vec<String>, StatusCode> {
    let mut segments: Vec<String> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.pop().is_none() {
                    return Err(StatusCode::PermissionDenied);
                }
            }
            other => {
                if other.contains('\\') || other.contains(':') {
                    return Err(StatusCode::PermissionDenied);
                }
                segments.push(other.to_string())
            }
        }
    }
    Ok(segments)
}

/// Render normalized segments back as an absolute virtual path.
fn virtual_path(segments: &[String]) -> String {
    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

fn io_status(error: &std::io::Error) -> StatusCode {
    match error.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        // SFTPv3 has no EEXIST / ENOTEMPTY / ENOSPC code - every server folds
        // them into SSH_FX_FAILURE and leaves the message as the only signal.
        _ => StatusCode::Failure,
    }
}

fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_string(),
        language_tag: "en-US".to_string(),
    }
}

impl russh_sftp::server::Handler for FsSftpHandler {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = self.resolve(&filename)?;
        let options = std::fs::OpenOptions::from(pflags);
        let file = options.open(&path).map_err(|e| io_status(&e))?;
        let handle = self.mint_handle(OpenHandle::File(tokio::fs::File::from_std(file)));
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        match self.handles.remove(&handle) {
            Some(_) => Ok(ok_status(id)),
            None => Err(StatusCode::Failure),
        }
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let file = self.file_mut(&handle)?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| io_status(&e))?;
        let mut data = vec![0u8; len as usize];
        let mut filled = 0usize;
        while filled < data.len() {
            let read = file
                .read(&mut data[filled..])
                .await
                .map_err(|e| io_status(&e))?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(filled);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let file = self.file_mut(&handle)?;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| io_status(&e))?;
        file.write_all(&data).await.map_err(|e| io_status(&e))?;
        Ok(ok_status(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let path = self.resolve(&path)?;
        let metadata = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|e| io_status(&e))?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let path = self.resolve(&path)?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|e| io_status(&e))?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let file = self.file_mut(&handle)?;
        let metadata = file.metadata().await.map_err(|e| io_status(&e))?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        // Accepted no-op: the backend does not depend on remote mode/mtime,
        // and a temp-dir server has nothing meaningful to apply.
        self.resolve(&path)?;
        Ok(ok_status(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        handle: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        self.file_mut(&handle)?;
        Ok(ok_status(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let real = self.resolve(&path)?;
        let metadata = tokio::fs::metadata(&real)
            .await
            .map_err(|e| io_status(&e))?;
        if !metadata.is_dir() {
            return Err(StatusCode::Failure);
        }

        // `.` and `..` first, exactly as a real server reports them.
        let mut entries = vec![
            File::new(".", FileAttributes::from(&metadata)),
            File::new("..", FileAttributes::from(&metadata)),
        ];
        let mut reader = tokio::fs::read_dir(&real)
            .await
            .map_err(|e| io_status(&e))?;
        while let Some(entry) = reader.next_entry().await.map_err(|e| io_status(&e))? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = entry.metadata().await.map_err(|e| io_status(&e))?;
            entries.push(File::new(name, FileAttributes::from(&metadata)));
        }

        let handle = self.mint_handle(OpenHandle::Dir {
            entries,
            drained: false,
        });
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        // The end-of-listing latch is PER HANDLE. A per-session flag would
        // report EOF for a nested listing opened while an outer one is live.
        match self.handles.get_mut(&handle) {
            Some(OpenHandle::Dir { entries, drained }) => {
                if *drained {
                    return Err(StatusCode::Eof);
                }
                *drained = true;
                Ok(Name {
                    id,
                    files: entries.clone(),
                })
            }
            _ => Err(StatusCode::Failure),
        }
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let path = self.resolve(&filename)?;
        tokio::fs::remove_file(&path)
            .await
            .map_err(|e| io_status(&e))?;
        Ok(ok_status(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = self.resolve(&path)?;
        tokio::fs::create_dir(&path)
            .await
            .map_err(|e| io_status(&e))?;
        Ok(ok_status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let path = self.resolve(&path)?;
        tokio::fs::remove_dir(&path)
            .await
            .map_err(|e| io_status(&e))?;
        Ok(ok_status(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let old = self.resolve(&oldpath)?;
        let new = self.resolve(&newpath)?;
        // SFTPv3 has no overwrite flag, and OpenSSH's v3 handler refuses to
        // clobber. Matching that here keeps a backend from shipping a
        // rename-into-place that only works against this server.
        if tokio::fs::symlink_metadata(&new).await.is_ok() {
            return Err(StatusCode::Failure);
        }
        tokio::fs::rename(&old, &new)
            .await
            .map_err(|e| io_status(&e))?;
        Ok(ok_status(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let segments = virtual_segments(&path)?;
        Ok(Name {
            id,
            files: vec![File::dummy(virtual_path(&segments))],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_that_climbs_above_the_root_is_refused() {
        assert_eq!(
            virtual_segments("/a/../../etc/passwd").unwrap_err(),
            StatusCode::PermissionDenied
        );
        assert_eq!(
            virtual_segments("..").unwrap_err(),
            StatusCode::PermissionDenied
        );
    }

    #[test]
    fn a_windows_drive_or_backslash_segment_is_refused() {
        // Both shapes escape the root on Windows via `PathBuf::push`, and both
        // must be refused on every platform so CI runners agree.
        for path in [
            "/C:\\Windows\\system32",
            "C:/Windows",
            "/a/C:",
            "/a\\..\\..\\b",
            "/sub\\nested",
        ] {
            assert_eq!(
                virtual_segments(path).unwrap_err(),
                StatusCode::PermissionDenied,
                "{path:?} must not resolve"
            );
        }
    }

    #[test]
    fn resolution_never_leaves_the_root() {
        let root = TempDir::new().unwrap();
        let handler = FsSftpHandler::new(root.path().to_path_buf());

        // The containment guard holds for everything that IS accepted...
        for path in ["/", ".", "/a/b", "/a/./b/../c", "deep/nested/leaf.txt"] {
            let resolved = handler.resolve(path).expect("{path:?} should resolve");
            assert!(
                resolved.starts_with(root.path()),
                "{path:?} resolved outside the root: {resolved:?}"
            );
        }
        // ...and the escapes are refused outright.
        for path in ["/../outside", "/C:\\Windows"] {
            assert_eq!(
                handler.resolve(path).unwrap_err(),
                StatusCode::PermissionDenied,
                "{path:?}"
            );
        }
    }

    #[test]
    fn virtual_paths_normalize_to_an_absolute_form() {
        assert_eq!(virtual_path(&virtual_segments(".").unwrap()), "/");
        assert_eq!(virtual_path(&virtual_segments("/").unwrap()), "/");
        assert_eq!(
            virtual_path(&virtual_segments("/a/./b/../c").unwrap()),
            "/a/c"
        );
        assert_eq!(virtual_path(&virtual_segments("a/b").unwrap()), "/a/b");
    }

    #[tokio::test]
    async fn the_server_binds_a_free_loopback_port_and_reports_a_sha256_fingerprint() {
        let server = TestSftpServer::spawn().await.unwrap();
        assert_eq!(server.addr().ip().to_string(), "127.0.0.1");
        assert_ne!(server.port(), 0);
        assert!(
            server.host_key_fingerprint().starts_with("SHA256:"),
            "{}",
            server.host_key_fingerprint()
        );
        assert!(server.root().is_dir());
        assert!(server
            .client_private_key_pem()
            .starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
    }

    #[tokio::test]
    async fn readdir_reports_the_dot_entries_and_ends_with_eof_per_handle() {
        // Asserted against the handler directly, because `russh_sftp`'s
        // client-side `ReadDir` iterator filters `.` and `..` back out - so a
        // client-level test could not tell an honest server from one that
        // omitted them, and a listing walk built on raw `readdir` packets
        // would be surprised in production.
        use russh_sftp::server::Handler as _;

        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("leaf.txt"), b"leaf").unwrap();
        let mut handler = FsSftpHandler::new(root.path().to_path_buf());

        let handle = handler.opendir(1, "/".to_string()).await.unwrap().handle;
        let names: Vec<String> = handler
            .readdir(2, handle.clone())
            .await
            .unwrap()
            .files
            .into_iter()
            .map(|file| file.filename)
            .collect();
        assert!(names.contains(&".".to_string()), "{names:?}");
        assert!(names.contains(&"..".to_string()), "{names:?}");
        assert!(names.contains(&"leaf.txt".to_string()), "{names:?}");

        assert_eq!(
            handler.readdir(3, handle).await.unwrap_err(),
            StatusCode::Eof,
            "a drained handle must report EOF"
        );

        // A second handle starts undrained: the latch is per handle.
        let second = handler.opendir(4, "/".to_string()).await.unwrap().handle;
        assert!(!handler.readdir(5, second).await.unwrap().files.is_empty());
    }

    #[tokio::test]
    async fn two_servers_get_distinct_host_keys() {
        let a = TestSftpServer::spawn().await.unwrap();
        let b = TestSftpServer::spawn().await.unwrap();
        assert_ne!(a.host_key_fingerprint(), b.host_key_fingerprint());
    }
}
