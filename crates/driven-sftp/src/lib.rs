//! `driven-sftp` - the SSH (SFTP) backup destination.
//!
//! This crate is being built incrementally per
//! `docs/superpowers/specs/2026-08-02-sftp-backend-design.md`. It currently
//! covers configuration, credential storage, error classification, the
//! connection layer and the object/folder half of the `RemoteStore` contract;
//! resumable uploads, the complete source listing and quota land in the next
//! slice and currently fail loudly rather than answering.
//!
//! ## Shape
//!
//! - [`config`] - [`SftpConfig`] (host/port/root_path/username/auth tag/
//!   pinned host-key fingerprint), persisted in
//!   `accounts.backend_config_json`, and [`SftpCredentialStore`], which holds
//!   the actual password or private key in the OS keychain only. **No
//!   credential ever touches SQLite, a config file, or a log line.**
//! - [`error`] - SSH/SFTP failures mapped onto Driven's SPEC s24
//!   [`driven_remote::remote_store::DriveErrorClassification`], expressed over
//!   a library-neutral [`SftpFailure`] summary so the classification table is
//!   testable on its own.
//! - [`session`] - [`SftpSession`], which connects, verifies the pinned host
//!   key, authenticates with a password or private key, and exposes a live
//!   SFTP channel plus a reconnect path that re-runs the same checks.
//! - [`names`] - the destination-independent filename encoding, so a name a
//!   Linux source allows and an NTFS or exFAT share rejects still round-trips -
//!   byte-identical to `driven-localfs`, so a backup tree can move between an
//!   SFTP server and a USB stick untouched.
//! - [`meta`] - the per-object metadata sidecar carrying `app_properties`
//!   through a protocol that has nowhere to put them.
//! - [`provision`] - [`prepare_destination`], the account-creation probe. It
//!   pins the host key, stamps (or adopts) the destination-identity marker and
//!   proves the root is writable, in that order, BEFORE the caller persists an
//!   account.
//! - [`store`] - [`SftpStore`], the trait implementation. Read its module docs
//!   before touching an upload path: because SFTP returns no server-computed
//!   digest, every committed object is **re-downloaded and re-hashed**, and
//!   that digest is what the executor's post-upload check compares against.
//! - [`test_support`] - a real in-process `russh` + `russh-sftp` server over a
//!   real socket, for this crate's tests and (behind the `test-server`
//!   feature) for other crates'.
//!
//! [`SftpConfig`]: config::SftpConfig
//! [`SftpCredentialStore`]: config::SftpCredentialStore
//! [`SftpSession`]: session::SftpSession
//! [`SftpStore`]: store::SftpStore

#![deny(missing_docs)]

pub mod config;
pub mod error;
pub mod meta;
pub mod names;
pub mod provision;
pub mod session;
pub mod store;

#[cfg(any(test, feature = "test-server"))]
pub mod test_support;

pub use config::{
    DestinationMarker, SftpAuthKind, SftpConfig, SftpConfigError, SftpCredential,
    SftpCredentialStore, DEFAULT_PORT,
};
pub use error::{sftp_error, sftp_error_classification, SftpFailure};
pub use provision::{prepare_destination, PreparedDestination};
pub use session::{host_key_fingerprint, SftpSession};
pub use store::SftpStore;

/// Tracing target for the SSH (SFTP) backend.
pub(crate) const TARGET: &str = "driven::sftp";
