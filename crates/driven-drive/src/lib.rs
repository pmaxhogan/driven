//! `driven-drive` - the Google Drive backend plus the shared in-memory fake.
//!
//! - `google::GoogleDriveStore` is the production Google Drive backend
//!   (OAuth via PKCE loopback, resumable uploads, refresh-token storage
//!   in the OS keychain).
//! - `fake::InMemoryRemoteStore` is the in-memory backend exercised by
//!   the contract tests and by every sync-engine test in this workspace.
//!
//! M1 phase 2B: the `fake` module is wired up. M4: the `google` module
//! lands the production Google Drive backend (OAuth, resumable uploads,
//! keychain-backed refresh-token storage).
//!
//! ## The trait now lives in `driven-remote`
//!
//! `RemoteStore`, its value types, the SPEC s24 `DriveError` taxonomy, the
//! retry middleware and the `app_properties` key vocabulary moved to the
//! backend-neutral `driven-remote` crate so a second destination can implement
//! the contract without depending on Google's OAuth/keyring stack. Everything
//! is re-exported from its historical path here - `driven_drive::remote_store`
//! and `driven_drive::google::{DriveError, retry, SOURCE_ID_KEY, ..}` all still
//! resolve - so no downstream `use` changed.

pub mod fake;
pub mod google;

/// The `RemoteStore` trait and its value types, re-exported from
/// [`driven_remote`] at their historical `driven_drive::remote_store` path.
pub use driven_remote as remote_store;

// Issue #34: re-export the custom-root-CA config type so callers that already
// depend on `driven-drive` (the CLI, the google_e2e integration test) can name
// it without a separate `driven-tls` dependency. `apply_custom_ca` /
// `validate_ca_file` live in `driven_tls` for the crates that build clients.
pub use driven_tls::CustomCaConfig;
// Issue #34: likewise re-export the proxy config type (SOCKS5 + PAC support) so
// the same callers can name it without a direct `driven-tls` dependency.
pub use driven_tls::ProxyConfig;
