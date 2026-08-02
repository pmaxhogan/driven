//! The SSH/SFTP session: connect, authenticate, verify the host key, and hand
//! later slices a live SFTP channel.
//!
//! ## Host-key policy (design doc, "Connection, auth, security")
//!
//! Driven pins host keys on first use and hard-fails on any later change. The
//! policy is expressed as two entry points that are deliberately hard to
//! confuse:
//!
//! - [`SftpSession::connect`] is what every normal operation uses. It requires
//!   [`SftpConfig::host_key_fingerprint`] to be `Some`. If the server presents
//!   a different key, the connection is refused **inside the key check**,
//!   before any credential is sent, with an error naming both fingerprints. If
//!   the config is not pinned at all, it fails before the socket is even
//!   opened - an unpinned account is a bug, not a network condition.
//! - [`SftpSession::connect_and_pin`] is the account-creation / reconnect
//!   probe, and the ONLY thing allowed to accept an unknown key. It records
//!   what it saw into the config so the wizard can display it. Given an
//!   already-pinned config it behaves exactly like [`SftpSession::connect`] -
//!   pinning is not a way to launder a mismatch away.
//!
//! Reconnection ([`SftpSession::reconnect`], [`SftpSession::ensure_connected`])
//! runs the *same* code path, so a resumed session re-verifies the pin. A
//! reconnect that skipped the check would be a silent hole that every
//! connect-time test would still pass.
//!
//! ## Error mapping
//!
//! Everything failing here is translated into an [`SftpFailure`] and handed to
//! [`sftp_error`], so this module never invents a classification of its own:
//! a refused/timed-out TCP connect is `Connect` (Network), a rejected
//! credential is `AuthFailed` (AuthInvalidGrant), a host-key problem is
//! `HostKeyMismatch` / `HostKeyUnpinned` (AuthInvalidGrant), and anything that
//! breaks once the transport is up is `ConnectionLost` (Network).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use driven_remote::DriveError;
use russh::client::AuthResult;
use russh::keys::ssh_key::{HashAlg, PublicKey};
use russh::keys::{decode_secret_key, PrivateKeyWithHashAlg};
use russh_sftp::client::SftpSession as RusshSftpSession;

use crate::config::{SftpConfig, SftpCredential};
use crate::error::{sftp_error, SftpFailure};

/// How long to wait for the TCP connect before calling the host unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long everything between "the socket is open" and "the SFTP channel is
/// usable" gets: banner exchange, key exchange, authentication, channel open,
/// subsystem request.
///
/// This wrapper is NOT redundant with [`INACTIVITY_TIMEOUT`], and the
/// difference is load-bearing. Verified against russh 0.62.5
/// (`src/client/mod.rs`): `connect_stream` reads the server's identification
/// banner with `SshRead::read_ssh_id()` **inline, before** it spawns the
/// session task - and neither that call nor `ssh_read.rs` applies any timeout.
/// The inactivity timer is created inside the session run loop, so it does not
/// exist yet. A host that completes the TCP handshake and then never writes a
/// banner - a non-SSH service squatting on port 22, a NAS still waking up, a
/// silently-dropping middlebox - therefore hangs `connect_stream` forever no
/// matter what `Config` says. Only an external timeout bounds it.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often to send a keepalive on an established session.
///
/// With russh's default `keepalive_max` of 3, a peer that stops answering is
/// declared dead after roughly `KEEPALIVE_INTERVAL * 4`. This is the PRIMARY
/// half-open detector, and what makes
/// [`SftpSession::is_connected`] honest: without it, a pipe that died silently
/// (NAT timeout, yanked cable, sleeping NAS) is indistinguishable from an idle
/// one, and `ensure_connected` returns `Ok` on a corpse.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Hard cap on how long a session may go without any traffic from the server.
///
/// Deliberately several times the keepalive detection window (~60s), because
/// the ordering matters: russh only resets the inactivity timer on loop
/// iterations that did NOT just send a keepalive, so on an idle-but-healthy
/// connection it is the server's keepalive *replies* that keep this timer
/// alive. Set below the keepalive window, this would kill healthy sessions
/// against a server that ignores keepalive requests entirely. It is a backstop
/// for that case, not the main detector.
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(120);

/// The russh client config Driven connects with.
///
/// `Config::default()` leaves BOTH `inactivity_timeout` and
/// `keepalive_interval` as `None` - i.e. no liveness detection at all - which
/// is why this is spelled out rather than defaulted.
fn client_config() -> russh::client::Config {
    russh::client::Config {
        inactivity_timeout: Some(INACTIVITY_TIMEOUT),
        keepalive_interval: Some(KEEPALIVE_INTERVAL),
        // `keepalive_max` keeps russh's default of 3.
        ..Default::default()
    }
}

/// The two externally-imposed deadlines on establishing a session.
///
/// Carried as a value rather than read from the consts directly so tests can
/// drive the handshake deadline without a 30s wall-clock wait. Tokio's paused
/// clock is NOT usable for this: with a real socket it can auto-advance past
/// the connect deadline before the TCP connect resolves, so the connect
/// timeout fires instead of the handshake one and the test flakes (observed at
/// roughly 50%).
#[derive(Debug, Clone, Copy)]
struct Timeouts {
    connect: Duration,
    handshake: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            handshake: HANDSHAKE_TIMEOUT,
        }
    }
}

