//! `driven-s3` - the S3-compatible backup destination.
//!
//! Implements [`driven_remote::remote_store::RemoteStore`] against any service
//! that speaks the S3 API: AWS S3, Cloudflare R2, MinIO, Backblaze B2, Wasabi,
//! Ceph RGW, and so on.
//!
//! ## Shape
//!
//! - [`config`] - the non-secret [`S3Config`] persisted in
//!   `accounts.backend_config_json`, plus the keychain-backed
//!   [`S3CredentialStore`] that holds the access key pair. **No credential ever
//!   touches SQLite, a config file, or a log line.**
//! - [`keys`] - the folder-prefix key layout and the `app_properties` codec.
//! - [`error`] - S3's XML error document mapped onto Driven's SPEC s24
//!   classification (notably `503 SlowDown`, which is throttling, not a
//!   transient server fault).
//! - [`http`] - client construction on Driven's shared `reqwest`/`rustls` stack,
//!   with the issue #34 corporate custom-CA and proxy config applied
//!   fail-closed.
//! - [`store`] - [`S3Store`], the trait implementation.
//!
//! ## Why not the AWS SDK
//!
//! Signing is delegated to `rusty-s3`, a sans-io SigV4 + XML crate. It brings no
//! HTTP client of its own, so S3 traffic rides the SAME `reqwest` client,
//! `rustls` trust store, corporate CA, proxy configuration and timeout profiles
//! as every other request Driven makes - which the AWS SDK's own hyper stack
//! would have bypassed entirely, silently defeating the custom-CA and PAC-proxy
//! support in a corporate deployment.
//!
//! ## Integrity
//!
//! Read the [`store`] module docs before touching an upload path: the executor's
//! post-upload md5 check is only meaningful because every request carries a
//! server-verified `Content-MD5`, and the multipart path additionally verifies
//! the composed ETag. Both are load-bearing.
//!
//! [`S3Config`]: config::S3Config
//! [`S3CredentialStore`]: config::S3CredentialStore
//! [`S3Store`]: store::S3Store

#![deny(missing_docs)]

pub mod config;
pub mod error;
pub mod http;
pub mod keys;
pub mod store;

pub use config::{S3Config, S3ConfigError, S3CredentialStore, S3Credentials, DEFAULT_REGION};
pub use store::{S3Store, MULTIPART_THRESHOLD, PART_SIZE};

/// Tracing target for the S3 backend.
pub(crate) const TARGET: &str = "driven::s3";
