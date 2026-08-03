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
//! ## Fault injection
//!
//! The server is **plain until something arms it**. Every switch in
//! [`SftpFaultCounts`]'s companion `arm_*` family is off after `spawn()`, so a
//! test that does not mention faults talks to an honest server, and the chaos
//! harness's `SftpFixture` is the only caller that turns any of them on. The
//! set is deliberately closed: it is exactly the hazards the chaos rows
//! consume (a transport cut mid-stream, an auth flap, a host-key swap, a full
//! remote disk, a truncated directory enumeration, a `statvfs` extension that
//! is advertised and then refused) and nothing was added "while we are here" -
//! this is a fixture, not a general fault framework.
//!
//! Two of them are worth reading before use, because what they inject is not
//! quite what their name suggests:
//!
//! - [`TestSftpServer::arm_disconnect_after_bytes`] cuts the **TCP stream**,
//!   not the SFTP channel. Killing only the channel would leave
//!   `russh::client::Handle` open, and
//!   [`SftpSession::is_connected`](crate::session::SftpSession::is_connected)
//!   reads exactly that - so the store would keep handing out a dead SFTP
//!   session instead of reconnecting, and the row would be measuring a
//!   fixture artefact. Cutting the transport is also the more honest model of
//!   the thing being simulated: a yanked cable, a NAT timeout, a NAS going to
//!   sleep. The budget counts ENCRYPTED transport bytes arriving from the
//!   client, so pick one comfortably above a handshake (a few KiB) and below
//!   the payload.
//! - [`TestSftpServer::arm_truncated_readdir`] serves a PARTIAL first batch and
//!   then fails the enumeration. It cannot simulate the other truncation - a
//!   server that quietly returns fewer entries and then a clean EOF - because
//!   that is indistinguishable from a smaller directory at the protocol level,
//!   by any client. See [`TestSftpServer::arm_truncated_readdir`]'s docs.
//!
//! **[`TestSftpServer::corrupt_committed_bytes_after_rename`] predates the
//! rest and is not a chaos switch.** The store's integrity protocol
//! re-downloads and re-hashes every object it commits, and that step is only
//! load-bearing if something can actually corrupt the bytes between the rename
//! and the verify - otherwise every test passes just as happily against an
//! implementation that returns the digest it accumulated while writing, which
//! is the `x == x` check the module docs of [`crate::store`] warn about. There
//! is no way to stage that corruption from the client side, so after a
//! successful rename it flips one byte of the destination IN PLACE (the length
//! is preserved on purpose, so the sidecar's size guard cannot mask the digest
//! failure).
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
//! - `setstat` / `fsetstat` are accepted no-ops, and symlink / hardlink
//!   requests are left `unimplemented` (`SSH_FX_OP_UNSUPPORTED`).
//! - **`statvfs@openssh.com` v2 is advertised and answered**, because OpenSSH's
//!   `sftp-server` advertises it and `about()` reads it for the quota display.
//!   Plenty of real servers do not (embedded NAS firmware, restricted
//!   `internal-sftp` builds), so [`TestSftpServer::spawn_without_statvfs`]
//!   serves the other half of that fork.
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
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};
use std::time::Duration;

use russh::keys::ssh_key::{Algorithm, HashAlg, LineEnding, PublicKey};
use russh::keys::PrivateKey;
use russh::server::{Auth, Msg, Session};
use russh::{Channel, ChannelId};
use russh_sftp::extensions::{self, Statvfs};
use russh_sftp::protocol::{
    Attrs, Data, ExtendedReply, File, FileAttributes, Handle, Name, OpenFlags, Packet, Status,
    StatusCode, Version,
};
use tempfile::TempDir;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::task::JoinHandle;

use crate::config::{DestinationMarker, SftpAuthKind, SftpConfig, SftpCredential};
use crate::names;

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
    alternate_host_key_fingerprint: String,
    client_private_key_pem: String,
    root: TempDir,
    tasks: Arc<StdMutex<Vec<JoinHandle<()>>>>,
    accept_task: JoinHandle<()>,
    faults: Arc<SftpFaults>,
}

/// The value every byte-budget switch holds while it is DISARMED.
///
/// `u64::MAX` rather than a `None`, so the hot paths (`poll_read`, `write`)
/// read one relaxed atomic and compare, with no lock and no allocation.
const DISARMED: u64 = u64::MAX;

/// How many times each armed fault actually fired.
///
/// Every chaos row asserts on one of these before it asserts anything else:
/// a row that never reached its fault has tested nothing, and every assertion
/// about "what must not happen afterwards" is trivially true on a run where
/// nothing happened at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SftpFaultCounts {
    /// Transport cuts served (see [`TestSftpServer::arm_disconnect_after_bytes`]).
    pub disconnects: u64,
    /// SFTP channels ended while the SSH transport underneath stayed up
    /// (see [`TestSftpServer::arm_channel_close_after_bytes`]).
    pub channel_closures: u64,
    /// Bytes of file data the destination accepted before it started reporting
    /// ENOSPC. A row asserting "the disk filled up MID-transfer" needs this to
    /// be non-zero, or it is really testing "full before we started".
    pub bytes_accepted_before_enospc: u64,
    /// Authentication attempts rejected by the flap, password and key alike.
    pub auth_rejections: u64,
    /// Connections served with the ALTERNATE host key.
    pub host_key_swaps: u64,
    /// Writes refused because the injected byte budget was exhausted.
    pub enospc_refusals: u64,
    /// Directory enumerations cut short after their partial first batch.
    pub truncated_readdirs: u64,
    /// `statvfs@openssh.com` requests refused after being advertised.
    pub statvfs_refusals: u64,
}

