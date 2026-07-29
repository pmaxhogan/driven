//! `driven-remote` - the backend-neutral contract every backup destination
//! implements.
//!
//! This crate holds the pieces that are shared by ALL destinations and by the
//! sync engine that drives them:
//!
//! - [`remote_store`] - the [`RemoteStore`] trait and its value types.
//! - [`error`] - the classified [`DriveError`] taxonomy (SPEC s24) the executor
//!   downcasts to for pacer / circuit-breaker verdicts.
//! - [`retry`] - the exponential-backoff + classification middleware.
//! - [`props`] - the `app_properties` identity vocabulary.
//! - [`backend`] - [`BackendKind`], the per-account destination selector.
//!
//! It deliberately performs NO I/O and builds NO HTTP clients: it depends on
//! `reqwest` only to classify a `reqwest::Error` and to sleep between retries.
//! Concrete backends (`driven-drive`, and the S3-compatible store) live in
//! their own crates and are assembled by `driven-backend`'s factory, keeping
//! `driven-core` free of any destination-specific dependency.
//!
//! ## Why these types moved here
//!
//! They all used to live in `driven-drive`, back when Google Drive was the only
//! destination. A second backend would have had to depend on the whole Drive
//! crate (oauth2, keyring, the Drive REST client) just to name the trait it
//! implements - or, worse, re-declare the `app_properties` keys, which the
//! executor's own comments call out as a correctness trap. `driven-drive`
//! re-exports every moved item from its historical path, so no downstream `use`
//! had to change.
//!
//! [`RemoteStore`]: remote_store::RemoteStore
//! [`DriveError`]: error::DriveError
//! [`BackendKind`]: backend::BackendKind

#![deny(missing_docs)]

pub mod backend;
pub mod error;
pub mod props;
pub mod remote_store;
pub mod retry;

pub use backend::BackendKind;
pub use error::{classification_of, classify_stream_read_error, DriveError};
pub use props::{
    BUNDLE_FORMAT_KEY, CLIENT_OP_UUID_KEY, FOLDER_MARKER_KEY, RELATIVE_PATH_HASH_KEY, SOURCE_ID_KEY,
};
pub use remote_store::*;