/// Format a public key the way OpenSSH does - `SHA256:<unpadded base64>` -
/// which is the exact string [`SftpConfig::host_key_fingerprint`] stores and
/// the wizard displays.
pub fn host_key_fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// A live SSH connection with an open SFTP subsystem channel.
///
/// One session multiplexes every operation for a store (design doc,
/// "Connection lifecycle"). It keeps the config and credential it was built
/// from so it can re-establish itself; [`SftpCredential`]'s hand-written
/// `Debug` redacts the secret material, and this type derives nothing that
/// would leak it.
pub struct SftpSession {
    config: SftpConfig,
    credential: SftpCredential,
    handle: russh::client::Handle<PinningHandler>,
    sftp: RusshSftpSession,
}

impl std::fmt::Debug for SftpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpSession")
            .field("host", &self.config.host)
            .field("port", &self.config.port)
            .field("username", &self.config.username)
            .field("connected", &self.is_connected())
            .finish_non_exhaustive()
    }
}

impl SftpSession {
    /// Connect to an already-pinned account.
    ///
    /// Fails without touching the network if the config carries no pinned
    /// fingerprint, and fails during key exchange - before authenticating - if
    /// the server's key does not match the pinned one.
    pub async fn connect(
        config: &SftpConfig,
        credential: &SftpCredential,
    ) -> Result<Self, DriveError> {
        Self::connect_with_timeouts(config, credential, Timeouts::default()).await
    }

    /// [`SftpSession::connect`] with the deadlines supplied, so a test can
    /// exercise the handshake timeout without waiting 30 real seconds. The
    /// only difference is the timing.
    async fn connect_with_timeouts(
        config: &SftpConfig,
        credential: &SftpCredential,
        timeouts: Timeouts,
    ) -> Result<Self, DriveError> {
        let Some(expected) = config.host_key_fingerprint.clone() else {
            return Err(sftp_error(SftpFailure::HostKeyUnpinned {
                detail: format!(
                    "no pinned host key for {}:{}; the account must be re-probed before it can be used",
                    config.host, config.port
                ),
            }));
        };
        let (handle, sftp, _) = establish(config, credential, Some(expected), timeouts).await?;
        Ok(Self {
            config: config.clone(),
            credential: credential.clone(),
            handle,
            sftp,
        })
    }

    /// Connect, recording the server's host-key fingerprint into `config` if
    /// it does not have one yet.
    ///
    /// This is the account-creation / reconnect probe and the only trust-on-
    /// first-use entry point. When `config` IS already pinned this behaves
    /// identically to [`SftpSession::connect`], mismatch included.
    pub async fn connect_and_pin(
        config: &mut SftpConfig,
        credential: &SftpCredential,
    ) -> Result<Self, DriveError> {
        let expected = config.host_key_fingerprint.clone();
        let (handle, sftp, observed) =
            establish(config, credential, expected, Timeouts::default()).await?;
        config.host_key_fingerprint = Some(observed);
        Ok(Self {
            config: config.clone(),
            credential: credential.clone(),
            handle,
            sftp,
        })
    }

    /// The account config this session is bound to, with the pinned
    /// fingerprint filled in.
    pub fn config(&self) -> &SftpConfig {
        &self.config
    }

    /// The open SFTP channel. Callers should go through
    /// [`SftpSession::ensure_connected`] first if the session may have been
    /// idle.
    pub fn sftp(&self) -> &RusshSftpSession {
        &self.sftp
    }

    /// Is the underlying SSH transport still up?
    ///
    /// This detects a peer-initiated close immediately, and a transport that
    /// died silently within roughly `KEEPALIVE_INTERVAL * (keepalive_max + 1)`
    /// (about a minute), because [`client_config`] configures keepalives and an
    /// inactivity backstop. It is NOT instantaneous: a pipe that broke a second
    /// ago still reads as connected until a keepalive goes unanswered. An
    /// operation that fails with a connection-lost SFTP status should therefore
    /// call [`SftpSession::reconnect`] directly rather than asking this first.
    pub fn is_connected(&self) -> bool {
        !self.handle.is_closed()
    }

    /// Re-establish the session if the transport has died. Cheap no-op when it
    /// has not.
    pub async fn ensure_connected(&mut self) -> Result<(), DriveError> {
        if self.is_connected() {
            return Ok(());
        }
        self.reconnect().await
    }

    /// Re-establish the session unconditionally, re-verifying the pinned host
    /// key on the way (this goes through the same [`establish`] path as
    /// [`SftpSession::connect`] - a reconnect never gets to skip the check).
    pub async fn reconnect(&mut self) -> Result<(), DriveError> {
        let Some(expected) = self.config.host_key_fingerprint.clone() else {
            return Err(sftp_error(SftpFailure::HostKeyUnpinned {
                detail: format!(
                    "no pinned host key for {}:{}; refusing to reconnect",
                    self.config.host, self.config.port
                ),
            }));
        };
        let (handle, sftp, _) = establish(
            &self.config,
            &self.credential,
            Some(expected),
            Timeouts::default(),
        )
        .await?;
        self.handle = handle;
        self.sftp = sftp;
        Ok(())
    }

