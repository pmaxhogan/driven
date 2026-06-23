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