/// The fault switches, shared by the accept loop and every live connection.
///
/// All atomics: the switches are flipped from a test's own task while
/// connection tasks read them, and nothing here is worth a lock.
#[derive(Debug, Default)]
struct SftpFaults {
    // -- transport cut -------------------------------------------------------
    /// Client-to-server transport bytes allowed before the socket is cut.
    disconnect_after_bytes: AtomicU64,
    /// Bytes seen since the switch was armed (reset on each arming).
    client_bytes: AtomicU64,
    disconnects: AtomicU64,

    // -- channel-only death --------------------------------------------------
    /// Bytes the SFTP CHANNEL will carry before it reports end-of-stream,
    /// leaving the SSH transport alive underneath it.
    close_channel_after_bytes: AtomicU64,
    /// Bytes seen on the channel since the switch was armed.
    channel_bytes: AtomicU64,
    channel_closures: AtomicU64,

    // -- auth flap -----------------------------------------------------------
    /// Authentication attempts still to be rejected before one is accepted.
    auth_rejections_remaining: AtomicU64,
    auth_rejections: AtomicU64,

    // -- host-key swap -------------------------------------------------------
    /// Latched: once set, EVERY later connection presents the alternate key.
    swap_host_key: AtomicBool,
    host_key_swaps: AtomicU64,

    // -- remote disk full ----------------------------------------------------
    /// Bytes the destination will still accept before it reports ENOSPC.
    enospc_after_bytes: AtomicU64,
    /// Bytes written since the switch was armed (reset on each arming).
    written_bytes: AtomicU64,
    /// Set the first time a write is refused. Without it a SMALLER write that
    /// still fits under the budget would succeed after a larger one was
    /// refused, which is not what a full filesystem does.
    enospc_latched: AtomicBool,
    enospc_refusals: AtomicU64,

    // -- truncated enumeration -----------------------------------------------
    /// Entries the first `readdir` batch may carry before the enumeration is
    /// failed on the next call.
    truncate_readdir_after: AtomicU64,
    truncated_readdirs: AtomicU64,

    // -- statvfs -------------------------------------------------------------
    /// Refuse `statvfs@openssh.com` even though `init` advertised it.
    refuse_statvfs: AtomicBool,
    statvfs_refusals: AtomicU64,

    // -- post-rename corruption (predates the chaos switches) ----------------
    corrupt_after_rename: AtomicBool,
}

impl SftpFaults {
    /// A disarmed set. `Default` would leave the byte budgets at 0, which reads
    /// as "cut immediately" rather than "off".
    fn disarmed() -> Self {
        Self {
            disconnect_after_bytes: AtomicU64::new(DISARMED),
            close_channel_after_bytes: AtomicU64::new(DISARMED),
            enospc_after_bytes: AtomicU64::new(DISARMED),
            truncate_readdir_after: AtomicU64::new(DISARMED),
            ..Self::default()
        }
    }

    fn clear(&self) {
        self.disconnect_after_bytes
            .store(DISARMED, Ordering::SeqCst);
        self.client_bytes.store(0, Ordering::SeqCst);
        self.close_channel_after_bytes
            .store(DISARMED, Ordering::SeqCst);
        self.channel_bytes.store(0, Ordering::SeqCst);
        self.auth_rejections_remaining.store(0, Ordering::SeqCst);
        self.swap_host_key.store(false, Ordering::SeqCst);
        self.enospc_after_bytes.store(DISARMED, Ordering::SeqCst);
        self.written_bytes.store(0, Ordering::SeqCst);
        self.enospc_latched.store(false, Ordering::SeqCst);
        self.truncate_readdir_after
            .store(DISARMED, Ordering::SeqCst);
        self.refuse_statvfs.store(false, Ordering::SeqCst);
        self.corrupt_after_rename.store(false, Ordering::SeqCst);
    }

    fn counts(&self) -> SftpFaultCounts {
        SftpFaultCounts {
            disconnects: self.disconnects.load(Ordering::SeqCst),
            channel_closures: self.channel_closures.load(Ordering::SeqCst),
            bytes_accepted_before_enospc: self.written_bytes.load(Ordering::SeqCst),
            auth_rejections: self.auth_rejections.load(Ordering::SeqCst),
            host_key_swaps: self.host_key_swaps.load(Ordering::SeqCst),
            enospc_refusals: self.enospc_refusals.load(Ordering::SeqCst),
            truncated_readdirs: self.truncated_readdirs.load(Ordering::SeqCst),
            statvfs_refusals: self.statvfs_refusals.load(Ordering::SeqCst),
        }
    }