    /// Overwrite the pinned fingerprint on a live session.
    ///
    /// Test-only, and deliberately gated on `cfg(test)` ALONE rather than
    /// also on the `test-server` feature: unpinning a live session is exactly
    /// the operation the host-key policy exists to prevent, so it must not be
    /// reachable from another crate that merely wants the test server. It
    /// exists so a test can prove that [`SftpSession::reconnect`] really
    /// re-runs the host-key check rather than trusting the connection it
    /// already had. There is no production path that repins a session in
    /// place - the wizard's reconnect flow builds a fresh config and calls
    /// [`SftpSession::connect_and_pin`].
    #[cfg(test)]
    pub fn repin_for_test(&mut self, fingerprint: Option<String>) {
        self.config.host_key_fingerprint = fingerprint;
    }

    /// Tear down the SSH transport. Test-only (`cfg(test)` alone, same
    /// reasoning as [`SftpSession::repin_for_test`]): it lets a test simulate
    /// a dead session while the server is still running, which is the only way
    /// to exercise the reconnect path deterministically.
    #[cfg(test)]
    pub async fn disconnect_for_test(&mut self) {
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "test", "en")
            .await;
    }
}

/// What the key check should do with the key the server presents.
type ExpectedFingerprint = Option<String>;

/// The shared cell the handler uses to report back across the trait boundary.
///
/// `check_server_key` returns a typed error on mismatch, but whether that
/// error surfaces out of `connect_stream` or later (as a generic transport
/// failure on the first authentication attempt) depends on where russh runs
/// key exchange. Recording the verdict here makes the outcome independent of
/// that: any failure after the check ran is re-read from this cell first.
#[derive(Default)]
struct KeyVerdict {
    observed: Mutex<Option<String>>,
    mismatch: Mutex<Option<(String, String)>>,
}

impl KeyVerdict {
    fn observed(&self) -> Option<String> {
        self.observed.lock().expect("key verdict poisoned").clone()
    }

    fn mismatch(&self) -> Option<(String, String)> {
        self.mismatch.lock().expect("key verdict poisoned").clone()
    }
}

/// The russh client handler. Its only job is the host-key decision.
struct PinningHandler {
    expected: ExpectedFingerprint,
    verdict: Arc<KeyVerdict>,
}

/// Errors a [`PinningHandler`] can produce. `russh` requires the handler's
/// error type to absorb `russh::Error`, so the mismatch rides alongside it.
#[derive(Debug)]
enum HandshakeError {
    HostKeyMismatch { expected: String, observed: String },
    Ssh(#[allow(dead_code)] russh::Error),
}

impl From<russh::Error> for HandshakeError {
    fn from(error: russh::Error) -> Self {
        Self::Ssh(error)
    }
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostKeyMismatch { expected, observed } => write!(
                f,
                "host key changed: expected {expected}, server presented {observed}"
            ),
            Self::Ssh(error) => write!(f, "{error}"),
        }
    }
}

impl russh::client::Handler for PinningHandler {
    type Error = HandshakeError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let observed = host_key_fingerprint(server_public_key);
        *self.verdict.observed.lock().expect("key verdict poisoned") = Some(observed.clone());

        match &self.expected {
            // Trust on first use: only reachable via `connect_and_pin`.
            None => Ok(true),
            Some(expected) if expected == &observed => Ok(true),
            Some(expected) => {
                // Reject from inside the check, so the handshake never
                // completes against an unverified host and no credential is
                // ever offered to it.
                *self.verdict.mismatch.lock().expect("key verdict poisoned") =
                    Some((expected.clone(), observed.clone()));
                Err(HandshakeError::HostKeyMismatch {
                    expected: expected.clone(),
                    observed,
                })
            }
        }
    }
}

/// Build the mismatch failure from whatever the handler recorded.
fn mismatch_failure(expected: &str, observed: &str) -> SftpFailure {
    SftpFailure::HostKeyMismatch {
        detail: format!(
            "the server's host key changed: pinned {expected}, but the server presented {observed}. \
             Reconnect the account only if you expect this (a rebuilt server or a rotated key); \
             otherwise treat it as an interception attempt"
        ),
    }
}

/// Open a socket, run the SSH handshake with host-key verification,
/// authenticate, and start the SFTP subsystem.
///
/// Returns the transport handle, the SFTP channel, and the fingerprint the
/// server actually presented (which `connect_and_pin` records).
async fn establish(
    config: &SftpConfig,
    credential: &SftpCredential,
    expected: ExpectedFingerprint,
    timeouts: Timeouts,
) -> Result<
    (
        russh::client::Handle<PinningHandler>,
        RusshSftpSession,
        String,
    ),
    DriveError,
