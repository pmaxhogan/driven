//! `driven-localfs` - the local / removable-folder backup destination.
//!
//! Implements [`driven_remote::remote_store::RemoteStore`] against a plain
//! directory tree: a USB stick, an external SSD, a NAS share, or any folder on
//! the machine. Two things it is good at that a cloud destination is not - an
//! air-gapped copy you physically own, and a restore drill that runs at disk
//! speed instead of download speed.
//!
//! ## Shape
//!
//! - [`config`] - the [`LocalFsConfig`] persisted in
//!   `accounts.backend_config_json`, and the destination-identity marker that
//!   tells an unplugged drive apart from an empty mount point. There is NO
//!   credential and this crate never touches the OS keychain.
//! - [`names`] - the destination-independent filename encoding, so a name the
//!   source allows and exFAT rejects still round-trips.
//! - [`meta`] - the per-object metadata sidecar carrying `app_properties`
//!   through a filesystem that has nowhere to put them.
//! - [`layout`] - object ids, their paths, and the collision-safe name claim
//!   that stops two case-only-different source files sharing one destination
//!   file.
//! - [`fsx`] - the crash-safe write primitives (temp file, `F_FULLFSYNC`,
//!   atomic rename, directory sync).
//! - [`error`] - errno mapped onto Driven's SPEC s24 classification, so a full
//!   disk pauses, a flapping mount retries, and an unplugged drive asks the user
//!   to plug it back in.
//! - [`store`] - [`LocalFsStore`], the trait implementation.
//!
//! ## Integrity
//!
//! Read the [`store`] module docs before touching an upload path. Because there
//! is no server to return a digest, every committed object is **re-read off the
//! destination and re-hashed**, and that digest is what the executor's
//! post-upload check compares against. Returning the in-memory digest instead
//! would make the check compare a value against itself.
//!
//! ## Durability
//!
//! This is a backup destination, so the failure that matters is a write that
//! REPORTED success and did not survive a power cut. Nothing is ever written
//! into a live object's file: content goes to a temp file in the same directory,
//! is flushed with the strongest barrier the platform offers (`F_FULLFSYNC` on
//! macOS, which a plain `fsync` is not), and is published with an atomic
//! `rename` whose directory entry is then synced too.
//!
//! [`LocalFsConfig`]: config::LocalFsConfig
//! [`LocalFsStore`]: store::LocalFsStore

#![deny(missing_docs)]

pub mod config;
pub mod error;
pub mod fsx;
pub mod layout;
pub mod meta;
pub mod names;
pub mod store;

pub use config::{
    prepare_destination, DestinationMarker, LocalFsConfig, LocalFsConfigError, PreparedDestination,
};
pub use store::LocalFsStore;

/// Tracing target for the local-folder backend.
pub(crate) const TARGET: &str = "driven::localfs";
