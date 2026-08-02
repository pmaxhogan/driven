//! `driven-sftp` - the SSH (SFTP) backup destination.
//!
//! This crate is being built incrementally per
//! `docs/superpowers/specs/2026-08-02-sftp-backend-design.md`. This first slice
//! lays the foundation only: the non-secret [`SftpConfig`], the
//! keychain-backed [`SftpCredentialStore`], and the SPEC s24 error mapping in
//! [`error`]. The `RemoteStore` implementation itself (the `russh`/
//! `russh-sftp` session layer, upload/download/list/rename) lands in a later
//! slice - there is deliberately no `russh` dependency yet.
//!
//! ## Shape
//!
//! - [`config`] - [`SftpConfig`] (host/port/root_path/username/auth tag/
//!   pinned host-key fingerprint), persisted in
//!   `accounts.backend_config_json`, and [`SftpCredentialStore`], which holds
//!   the actual password or private key in the OS keychain only. **No
//!   credential ever touches SQLite, a config file, or a log line.**
//! - [`error`] - SSH/SFTP failures mapped onto Driven's SPEC s24
//!   [`driven_remote::remote_store::DriveErrorClassification`], independent of
//!   any particular `russh`/`russh-sftp` error type so the later session layer
//!   can wire real errors onto an already-tested table.
//!
//! [`SftpConfig`]: config::SftpConfig
//! [`SftpCredentialStore`]: config::SftpCredentialStore

#![deny(missing_docs)]

pub mod config;
pub mod error;

pub use config::{
    SftpAuthKind, SftpConfig, SftpConfigError, SftpCredential, SftpCredentialStore, DEFAULT_PORT,
};
pub use error::{sftp_error, sftp_error_classification, SftpFailure};