> {
    let verdict = Arc::new(KeyVerdict::default());

    // The TCP connect is done here rather than inside `russh::client::connect`
    // so that "refused" and "timed out" stay distinguishable from every later
    // failure - they are the only ones that classify as `Connect`.
    let address = (config.host.as_str(), config.port);
    let stream =
        match tokio::time::timeout(timeouts.connect, tokio::net::TcpStream::connect(address)).await
        {
            Err(_elapsed) => {
                return Err(sftp_error(SftpFailure::Connect {
                    detail: format!(
                        "connecting to {}:{} timed out after {}s",
                        config.host,
                        config.port,
                        timeouts.connect.as_secs()
                    ),
                }))
            }
            Ok(Err(error)) => {
                return Err(sftp_error(SftpFailure::Connect {
                    detail: format!("connecting to {}:{}: {error}", config.host, config.port),
                }))
            }
            Ok(Ok(stream)) => stream,
        };

    let handler = PinningHandler {
        expected,
        verdict: Arc::clone(&verdict),
    };

    // Everything from the banner exchange to a usable SFTP channel is bounded
    // externally - see HANDSHAKE_TIMEOUT for why russh's own config cannot
    // cover the banner read.
    match tokio::time::timeout(
        timeouts.handshake,
        handshake(stream, handler, config, credential, &verdict),
    )
    .await
    {
        Ok(result) => result,
        Err(_elapsed) => Err(sftp_error(SftpFailure::Connect {
            detail: format!(
                "{}:{} accepted the connection but did not complete the SSH handshake within {}s \
                 (a service that is not SSH, or a host that is not finished waking up, will look \
                 like this)",
                config.host,
                config.port,
                timeouts.handshake.as_secs()
            ),
        })),
    }
}

/// The post-TCP half of [`establish`], split out so the whole of it sits under
/// one [`HANDSHAKE_TIMEOUT`].
async fn handshake(
    stream: tokio::net::TcpStream,
    handler: PinningHandler,
    config: &SftpConfig,
    credential: &SftpCredential,
    verdict: &Arc<KeyVerdict>,
) -> Result<
    (
        russh::client::Handle<PinningHandler>,
        RusshSftpSession,
        String,
    ),
    DriveError,
> {
    let mut handle = russh::client::connect_stream(Arc::new(client_config()), stream, handler)
        .await
        .map_err(|error| handshake_error(verdict, error))?;

    authenticate(&mut handle, config, credential, verdict).await?;

    let channel = handle
        .channel_open_session()
        .await
        .map_err(|error| connection_lost(verdict, format!("opening a session channel: {error}")))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|error| {
            connection_lost(verdict, format!("requesting the sftp subsystem: {error}"))
        })?;
    let sftp = RusshSftpSession::new(channel.into_stream())
        .await
        .map_err(|error| connection_lost(verdict, format!("starting the sftp session: {error}")))?;

    // The handler always records what it saw before returning; a missing value
    // would mean the handshake completed without a key check, which russh does
    // not do.
    let observed = verdict.observed().ok_or_else(|| {
        sftp_error(SftpFailure::HostKeyMismatch {
            detail: "the SSH handshake completed without presenting a host key".to_string(),
        })
    })?;

    Ok((handle, sftp, observed))
}

/// Offer the stored credential. A rejected credential comes back as
/// `Ok(AuthResult::Failure { .. })`, NOT as an `Err` - handling only the error
/// arm would silently treat a wrong password as success.
async fn authenticate(
    handle: &mut russh::client::Handle<PinningHandler>,
    config: &SftpConfig,
    credential: &SftpCredential,
    verdict: &Arc<KeyVerdict>,
) -> Result<(), DriveError> {
    let result = match credential {
        SftpCredential::Password { password } => handle
            .authenticate_password(config.username.clone(), password.clone())
            .await
            .map_err(|error| auth_transport_error(verdict, error))?,
        SftpCredential::PrivateKey { pem, passphrase } => {
            let key = decode_secret_key(pem, passphrase.as_deref()).map_err(|error| {
                sftp_error(SftpFailure::AuthFailed {
                    detail: format!("the stored private key could not be read: {error}"),
                })
            })?;
            // An RSA key PARSES fine here - `ssh-key` reads the structure
            // regardless - and would only fail later, deep inside russh, with
            // an opaque "no more auth methods" rejection. Catch it up front so
            // the user is told the key TYPE is the problem.
            if let Some(detail) = unsupported_key_algorithm(&key.algorithm()) {
                return Err(sftp_error(SftpFailure::AuthFailed { detail }));
            }
            handle
                .authenticate_publickey(
                    config.username.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), None),
                )
                .await
                .map_err(|error| auth_transport_error(verdict, error))?
        }
    };

    match result {
        AuthResult::Success => Ok(()),
        AuthResult::Failure { .. } => Err(sftp_error(SftpFailure::AuthFailed {
            detail: format!(
                "the server rejected the credentials for {}@{}:{}",
                config.username, config.host, config.port
            ),
        })),
    }
}

/// Key types this build can parse but deliberately cannot sign with.
///
/// Driven's SSH stack is compiled without russh's `rsa` feature: the only
/// pure-Rust RSA implementation available (the `rsa` crate) carries
/// RUSTSEC-2023-0071, a key-recovery timing sidechannel with no fixed release,
/// and signing with the user's own private key is exactly the operation that
/// advisory covers. Dropping the feature also drops `ssh-rsa` / `rsa-sha2-*`
/// from the host-key algorithms this client will negotiate, so a server whose
/// ONLY host key is RSA is unreachable too - rare on anything from the last
/// decade, since sshd has generated an Ed25519 host key alongside since 2014.
///
/// Re-enabling is a one-line change to the `russh` dependency once the `rsa`
/// crate ships a constant-time release.
fn unsupported_key_algorithm(algorithm: &russh::keys::Algorithm) -> Option<String> {
    // Matched rather than `Algorithm::is_rsa()`, which consumes the value.
    matches!(algorithm, russh::keys::Algorithm::Rsa { .. }).then(|| {
        "this is an RSA key, which this build cannot use for authentication (its SSH stack is \
         compiled without the RSA backend, whose only Rust implementation has an unfixed \
         key-recovery timing vulnerability). Use an Ed25519 or ECDSA key instead - generate one \
         with `ssh-keygen -t ed25519`"
            .to_string()
    })
}

