//! `driven-sftp` - the SSH (SFTP) backup destination.
//!
//! This crate is being built incrementally per
//! `docs/superpowers/specs/2026-08-02-sftp-backend-design.md`. It currently
//! covers configuration, credential storage, error classification and the
//! connection layer; the `RemoteStore` implementation itself
//! (upload/download/list/rename over the session below) lands in a later
//! slice.
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
//! - [`test_support`] - a real in-process `russh` + `russh-sftp` server over a
//!   real socket, for this crate's tests and (behind the `test-server`
//!   feature) for other crates'.
//!
//! [`SftpConfig`]: config::SftpConfig
//! [`SftpCredentialStore`]: config::SftpCredentialStore
//! [`SftpSession`]: session::SftpSession

#![deny(missing_docs)]

pub mod config;
pub mod error;
pub mod session;

#[cfg(any(test, feature = "test-server"))]
pub mod test_support;

pub use config::{
    SftpAuthKind, SftpConfig, SftpConfigError, SftpCredential, SftpCredentialStore, DEFAULT_PORT,
};
pub use error::{sftp_error, sftp_error_classification, SftpFailure};
pub use session::{host_key_fingerprint, SftpSession};