    /// Account for `n` transport bytes read from a client. `true` means the
    /// budget is spent and this socket must die now.
    ///
    /// SINGLE-SHOT: the switch disarms itself as it fires, so the client's
    /// reconnect succeeds and the row measures RECOVERY rather than a permanent
    /// outage. The `swap` is also what makes the disarm race-free when several
    /// connections are reading at once - exactly one of them sees the armed
    /// value and counts the cut.
    fn note_client_bytes(&self, n: u64) -> bool {
        let budget = self.disconnect_after_bytes.load(Ordering::SeqCst);
        if budget == DISARMED {
            return false;
        }
        if self.client_bytes.fetch_add(n, Ordering::SeqCst) + n < budget {
            return false;
        }
        if self.disconnect_after_bytes.swap(DISARMED, Ordering::SeqCst) == DISARMED {
            return false;
        }
        self.disconnects.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Account for `n` bytes read off the SFTP CHANNEL. `true` means the
    /// channel must now report end-of-stream, leaving the transport alive.
    ///
    /// End-of-stream rather than an io error, on purpose: `russh_sftp`'s server
    /// loop breaks only on `UnexpectedEof` and merely warns-and-continues on any
    /// other error, so an error would spin the loop instead of ending the
    /// subsystem. EOF is also the truthful shape - `sftp-server` exiting closes
    /// its stdout.
    ///
    /// Single-shot, like the transport cut, so a reconnect can succeed.
    fn note_channel_bytes(&self, n: u64) -> bool {
        let budget = self.close_channel_after_bytes.load(Ordering::SeqCst);
        if budget == DISARMED {
            return false;
        }
        if self.channel_bytes.fetch_add(n, Ordering::SeqCst) + n < budget {
            return false;
        }
        if self
            .close_channel_after_bytes
            .swap(DISARMED, Ordering::SeqCst)
            == DISARMED
        {
            return false;
        }
        self.channel_closures.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Whether this authentication attempt is inside the flap.
    fn reject_auth(&self) -> bool {
        let remaining = self.auth_rejections_remaining.load(Ordering::SeqCst);
        if remaining == 0 {
            return false;
        }
        self.auth_rejections_remaining
            .store(remaining - 1, Ordering::SeqCst);
        self.auth_rejections.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Whether this write exceeds the injected byte budget. LATCHED: a full
    /// disk stays full until [`TestSftpServer::clear_faults`] frees it.
    fn refuse_write(&self, n: u64) -> bool {
        let budget = self.enospc_after_bytes.load(Ordering::SeqCst);
        if budget == DISARMED {
            return false;
        }
        if !self.enospc_latched.load(Ordering::SeqCst)
            && self.written_bytes.load(Ordering::SeqCst) + n <= budget
        {
            self.written_bytes.fetch_add(n, Ordering::SeqCst);
            return false;
        }
        self.enospc_latched.store(true, Ordering::SeqCst);
        self.enospc_refusals.fetch_add(1, Ordering::SeqCst);
        true
    }
}

/// The quota numbers [`TestSftpServer`] reports through `statvfs@openssh.com`.
///
/// Chosen so every derived figure is exact in a test assertion and none of them
/// coincide: a 1 TiB filesystem with 400 GiB free to root and 360 GiB free to
/// an unprivileged user, so `blocks_free` and `blocks_avail` cannot be confused
/// for one another by an implementation that reads the wrong field.
pub const TEST_STATVFS: Statvfs = Statvfs {
    block_size: 4096,
    fragment_size: 4096,
    blocks: 268_435_456,
    blocks_free: 104_857_600,
    blocks_avail: 94_371_840,
    inodes: 65_536_000,
    inodes_free: 64_000_000,
    inodes_avail: 64_000_000,
    fs_id: 0,
    flags: 0,
    name_max: 255,
};

impl TestSftpServer {
    /// Bind a free `127.0.0.1` port, generate a fresh host key and client
    /// keypair, and start serving a fresh temp directory.
    ///
    /// Advertises `statvfs@openssh.com` v2, as OpenSSH's own `sftp-server`
    /// does.
    pub async fn spawn() -> anyhow::Result<Self> {
        Self::spawn_with_features(ServerFeatures { statvfs: true }).await
    }

    /// A server that advertises NO extensions - the shape of an embedded NAS
    /// or a restricted `internal-sftp` build, where `about()` has to report an
    /// unknown quota rather than guess one.
    pub async fn spawn_without_statvfs() -> anyhow::Result<Self> {
        Self::spawn_with_features(ServerFeatures { statvfs: false }).await
    }

    async fn spawn_with_features(features: ServerFeatures) -> anyhow::Result<Self> {
        let root = TempDir::new()?;
        let faults = Arc::new(SftpFaults::disarmed());

        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let host_key_fingerprint = fingerprint(host_key.public_key());
        // Generated up front, not on demand: a host-key swap has to be
        // observable as a DIFFERENT fingerprint the moment it is armed, and
        // minting one inside the accept loop would make the fixture's own
        // `alternate_host_key_fingerprint()` a promise it could not keep.
        let alternate_host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let alternate_host_key_fingerprint = fingerprint(alternate_host_key.public_key());

        let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let client_private_key_pem = client_key.to_openssh(LineEnding::LF)?.to_string();
        let authorized_key = client_key.public_key().clone();

        let server_config = |key: PrivateKey| {
            Arc::new(russh::server::Config {
                keys: vec![key],
                // A test server has no reason to pay the constant-time
                // rejection delay russh applies for real deployments - a
                // wrong-password test would sleep a second per run.
                auth_rejection_time: Duration::ZERO,
                auth_rejection_time_initial: Some(Duration::ZERO),
                inactivity_timeout: Some(Duration::from_secs(120)),
                ..Default::default()
            })
        };
        let config = server_config(host_key);
        let alternate_config = server_config(alternate_host_key);

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
            let faults = Arc::clone(&faults);
            async move {
                loop {
                    let Ok((stream, _peer)) = listener.accept().await else {
                        break;
                    };
                    // The host key is chosen PER CONNECTION, so a swap armed
                    // mid-run is seen by the reconnect rather than needing the
                    // whole fixture rebuilt (which would discard the objects
                    // the assertion is about).
                    let config = if faults.swap_host_key.load(Ordering::SeqCst) {
                        faults.host_key_swaps.fetch_add(1, Ordering::SeqCst);
                        Arc::clone(&alternate_config)
                    } else {
                        Arc::clone(&config)
                    };
                    let handler = SshHandler::new(
                        root_path.clone(),
                        authorized_key.clone(),
                        features,
                        Arc::clone(&faults),
                    );
                    let stream = FaultStream {
                        inner: stream,
                        faults: Arc::clone(&faults),
                    };
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
            alternate_host_key_fingerprint,
            client_private_key_pem,
            root,
            tasks,
            accept_task,
            faults,
        })
    }

    /// Make every subsequent successful `rename` flip one byte of the
    /// destination file IN PLACE, simulating a server (or a disk, or a link)
    /// that damaged an object between the publish and the read-back.
    ///
    /// The length is deliberately preserved: a truncation would also trip the
    /// sidecar's size guard, so the test would no longer be evidence that the
    /// DIGEST check works. See the module docs for why this one exists at all.
    pub fn corrupt_committed_bytes_after_rename(&self, enabled: bool) {
        self.faults
            .corrupt_after_rename
            .store(enabled, Ordering::SeqCst);
    }

    // -- fault injection ------------------------------------------------------

    /// How many times each armed fault has fired.
    pub fn fault_counts(&self) -> SftpFaultCounts {
        self.faults.counts()
    }

    /// Disarm every fault and reset the byte budgets, leaving the counters
    /// alone so a row can still prove its fault fired earlier.
    pub fn clear_faults(&self) {
        self.faults.clear();
    }

    /// Cut the TCP connection once `bytes` more transport bytes have arrived
    /// from a client, then disarm.
    ///
    /// The cut is at the **transport**, not the SFTP channel: see the module
    /// docs for why (`is_connected` reads the SSH handle, so a channel-only
    /// kill would leave the store using a corpse). `bytes` counts ENCRYPTED
    /// bytes, so it includes framing and any handshake that happens after
    /// arming - pick a budget well above a handshake (a few KiB) and well below
    /// the payload, and the cut lands mid-transfer every run.
    ///
    /// Single-shot on purpose: the reconnect must succeed, or the row measures
    /// a permanent outage instead of recovery from a blip.
    pub fn arm_disconnect_after_bytes(&self, bytes: u64) {
        self.faults.client_bytes.store(0, Ordering::SeqCst);
        self.faults
            .disconnect_after_bytes
            .store(bytes, Ordering::SeqCst);
    }

    /// End the SFTP CHANNEL once `bytes` more have crossed it, leaving the SSH
    /// transport alive underneath - the shape of `sftp-server` crashing or
    /// being OOM-killed while `sshd` carries on.
    ///
    /// This is a strictly nastier hazard than
    /// [`Self::arm_disconnect_after_bytes`] and it exists to prove a specific
    /// guard: `SftpSession::is_connected` reads the SSH session handle, so it
    /// reports HEALTHY here even though the channel carrying every SFTP request
    /// is gone. A write path bounded on liveness alone would wait forever. See
    /// `driven_sftp::store::while_connected`.
    ///
    /// Single-shot, so a reconnect succeeds and recovery is measurable.
    pub fn arm_channel_close_after_bytes(&self, bytes: u64) {
        self.faults.channel_bytes.store(0, Ordering::SeqCst);
        self.faults
            .close_channel_after_bytes
            .store(bytes, Ordering::SeqCst);
    }

    /// Reject the next `attempts` authentication requests - password and
    /// public key alike - and accept normally afterwards.
    ///
    /// Models a NAS that is up but not yet ready (PAM still starting, a
    /// directory service briefly unreachable), which is a very different
    /// thing from a wrong credential even though the wire looks identical.
    pub fn arm_auth_failures(&self, attempts: u64) {
        self.faults
            .auth_rejections_remaining
            .store(attempts, Ordering::SeqCst);
    }

    /// Present the ALTERNATE host key on every connection from now on.
    ///
    /// Latched rather than single-shot, because that is what the hazard is: a
    /// server's key does not flicker, it changes - a rebuild, a restored
    /// image, or someone standing in the middle. Driven must refuse it and
    /// keep refusing it, not retry until it happens to work.
    pub fn arm_host_key_swap(&self) {
        self.faults.swap_host_key.store(true, Ordering::SeqCst);
    }

    /// Accept `bytes` more bytes of file data, then refuse every write with the
    /// `SSH_FX_FAILURE` + "No space left on device" shape a real server reports
    /// a full disk with.
    ///
    /// SFTPv3 has no ENOSPC status code, so the MESSAGE is the only signal -
    /// which is exactly why `driven-sftp` classifies on it
    /// ([`crate::error`]'s `ENOSPC_MARKERS`) and why this fixture sends a
    /// realistic one rather than a bare status.
    ///
    /// Latched: a full disk stays full until [`Self::clear_faults`].
    pub fn arm_enospc_after_bytes(&self, bytes: u64) {
        self.faults.written_bytes.store(0, Ordering::SeqCst);
        self.faults.enospc_latched.store(false, Ordering::SeqCst);
        self.faults
            .enospc_after_bytes
            .store(bytes, Ordering::SeqCst);
    }

    /// Serve at most `entries` names in a directory's first `readdir` batch,
    /// then FAIL the enumeration instead of reporting `SSH_FX_EOF`.
    ///
    /// ## What this can and cannot simulate
    ///
    /// A client discovers the end of a directory by reading batches until the
    /// server says `EOF`. There are therefore two ways an enumeration can come
    /// up short, and only one of them is detectable by anybody:
    ///
    /// - **The server cuts the enumeration with an error.** That is this
    ///   switch, and a client MUST NOT hand the partial batch back as though it
    ///   were the whole directory. `driven-sftp` relies on that all the way up:
    ///   `list_source_object_ids` computes `dead = recorded - live`, so a short
    ///   listing read as complete is a mass-deletion signal.
    /// - **The server quietly returns fewer entries and then a clean `EOF`.**
    ///   Indistinguishable from a smaller directory - there is no count to
    ///   check it against, in this protocol or any other. Not simulated here
    ///   because it cannot be detected there; recorded as a real gap rather
    ///   than a fault with a fake assertion attached.
    ///
    /// Latched while armed, so a recursive walk fails at the first directory
    /// large enough to be truncated.
    pub fn arm_truncated_readdir(&self, entries: u64) {
        self.faults
            .truncate_readdir_after
            .store(entries, Ordering::SeqCst);
    }

    /// Keep advertising `statvfs@openssh.com` in `init` but refuse the request
    /// itself with `SSH_FX_OP_UNSUPPORTED`.
    ///
    /// This is the third quota case, and the only one
    /// [`Self::spawn_without_statvfs`] cannot reach: a server that promises the
    /// extension and then does not answer. `about()` has to degrade to an
    /// unknown limit rather than failing, because a quota display is not worth
    /// failing an operation over.
    pub fn arm_statvfs_refusal(&self) {
        self.faults.refuse_statvfs.store(true, Ordering::SeqCst);
    }

    // -- destination provisioning ---------------------------------------------

    /// Write Driven's destination marker into the served root and return the
    /// destination id it records.
    ///
    /// The store proves this marker before every MUTATING operation
    /// (`SftpStore::guard_root`), so a caller outside this crate cannot drive a
    /// single upload against a bare fixture. Stamping it directly - rather than
    /// running [`crate::provision::prepare_destination`] - keeps the helper
    /// synchronous and usable from a fixture constructor, and the
    /// `a_marked_root_is_the_same_thing_the_real_probe_produces` test is what
    /// stops the two shapes drifting.
    pub fn mark_as_destination(&self) -> String {
        self.mark_directory(self.root())
    }

    /// The same, for a sub-directory of the served root (created if needed) -
    /// the shape an account whose `root_path` is not `/` has.
    pub fn mark_as_destination_in(&self, relative: &str) -> anyhow::Result<String> {
        let dir = self.root().join(relative);
        std::fs::create_dir_all(&dir)?;
        Ok(self.mark_directory(&dir))
    }

    fn mark_directory(&self, dir: &Path) -> String {
        let destination_id = uuid::Uuid::new_v4().to_string();
        let marker = DestinationMarker::new(&destination_id, 1_700_000_000_000);
        std::fs::write(
            dir.join(names::MARKER_FILE),
            serde_json::to_vec(&marker).expect("a marker serializes"),
        )
        .expect("the fixture owns its own temp directory");
        destination_id
    }

    /// The destination id recorded in `directory`'s marker, so a SECOND store
    /// can be built against a root that is already marked instead of re-marking
    /// it under a new id.
    pub fn destination_id_in(directory: &Path) -> anyhow::Result<String> {
        let raw = std::fs::read(directory.join(names::MARKER_FILE))?;
        let marker: DestinationMarker = serde_json::from_slice(&raw)?;
        Ok(marker.destination_id)
    }

    /// A pinned [`SftpConfig`] for a root this call has just marked as a Driven
    /// destination - everything `SftpStore::new` needs to run a mutating
    /// operation, in one line.
    pub fn prepared_config(&self, auth: SftpAuthKind) -> SftpConfig {
        let destination_id = self.mark_as_destination();
        SftpConfig {
            destination_id: Some(destination_id),
            ..self.pinned_config(auth)
        }
    }

    /// The fingerprint the server presents once [`Self::arm_host_key_swap`] has
    /// been called - the "this is not the server you pinned" value.
    pub fn alternate_host_key_fingerprint(&self) -> &str {
        &self.alternate_host_key_fingerprint
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
            // The creation probe (and, in this crate's tests, the
            // `seed_destination` helper) is what initializes a destination and
            // records its id; a bare config from the fixture has none.
            destination_id: None,
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

/// Which optional SFTP extensions this server advertises.
#[derive(Clone, Copy, Debug)]
struct ServerFeatures {
    statvfs: bool,
}

/// The accepted TCP stream, wrapped so an armed budget can cut it mid-transfer.
///
/// Wrapping the SOCKET rather than the SFTP channel is the whole point - see
/// the module docs. Everything except the read budget is a straight delegation.
struct FaultStream<S> {
    inner: S,
    faults: Arc<SftpFaults>,
}

impl<S: AsyncRead + Unpin> AsyncRead for FaultStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) {
            let read = (buf.filled().len() - before) as u64;
            if this.faults.note_client_bytes(read) {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "test sftp server: injected transport cut",
                )));
            }
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for FaultStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// The SFTP channel stream, wrapped so an armed budget can END THE SUBSYSTEM
/// while the SSH transport underneath stays up.
///
/// Reports end-of-stream rather than an error: `russh_sftp`'s server loop
/// breaks on `UnexpectedEof` and warns-and-continues on anything else, so an
/// error would busy-spin the loop instead of ending the subsystem. EOF is also
/// what really happens when `sftp-server` exits - its stdout closes.
struct ChannelFaultStream<S> {
    inner: S,
    faults: Arc<SftpFaults>,
}

impl<S: AsyncRead + Unpin> AsyncRead for ChannelFaultStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) {
            let read = (buf.filled().len() - before) as u64;
            if this.faults.note_channel_bytes(read) {
                // Rewind to an EMPTY read, which is how tokio spells EOF.
                // Handing back the bytes AND the EOF would let the server
                // process one more packet before noticing.
                buf.set_filled(before);
            }
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ChannelFaultStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// The per-connection SSH handler: authenticates the one configured user and
/// hands an accepted `sftp` subsystem request to [`FsSftpHandler`].
struct SshHandler {
    root: PathBuf,
    authorized_key: PublicKey,
    channels: HashMap<ChannelId, Channel<Msg>>,
    features: ServerFeatures,
    faults: Arc<SftpFaults>,
}

impl SshHandler {
    fn new(
        root: PathBuf,
        authorized_key: PublicKey,
        features: ServerFeatures,
        faults: Arc<SftpFaults>,
    ) -> Self {
        Self {
            root,
            authorized_key,
            channels: HashMap::new(),
            features,
            faults,
        }
    }
}

impl russh::server::Handler for SshHandler {
    type Error = anyhow::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if self.faults.reject_auth() {
            return Ok(Auth::reject());
        }
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
        if self.faults.reject_auth() {
            return Ok(Auth::reject());
        }
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
        // The channel stream gets its OWN wrapper, separate from the socket's:
        // ending the subsystem while `sshd` lives is a different hazard from
        // losing the transport, and the store's guards answer them differently.
        let stream = ChannelFaultStream {
            inner: channel.into_stream(),
            faults: Arc::clone(&self.faults),
        };
        russh_sftp::server::run(
            stream,
            FsSftpHandler::new(self.root.clone(), self.features, Arc::clone(&self.faults)),
        )
        .await;
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
    features: ServerFeatures,
    faults: Arc<SftpFaults>,
}

impl FsSftpHandler {
    fn new(root: PathBuf, features: ServerFeatures, faults: Arc<SftpFaults>) -> Self {
        Self {
            root,
            handles: HashMap::new(),
            next_handle: 0,
            features,
            faults,
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

    /// Advertise the extensions this server actually answers.
    ///
    /// `russh-sftp`'s client reads this list once, at session setup, and
    /// `SftpSession::fs_info` returns `Ok(None)` without a round trip when
    /// `statvfs@openssh.com` is not advertised at version `"2"` - so this is
    /// the ONLY place the two `about()` branches can be told apart.
    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        let mut version = Version::new();
        if self.features.statvfs {
            version
                .extensions
                .insert(extensions::STATVFS.to_string(), "2".to_string());
        }
        Ok(version)
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
        if self.faults.refuse_write(data.len() as u64) {
            // Returned as an `Ok(Status)` carrying a non-OK code, NOT as
            // `Err(StatusCode)`: the error path builds its message from the
            // status code's own name, and the MESSAGE is the entire signal
            // here - SFTPv3 has no ENOSPC code, so a bare `SSH_FX_FAILURE`
            // would classify as a retryable transient rather than a full disk.
            return Ok(Status {
                id,
                status_code: StatusCode::Failure,
                error_message: "write failed: No space left on device".to_string(),
                language_tag: "en-US".to_string(),
            });
        }
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
        let truncate_after = self.faults.truncate_readdir_after.load(Ordering::SeqCst);
        // The end-of-listing latch is PER HANDLE. A per-session flag would
        // report EOF for a nested listing opened while an outer one is live.
        match self.handles.get_mut(&handle) {
            Some(OpenHandle::Dir { entries, drained }) => {
                if *drained {
                    // An armed truncation ends the enumeration with a FAILURE
                    // where an honest server would say EOF. The client has
                    // already been handed a partial batch, so this is the
                    // moment a client that swallowed the error would report a
                    // short directory as a complete one.
                    if truncate_after != DISARMED {
                        self.faults
                            .truncated_readdirs
                            .fetch_add(1, Ordering::SeqCst);
                        return Err(StatusCode::Failure);
                    }
                    return Err(StatusCode::Eof);
                }
                *drained = true;
                let mut files = entries.clone();
                if truncate_after != DISARMED {
                    files.truncate(truncate_after as usize);
                }
                Ok(Name { id, files })
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
        if self.faults.corrupt_after_rename.load(Ordering::SeqCst) {
            // Flip one byte, keeping the length: see the module docs. A failure
            // here is silent because the caller's next read is what the test
            // actually asserts on.
            if let Ok(mut bytes) = tokio::fs::read(&new).await {
                if let Some(first) = bytes.first_mut() {
                    *first ^= 0xFF;
                    let _ = tokio::fs::write(&new, &bytes).await;
                }
            }
        }
        Ok(ok_status(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let segments = virtual_segments(&path)?;
        Ok(Name {
            id,
            files: vec![File::dummy(virtual_path(&segments))],
        })
    }

    /// Answers `statvfs@openssh.com` with [`TEST_STATVFS`] when the extension
    /// is advertised; every other extension stays `SSH_FX_OP_UNSUPPORTED`.
    ///
    /// A server that advertises the extension in `init` and then refuses the
    /// request is a third case Driven has to survive. It is reachable only
    /// through [`TestSftpServer::arm_statvfs_refusal`] - the advertisement and
    /// the answer are otherwise wired to the same switch, so the fixture cannot
    /// drift into that state by accident.
    async fn extended(
        &mut self,
        id: u32,
        request: String,
        _data: Vec<u8>,
    ) -> Result<Packet, Self::Error> {
        if request != extensions::STATVFS || !self.features.statvfs {
            return Err(StatusCode::OpUnsupported);
        }
        if self.faults.refuse_statvfs.load(Ordering::SeqCst) {
            self.faults.statvfs_refusals.fetch_add(1, Ordering::SeqCst);
            return Err(StatusCode::OpUnsupported);
        }
        let data = russh_sftp::ser::to_bytes(&TEST_STATVFS)
            .map_err(|_| StatusCode::Failure)?
            .to_vec();
        Ok(Packet::ExtendedReply(ExtendedReply { id, data }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handler over `root` with the default feature set and no fault
    /// injection - what a plain connection gets.
    fn handler_for(root: &Path) -> FsSftpHandler {
        FsSftpHandler::new(
            root.to_path_buf(),
            ServerFeatures { statvfs: true },
            Arc::new(SftpFaults::disarmed()),
        )
    }

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
        let handler = handler_for(root.path());

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
        let mut handler = handler_for(root.path());

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
        assert_ne!(
            a.host_key_fingerprint(),
            a.alternate_host_key_fingerprint(),
            "the swap key must be a genuinely different key, or arming a swap changes nothing"
        );
    }

    /// A freshly spawned server must be indistinguishable from one with no
    /// fault machinery at all. This is the property that lets every existing
    /// test keep talking to an honest server, and the reason the byte budgets
    /// are `DISARMED` rather than `Default`-zero (zero reads as "cut on the
    /// first byte", which would break every connection ever made).
    #[tokio::test]
    async fn every_fault_is_off_until_something_arms_it() {
        let server = TestSftpServer::spawn().await.unwrap();
        assert_eq!(server.fault_counts(), SftpFaultCounts::default());

        let faults = SftpFaults::disarmed();
        assert!(!faults.note_client_bytes(u64::MAX / 2));
        assert!(!faults.reject_auth());
        assert!(!faults.refuse_write(u64::MAX / 2));
        assert!(!faults.refuse_statvfs.load(Ordering::SeqCst));
        assert!(!faults.swap_host_key.load(Ordering::SeqCst));
        assert!(!faults.corrupt_after_rename.load(Ordering::SeqCst));
        assert_eq!(faults.counts(), SftpFaultCounts::default());
    }

    /// The transport cut fires ONCE, at the budget, and disarms itself so the
    /// reconnect that follows succeeds.
    #[test]
    fn the_transport_cut_fires_once_at_the_budget_and_then_disarms() {
        let faults = SftpFaults::disarmed();
        faults.client_bytes.store(0, Ordering::SeqCst);
        faults.disconnect_after_bytes.store(100, Ordering::SeqCst);

        assert!(!faults.note_client_bytes(60), "still inside the budget");
        assert!(faults.note_client_bytes(60), "the budget is spent");
        assert_eq!(faults.counts().disconnects, 1);
        assert!(
            !faults.note_client_bytes(10_000),
            "a single-shot cut must not keep cutting, or the client can never reconnect"
        );
        assert_eq!(faults.counts().disconnects, 1);
    }

    /// The auth flap must be exhaustible: `n` rejections then normal service.
    #[test]
    fn the_auth_flap_rejects_exactly_the_armed_number_of_attempts() {
        let faults = SftpFaults::disarmed();
        faults.auth_rejections_remaining.store(2, Ordering::SeqCst);
        assert!(faults.reject_auth());
        assert!(faults.reject_auth());
        assert!(!faults.reject_auth(), "the flap is over");
        assert_eq!(faults.counts().auth_rejections, 2);
    }

    /// ENOSPC is LATCHED, unlike the transport cut: a full disk does not empty
    /// itself, and a row that recovers has to free the space explicitly.
    #[test]
    fn the_write_budget_latches_once_it_is_exhausted() {
        let faults = SftpFaults::disarmed();
        faults.written_bytes.store(0, Ordering::SeqCst);
        faults.enospc_after_bytes.store(10, Ordering::SeqCst);

        assert!(!faults.refuse_write(6), "room for the first write");
        assert!(faults.refuse_write(6), "the second write does not fit");
        assert!(faults.refuse_write(1), "and the disk stays full");
        assert_eq!(faults.counts().enospc_refusals, 2);

        faults.clear();
        assert!(!faults.refuse_write(1_000), "clearing frees the space");
        assert_eq!(
            faults.counts().enospc_refusals,
            2,
            "clearing disarms the switch but must not erase the evidence a row asserts on"
        );
    }

    /// A truncated enumeration serves a PARTIAL batch and then fails, which is
    /// the only truncation a client can detect at all (see the arming doc).
    #[tokio::test]
    async fn a_truncated_readdir_serves_a_partial_batch_and_then_fails() {
        use russh_sftp::server::Handler as _;

        let root = TempDir::new().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(root.path().join(name), b"x").unwrap();
        }
        let faults = Arc::new(SftpFaults::disarmed());
        // 3 entries + `.` + `..` = 5; ask for 2, so the batch is genuinely short.
        faults.truncate_readdir_after.store(2, Ordering::SeqCst);
        let mut handler = FsSftpHandler::new(
            root.path().to_path_buf(),
            ServerFeatures { statvfs: true },
            Arc::clone(&faults),
        );

        let handle = handler.opendir(1, "/".to_string()).await.unwrap().handle;
        let first = handler.readdir(2, handle.clone()).await.unwrap();
        assert_eq!(first.files.len(), 2, "the first batch is truncated");
        assert_eq!(
            handler.readdir(3, handle).await.unwrap_err(),
            StatusCode::Failure,
            "the enumeration must END IN AN ERROR - an EOF here is the bug this fault hunts"
        );
        assert_eq!(faults.counts().truncated_readdirs, 1);
    }

    /// The marker the fixture stamps must be the SAME artefact the real
    /// creation probe produces, or every chaos row provisions a destination the
    /// app would never create.
    ///
    /// Asserted by round trip rather than by comparing bytes: the real probe
    /// ADOPTS a marker it recognizes and reports the id it found, so an
    /// `Adopted` verdict carrying the fixture's own id is proof the two agree
    /// about both the filename and the schema.
    #[tokio::test]
    async fn a_marked_root_is_the_same_thing_the_real_probe_produces() {
        let server = TestSftpServer::spawn().await.unwrap();
        let stamped = server.mark_as_destination();

        let mut config = server.unpinned_config(SftpAuthKind::Password);
        let outcome = crate::provision::prepare_destination(
            &mut config,
            &server.password_credential(),
            1_700_000_000_000,
        )
        .await
        .expect("the real probe accepts a root the fixture marked");

        assert_eq!(
            outcome,
            crate::provision::PreparedDestination::Adopted,
            "the probe must ADOPT the fixture's marker, not re-initialize over it"
        );
        assert_eq!(config.destination_id.as_deref(), Some(stamped.as_str()));
        assert_eq!(
            TestSftpServer::destination_id_in(server.root()).unwrap(),
            stamped
        );
    }

    /// `prepared_config` is the one-liner outside callers use, so it has to
    /// produce a config a store will actually mutate through.
    #[tokio::test]
    async fn a_prepared_config_is_pinned_and_carries_the_destination_id() {
        let server = TestSftpServer::spawn().await.unwrap();
        let config = server.prepared_config(SftpAuthKind::Password);
        assert_eq!(
            config.host_key_fingerprint.as_deref(),
            Some(server.host_key_fingerprint())
        );
        assert_eq!(
            config.destination_id,
            Some(TestSftpServer::destination_id_in(server.root()).unwrap())
        );
    }

    /// A sub-directory root is the shape an account whose `root_path` is not
    /// `/` has, and the store's own scoping tests need it.
    #[tokio::test]
    async fn a_sub_directory_can_be_marked_as_its_own_destination() {
        let server = TestSftpServer::spawn().await.unwrap();
        let id = server.mark_as_destination_in("backups").unwrap();
        assert_eq!(
            TestSftpServer::destination_id_in(&server.root().join("backups")).unwrap(),
            id
        );
        assert!(
            !server.root().join(names::MARKER_FILE).exists(),
            "marking a sub-directory must not also mark the root"
        );
    }
}