/// A transport error out of `connect_stream`. If the key check already
/// rejected, that is the real cause regardless of how russh surfaced it.
fn handshake_error(verdict: &Arc<KeyVerdict>, error: HandshakeError) -> DriveError {
    if let HandshakeError::HostKeyMismatch { expected, observed } = &error {
        return sftp_error(mismatch_failure(expected, observed));
    }
    if let Some((expected, observed)) = verdict.mismatch() {
        return sftp_error(mismatch_failure(&expected, &observed));
    }
    sftp_error(SftpFailure::Connect {
        detail: format!("the SSH handshake failed: {error}"),
    })
}

/// A transport error raised while authenticating. A host-key rejection that
/// russh surfaces here (rather than out of `connect_stream`) still has to read
/// as a host-key problem, not as a dropped connection.
fn auth_transport_error(verdict: &Arc<KeyVerdict>, error: russh::Error) -> DriveError {
    if let Some((expected, observed)) = verdict.mismatch() {
        return sftp_error(mismatch_failure(&expected, &observed));
    }
    sftp_error(SftpFailure::ConnectionLost {
        detail: format!("the connection dropped while authenticating: {error}"),
    })
}

/// Something broke after authentication. Same host-key caveat as above.
fn connection_lost(verdict: &Arc<KeyVerdict>, detail: String) -> DriveError {
    if let Some((expected, observed)) = verdict.mismatch() {
        return sftp_error(mismatch_failure(&expected, &observed));
    }
    sftp_error(SftpFailure::ConnectionLost { detail })
}

#[cfg(test)]
mod tests {
    use driven_remote::remote_store::DriveErrorClassification;
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::config::SftpAuthKind;
    use crate::test_support::{TestSftpServer, TEST_USERNAME};

    fn classification(error: &DriveError) -> DriveErrorClassification {
        error.classification()
    }

    #[tokio::test]
    async fn a_password_account_connects_and_reaches_the_sftp_subsystem() {
        let server = TestSftpServer::spawn().await.unwrap();
        let config = server.pinned_config(SftpAuthKind::Password);

        let session = SftpSession::connect(&config, &server.password_credential())
            .await
            .expect("connect with the right password");

        assert!(session.is_connected());
        // Prove the channel is a working SFTP session, not just an open pipe.
        assert_eq!(session.sftp().canonicalize(".").await.unwrap(), "/");
    }

    #[tokio::test]
    async fn a_private_key_account_connects_and_reaches_the_sftp_subsystem() {
        let server = TestSftpServer::spawn().await.unwrap();
        let config = server.pinned_config(SftpAuthKind::PrivateKey);

        let session = SftpSession::connect(&config, &server.key_credential())
            .await
            .expect("connect with the generated key");

        assert_eq!(session.sftp().canonicalize(".").await.unwrap(), "/");
    }

    #[tokio::test]
    async fn a_session_can_read_and_write_the_served_directory() {
        let server = TestSftpServer::spawn().await.unwrap();
        std::fs::write(server.root().join("hello.txt"), b"from the server").unwrap();

        let session = SftpSession::connect(
            &server.pinned_config(SftpAuthKind::Password),
            &server.password_credential(),
        )
        .await
        .unwrap();

        let read = session.sftp().read("/hello.txt").await.unwrap();
        assert_eq!(read, b"from the server");

        // NOTE for later slices: `SftpSession::write` opens with `WRITE` only
        // and therefore CANNOT create a file - creating one needs the CREATE
        // flag explicitly. The server is right to answer `SSH_FX_NO_SUCH_FILE`
        // here; real sshd does the same.
        let mut file = session
            .sftp()
            .open_with_flags(
                "/written.txt",
                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
            )
            .await
            .unwrap();
        file.write_all(b"from the client").await.unwrap();
        file.shutdown().await.unwrap();
        assert_eq!(
            std::fs::read(server.root().join("written.txt")).unwrap(),
            b"from the client"
        );
    }

    #[tokio::test]
    async fn directory_listings_survive_nesting_and_drop_the_dot_entries() {
        // The server emits `.` and `..` like real sshd does (asserted against
        // the handler directly in `test_support`); `russh_sftp`'s `ReadDir`
        // iterator filters them out again, which is what a listing walk in a
        // later slice will actually see. Both halves matter - a walk built on
        // the raw `readdir` packets WOULD see them and must skip them itself.
        let server = TestSftpServer::spawn().await.unwrap();
        std::fs::create_dir(server.root().join("nested")).unwrap();
        std::fs::write(server.root().join("nested/leaf.txt"), b"leaf").unwrap();

        let session = SftpSession::connect(
            &server.pinned_config(SftpAuthKind::Password),
            &server.password_credential(),
        )
        .await
        .unwrap();

        let names: Vec<String> = session
            .sftp()
            .read_dir("/")
            .await
            .unwrap()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(names, vec!["nested".to_string()], "{names:?}");

        // A second listing must not inherit an end-of-listing latch from the
        // first - a per-session flag instead of a per-handle one breaks here.
        let nested: Vec<String> = session
            .sftp()
            .read_dir("/nested")
            .await
            .unwrap()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(nested, vec!["leaf.txt".to_string()], "{nested:?}");
    }

