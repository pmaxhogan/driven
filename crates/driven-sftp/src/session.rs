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
/// The SSH handshake and authentication that follow are governed by russh's
/// own timeouts.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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
        let Some(expected) = config.host_key_fingerprint.clone() else {
            return Err(sftp_error(SftpFailure::HostKeyUnpinned {
                detail: format!(
                    "no pinned host key for {}:{}; the account must be re-probed before it can be used",
                    config.host, config.port
                ),
            }));
        };
        let (handle, sftp, _) = establish(config, credential, Some(expected)).await?;
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
        let (handle, sftp, observed) = establish(config, credential, expected).await?;
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
    /// This detects a transport that has gone away (peer closed, keepalives
    /// exhausted, inactivity timeout). An operation that fails with a
    /// connection-lost SFTP status while the transport still looks alive
    /// should call [`SftpSession::reconnect`] directly rather than relying on
    /// this.
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
        let (handle, sftp, _) = establish(&self.config, &self.credential, Some(expected)).await?;
        self.handle = handle;
        self.sftp = sftp;
        Ok(())
    }

    /// Overwrite the pinned fingerprint on a live session.
    ///
    /// Test-only: it exists so a test can prove that
    /// [`SftpSession::reconnect`] really re-runs the host-key check rather
    /// than trusting the connection it already had. There is no production
    /// path that repins a session in place - the wizard's reconnect flow
    /// builds a fresh config and calls [`SftpSession::connect_and_pin`].
    #[cfg(any(test, feature = "test-server"))]
    pub fn repin_for_test(&mut self, fingerprint: Option<String>) {
        self.config.host_key_fingerprint = fingerprint;
    }

    /// Tear down the SSH transport. Test-only: it lets a test simulate a dead
    /// session while the server is still running, which is the only way to
    /// exercise the reconnect path deterministically.
    #[cfg(any(test, feature = "test-server"))]
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
    let stream = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect(address),
    )
    .await
    {
        Err(_elapsed) => {
            return Err(sftp_error(SftpFailure::Connect {
                detail: format!(
                    "connecting to {}:{} timed out after {}s",
                    config.host,
                    config.port,
                    CONNECT_TIMEOUT.as_secs()
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
    let mut handle =
        russh::client::connect_stream(Arc::new(russh::client::Config::default()), stream, handler)
            .await
            .map_err(|error| handshake_error(&verdict, error))?;

    authenticate(&mut handle, config, credential, &verdict).await?;

    let channel = handle.channel_open_session().await.map_err(|error| {
        connection_lost(&verdict, format!("opening a session channel: {error}"))
    })?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|error| {
            connection_lost(&verdict, format!("requesting the sftp subsystem: {error}"))
        })?;
    let sftp = RusshSftpSession::new(channel.into_stream())
        .await
        .map_err(|error| {
            connection_lost(&verdict, format!("starting the sftp session: {error}"))
        })?;

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
