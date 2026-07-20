//! `driven-drive` - the `RemoteStore` trait plus its implementations.
//!
//! - `remote_store` declares the trait every backend must satisfy.
//! - `google::GoogleDriveStore` is the production Google Drive backend
//!   (OAuth via PKCE loopback, resumable uploads, refresh-token storage
//!   in the OS keychain).
//! - `fake::InMemoryRemoteStore` is the in-memory backend exercised by
//!   the contract tests and by every sync-engine test in this workspace.
//!
//! M1 phase 2B: the `fake` module is wired up. M4: the `google` module
//! lands the production Google Drive backend (OAuth, resumable uploads,
//! keychain-backed refresh-token storage).

pub mod fake;
pub mod google;
pub mod remote_store;

// Issue #34: re-export the custom-root-CA config type so callers that already
// depend on `driven-drive` (the CLI, the google_e2e integration test) can name
// it without a separate `driven-tls` dependency. `apply_custom_ca` /
// `validate_ca_file` live in `driven_tls` for the crates that build clients.
pub use driven_tls::CustomCaConfig;