    #[tokio::test]
    async fn rename_refuses_to_clobber_an_existing_destination() {
        // SFTPv3 has no overwrite flag and OpenSSH's v3 handler fails the
        // request, so rename-into-place has to remove the target first. If
        // this server were more permissive, Task 3 could ship a rename that
        // only works here and fails in the e2e container.
        let server = TestSftpServer::spawn().await.unwrap();
        std::fs::write(server.root().join("from.tmp"), b"new").unwrap();
        std::fs::write(server.root().join("onto.txt"), b"old").unwrap();

        let session = SftpSession::connect(
            &server.pinned_config(SftpAuthKind::Password),
            &server.password_credential(),
        )
        .await
        .unwrap();

        session
            .sftp()
            .rename("/from.tmp", "/onto.txt")
            .await
            .expect_err("clobbering rename must be refused");

        session.sftp().remove_file("/onto.txt").await.unwrap();
        session
            .sftp()
            .rename("/from.tmp", "/onto.txt")
            .await
            .expect("rename onto a free name succeeds");
        assert_eq!(
            std::fs::read(server.root().join("onto.txt")).unwrap(),
            b"new"
        );
    }

    #[tokio::test]
    async fn a_path_climbing_out_of_the_root_is_refused() {
        let server = TestSftpServer::spawn().await.unwrap();
        let session = SftpSession::connect(
            &server.pinned_config(SftpAuthKind::Password),
            &server.password_credential(),
        )
        .await
        .unwrap();

        session
            .sftp()
            .metadata("/../../etc/passwd")
            .await
            .expect_err("the server must not serve anything above its root");
    }

    #[tokio::test]
    async fn a_wrong_password_is_an_auth_invalid_grant() {
        let server = TestSftpServer::spawn().await.unwrap();
        let config = server.pinned_config(SftpAuthKind::Password);
        let wrong = SftpCredential::Password {
            password: "definitely-not-the-fixture-password".to_string(),
        };

        let error = SftpSession::connect(&config, &wrong)
            .await
            .expect_err("a wrong password must not connect");

        assert_eq!(
            classification(&error),
            DriveErrorClassification::AuthInvalidGrant
        );
        assert!(
            format!("{error:?}").contains("sftp.auth_failed"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_wrong_private_key_is_an_auth_invalid_grant() {
        let server = TestSftpServer::spawn().await.unwrap();
        let other = TestSftpServer::spawn().await.unwrap();
        let config = server.pinned_config(SftpAuthKind::PrivateKey);

        let error = SftpSession::connect(&config, &other.key_credential())
            .await
            .expect_err("a key the server does not authorize must not connect");

        assert_eq!(
            classification(&error),
            DriveErrorClassification::AuthInvalidGrant
        );
    }

    #[tokio::test]
    async fn a_changed_host_key_hard_fails_naming_both_fingerprints() {
        let server = TestSftpServer::spawn().await.unwrap();
        let impostor = TestSftpServer::spawn().await.unwrap();

        // Point at the impostor while still pinned to the original's key -
        // the shape of a swapped-out box or a man in the middle.
        let mut config = impostor.pinned_config(SftpAuthKind::Password);
        config.host_key_fingerprint = Some(server.host_key_fingerprint().to_string());

        let error = SftpSession::connect(&config, &impostor.password_credential())
            .await
            .expect_err("a changed host key must hard-fail");

        assert_eq!(
            classification(&error),
            DriveErrorClassification::AuthInvalidGrant
        );
        let chain = format!("{error:?}");
        assert!(chain.contains("sftp.host_key_mismatch"), "{chain}");
        assert!(chain.contains(server.host_key_fingerprint()), "{chain}");
        assert!(chain.contains(impostor.host_key_fingerprint()), "{chain}");
    }

    #[tokio::test]
    async fn the_host_key_is_checked_before_any_credential_is_offered() {
        // Mismatched pin AND a wrong password: if the key check ran first the
        // error is a host-key mismatch; if authentication ran first it would
        // be an auth failure. This is the assertion that keeps the check from
        // drifting to after the handshake, where a credential would already
        // have been handed to an unverified server.
        let server = TestSftpServer::spawn().await.unwrap();
        let impostor = TestSftpServer::spawn().await.unwrap();
        let mut config = impostor.pinned_config(SftpAuthKind::Password);
        config.host_key_fingerprint = Some(server.host_key_fingerprint().to_string());

        let error = SftpSession::connect(
            &config,
            &SftpCredential::Password {
                password: "also-wrong".to_string(),
            },
        )
        .await
        .expect_err("must not connect");

        let chain = format!("{error:?}");
        assert!(chain.contains("sftp.host_key_mismatch"), "{chain}");
        assert!(!chain.contains("sftp.auth_failed"), "{chain}");
    }

    #[test]
    fn rsa_keys_are_rejected_with_an_explanation_naming_the_key_type() {
        // The guard is tested as a pure function rather than by feeding a real
        // RSA PEM through `connect`, for one practical reason: this repo is
        // public, and committing an OpenSSH private key - even a throwaway -
        // trips GitHub's secret-scanning push protection. The seam that is not
        // covered here (that `decode_secret_key` reports `Algorithm::Rsa` for
        // an RSA PEM even without russh's `rsa` feature) was verified against
        // a generated key while writing this.
        let detail = unsupported_key_algorithm(&russh::keys::Algorithm::Rsa { hash: None })
            .expect("RSA must be rejected");
        assert!(detail.contains("RSA"), "{detail}");
        assert!(detail.contains("ssh-keygen -t ed25519"), "{detail}");

        assert!(unsupported_key_algorithm(&russh::keys::Algorithm::Ed25519).is_none());
    }

    #[tokio::test]
    async fn an_unreadable_private_key_is_an_auth_invalid_grant() {
        let server = TestSftpServer::spawn().await.unwrap();
        let error = SftpSession::connect(
            &server.pinned_config(SftpAuthKind::PrivateKey),
            &SftpCredential::PrivateKey {
                pem: "-----BEGIN OPENSSH PRIVATE KEY-----\nnot a key\n".to_string(),
                passphrase: None,
            },
        )
        .await
        .expect_err("a corrupt key must not connect");

        assert_eq!(
            classification(&error),
            DriveErrorClassification::AuthInvalidGrant
        );
        assert!(
            format!("{error:?}").contains("could not be read"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn connect_refuses_an_unpinned_config() {
        let server = TestSftpServer::spawn().await.unwrap();
        let config = server.unpinned_config(SftpAuthKind::Password);

        let error = SftpSession::connect(&config, &server.password_credential())
            .await
            .expect_err("an unpinned config is not usable for a normal connect");

        assert_eq!(
            classification(&error),
            DriveErrorClassification::AuthInvalidGrant
        );
        assert!(
            format!("{error:?}").contains("sftp.host_key_unpinned"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn connect_and_pin_records_the_servers_fingerprint() {
        let server = TestSftpServer::spawn().await.unwrap();
        let mut config = server.unpinned_config(SftpAuthKind::Password);
        assert!(config.host_key_fingerprint.is_none());

        let session = SftpSession::connect_and_pin(&mut config, &server.password_credential())
            .await
            .expect("the creation probe accepts an unknown key");

        assert_eq!(
            config.host_key_fingerprint.as_deref(),
            Some(server.host_key_fingerprint())
        );
        assert_eq!(
            session.config().host_key_fingerprint.as_deref(),
            Some(server.host_key_fingerprint())
        );
        // The recorded value is usable for a subsequent plain connect.
        SftpSession::connect(&config, &server.password_credential())
            .await
            .expect("the pinned fingerprint round-trips");
    }

    #[tokio::test]
    async fn connect_and_pin_does_not_launder_a_mismatch() {
        let server = TestSftpServer::spawn().await.unwrap();
        let impostor = TestSftpServer::spawn().await.unwrap();
        let mut config = impostor.pinned_config(SftpAuthKind::Password);
        config.host_key_fingerprint = Some(server.host_key_fingerprint().to_string());

        let error = SftpSession::connect_and_pin(&mut config, &impostor.password_credential())
            .await
            .expect_err("an already-pinned config still hard-fails on a mismatch");

        assert_eq!(
            classification(&error),
            DriveErrorClassification::AuthInvalidGrant
        );
        // The stale pin is untouched - a failed probe must not overwrite it.
        assert_eq!(
            config.host_key_fingerprint.as_deref(),
            Some(server.host_key_fingerprint())
        );
    }

    #[tokio::test]
    async fn a_dead_port_is_a_network_error() {
        // Bind and immediately drop a listener to get a port nothing is on.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = SftpConfig {
            host: "127.0.0.1".to_string(),
            port,
            root_path: "/".to_string(),
            username: TEST_USERNAME.to_string(),
            auth: SftpAuthKind::Password,
            host_key_fingerprint: Some(
                "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
            ),
        };

        let error = SftpSession::connect(
            &config,
            &SftpCredential::Password {
                password: "unused".to_string(),
            },
        )
        .await
        .expect_err("nothing is listening on that port");

        assert_eq!(classification(&error), DriveErrorClassification::Network);
        assert!(
            format!("{error:?}").contains("sftp.connect_failed"),
            "{error:?}"
        );
    }

    /// A peer that completes the TCP handshake and then says nothing must not
    /// hang the connect forever. `CONNECT_TIMEOUT` does not cover this - the
    /// socket connects fine - and neither does russh's `inactivity_timeout`,
    /// because the banner is read before the session task (and its timers)
    /// exist. Only `HANDSHAKE_TIMEOUT` catches it.
    ///
    /// Runs on the real clock with an injected short handshake deadline. A
    /// paused clock was tried first and flaked about half the time: tokio
    /// auto-advanced past the connect deadline before the real TCP connect
    /// resolved, so the error was the connect timeout rather than the
    /// handshake one.
    #[tokio::test]
    async fn a_peer_that_never_sends_an_ssh_banner_times_out() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept and then hold the connection open, writing nothing. This is a
        // non-SSH service on the port, or a host that is not awake yet.
        let _silent = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });

        let config = SftpConfig {
            host: "127.0.0.1".to_string(),
            port,
            root_path: "/".to_string(),
            username: TEST_USERNAME.to_string(),
            auth: SftpAuthKind::Password,
            host_key_fingerprint: Some(
                "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
            ),
        };

        let error = SftpSession::connect_with_timeouts(
            &config,
            &SftpCredential::Password {
                password: "unused".to_string(),
            },
            Timeouts {
                connect: CONNECT_TIMEOUT,
                handshake: Duration::from_millis(250),
            },
        )
        .await
        .expect_err("a silent peer must not hang the connect");

        assert_eq!(classification(&error), DriveErrorClassification::Network);
        let chain = format!("{error:?}");
        assert!(chain.contains("sftp.connect_failed"), "{chain}");
        assert!(
            chain.contains("did not complete the SSH handshake"),
            "the handshake deadline must be what fired, not the connect one: {chain}"
        );
    }

    #[test]
    fn the_client_config_enables_liveness_detection() {
        // russh's `Config::default()` leaves both of these `None`, which makes
        // `is_connected()` blind to a half-open pipe. The ordering assertion
        // matters too: an inactivity timeout shorter than the keepalive
        // detection window would kill healthy idle sessions against a server
        // that ignores keepalives.
        let config = client_config();
        assert_eq!(config.keepalive_interval, Some(KEEPALIVE_INTERVAL));
        assert_eq!(config.inactivity_timeout, Some(INACTIVITY_TIMEOUT));
        let keepalive_window = KEEPALIVE_INTERVAL * (config.keepalive_max as u32 + 1);
        assert!(
            INACTIVITY_TIMEOUT > keepalive_window,
            "inactivity backstop {INACTIVITY_TIMEOUT:?} must outlast the keepalive window \
             {keepalive_window:?}"
        );
    }

    #[tokio::test]
    async fn ensure_connected_revives_a_dropped_session() {
        let server = TestSftpServer::spawn().await.unwrap();
        let mut session = SftpSession::connect(
            &server.pinned_config(SftpAuthKind::Password),
            &server.password_credential(),
        )
        .await
        .unwrap();

        session.disconnect_for_test().await;
        wait_until_disconnected(&session).await;

        session
            .ensure_connected()
            .await
            .expect("a dead session reconnects");
        assert!(session.is_connected());
        assert_eq!(session.sftp().canonicalize(".").await.unwrap(), "/");
    }

    #[tokio::test]
    async fn ensure_connected_is_a_no_op_while_the_session_is_alive() {
        let server = TestSftpServer::spawn().await.unwrap();
        let mut session = SftpSession::connect(
            &server.pinned_config(SftpAuthKind::Password),
            &server.password_credential(),
        )
        .await
        .unwrap();

        session.ensure_connected().await.unwrap();
        assert_eq!(session.sftp().canonicalize(".").await.unwrap(), "/");
    }

    #[tokio::test]
    async fn reconnecting_re_verifies_the_pinned_host_key() {
        // The hole this guards: a reconnect path that trusts the connection it
        // already had would sail past a host key that no longer matches.
        let server = TestSftpServer::spawn().await.unwrap();
        let mut session = SftpSession::connect(
            &server.pinned_config(SftpAuthKind::Password),
            &server.password_credential(),
        )
        .await
        .unwrap();

        let impostor_fingerprint = TestSftpServer::spawn()
            .await
            .unwrap()
            .host_key_fingerprint()
            .to_string();
        session.repin_for_test(Some(impostor_fingerprint.clone()));
        session.disconnect_for_test().await;
        wait_until_disconnected(&session).await;

        let error = session
            .ensure_connected()
            .await
            .expect_err("the reconnect must re-run the host-key check");

        assert_eq!(
            classification(&error),
            DriveErrorClassification::AuthInvalidGrant
        );
        let chain = format!("{error:?}");
        assert!(chain.contains("sftp.host_key_mismatch"), "{chain}");
        assert!(chain.contains(&impostor_fingerprint), "{chain}");
        assert!(chain.contains(server.host_key_fingerprint()), "{chain}");
    }

    #[tokio::test]
    async fn reconnecting_an_unpinned_session_is_refused() {
        let server = TestSftpServer::spawn().await.unwrap();
        let mut session = SftpSession::connect(
            &server.pinned_config(SftpAuthKind::Password),
            &server.password_credential(),
        )
        .await
        .unwrap();

        session.repin_for_test(None);
        let error = session
            .reconnect()
            .await
            .expect_err("an unpinned session must not reconnect");

        assert_eq!(
            classification(&error),
            DriveErrorClassification::AuthInvalidGrant
        );
        assert!(
            format!("{error:?}").contains("sftp.host_key_unpinned"),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn the_debug_rendering_never_carries_credentials() {
        let server = TestSftpServer::spawn().await.unwrap();
        let session = SftpSession::connect(
            &server.pinned_config(SftpAuthKind::Password),
            &server.password_credential(),
        )
        .await
        .unwrap();

        let rendered = format!("{session:?}");
        assert!(
            !rendered.contains(crate::test_support::TEST_PASSWORD),
            "{rendered}"
        );
        assert!(rendered.contains(TEST_USERNAME), "{rendered}");
    }

    /// `disconnect` is asynchronous on the wire: the transport task has to
    /// notice before `is_closed()` flips. Poll rather than sleeping a fixed
    /// amount, so this never becomes a timing flake.
    async fn wait_until_disconnected(session: &SftpSession) {
        for _ in 0..200 {
            if !session.is_connected() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the session never reported itself disconnected");
    }
}
