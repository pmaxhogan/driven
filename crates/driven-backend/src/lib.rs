//! `driven-backend` - the backup-destination factory.
//!
//! One function, [`build_store`], turns an account's persisted
//! [`BackendKind`] + backend config into the `Arc<dyn RemoteStore>` the sync
//! engine, the destination picker and the restore path all run against.
//!
//! ## Why a separate crate
//!
//! `driven-core` is deliberately I/O-free: it names the [`RemoteStore`] trait
//! and never constructs one. Before this crate existed, THREE call sites
//! hand-rolled the same `KeyringTokenStore` -> `RefreshingTokenSource` ->
//! `GoogleDriveStore` chain (`assembly::build_remote`, the Drive-folder
//! picker's `build_account_store`, and `driven-cli`'s `build_store`), so adding
//! a destination meant editing every one of them and keeping them in sync. The
//! factory collapses them into one arm-per-backend `match`, and it lives
//! outside `driven-core` so `driven-core` never links a destination's HTTP
//! stack.
//!
//! ## Secrets
//!
//! No credential is ever passed through the app's SQLite database or a config
//! file. Each backend reads its own secret from the OS keychain, keyed by the
//! account id, at the moment it builds a client. Nothing here logs a token, a
//! secret, or a client secret.
//!
//! [`RemoteStore`]: driven_remote::remote_store::RemoteStore

#![deny(missing_docs)]

use std::sync::Arc;

use driven_drive::google::token_store::{
    ClientCredsStore, KeyringTokenStore, RefreshingTokenSource,
};
use driven_drive::google::GoogleDriveStore;
use driven_remote::remote_store::RemoteStore;
use driven_remote::BackendKind;
use driven_tls::{CustomCaConfig, ProxyConfig};

/// Tracing target for the backend factory.
const TARGET: &str = "driven::backend";

/// Env override for the OAuth client id (a TEST injection seam; see
/// [`resolve_account_oauth_creds`]).
pub const ENV_OAUTH_CLIENT_ID: &str = "DRIVEN_OAUTH_CLIENT_ID";

/// Env override for the OAuth client secret (a TEST injection seam; see
/// [`resolve_account_oauth_creds`]).
pub const ENV_OAUTH_CLIENT_SECRET: &str = "DRIVEN_OAUTH_CLIENT_SECRET";

/// The result of asking the factory for an account's store.
pub enum StoreOutcome {
    /// A live store for the account.
    Store(Arc<dyn RemoteStore>),
    /// The account has no usable stored credentials and must be
    /// re-authenticated before anything can talk to its destination.
    ///
    /// C5-P1-1 (silent-data-loss guard): this is NEVER downgraded to an
    /// in-memory fake. Marking files `synced` against ephemeral fake ids and
    /// then losing the bytes on process exit is catastrophic for a backup tool,
    /// so a missing credential is surfaced to the caller, which persists
    /// `needs_reauth` and declines to spawn the orchestrator.
    NeedsReauth,
}

impl StoreOutcome {
    /// The store, or `None` when the account needs re-authentication.
    pub fn store(self) -> Option<Arc<dyn RemoteStore>> {
        match self {
            StoreOutcome::Store(s) => Some(s),
            StoreOutcome::NeedsReauth => None,
        }
    }
}

/// Everything the factory needs that is not backend-specific: the corporate
/// custom-CA and proxy configuration every client it builds must honour
/// (issue #34).
#[derive(Clone, Copy)]
pub struct BackendContext<'a> {
    /// Optional custom root CA (a TLS-inspecting corporate proxy's private CA).
    pub ca: &'a CustomCaConfig,
    /// Proxy configuration (system / none / manual / PAC).
    pub proxy: &'a ProxyConfig,
}

/// One account's destination, as persisted in the `accounts` row.
#[derive(Clone)]
pub struct AccountBackend {
    /// The account id. Doubles as the keychain lookup key for every backend's
    /// secret.
    pub account_id: String,
    /// Which destination this account backs up to.
    pub kind: BackendKind,
    /// The backend's non-secret configuration, as stored in
    /// `accounts.backend_config_json`. `None` for backends that need none
    /// (Google Drive: the account IS the configuration).
    pub config_json: Option<String>,
}

impl AccountBackend {
    /// A Google Drive account (the historical default), which carries no
    /// backend config.
    pub fn google_drive(account_id: impl Into<String>) -> Self {
        Self {
            account_id: account_id.into(),
            kind: BackendKind::GoogleDrive,
            config_json: None,
        }
    }
}

/// A destination the UI can offer, derived from a [`BackendKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendDescriptor {
    /// The kind itself.
    pub kind: BackendKind,
    /// The stable id crossing the IPC boundary (`BackendKind::id`).
    pub id: &'static str,
    /// Whether the setup wizard should run the OAuth consent flow for it.
    pub uses_oauth: bool,
    /// Whether the destination picker can browse a folder tree for it.
    pub supports_folder_picker: bool,
}

/// Every destination this build can construct, in picker order. The first entry
/// is the default selection.
pub fn descriptors() -> Vec<BackendDescriptor> {
    BackendKind::ALL
        .iter()
        .copied()
        .map(|kind| BackendDescriptor {
            kind,
            id: kind.id(),
            uses_oauth: kind.uses_oauth(),
            supports_folder_picker: kind.supports_folder_picker(),
        })
        .collect()
}

/// The id of the destination-tree ROOT to start the folder picker at.
///
/// Drive names its root with the `"root"` alias rather than a real file id.
pub fn picker_root_id(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::GoogleDrive => "root",
    }
}

/// Build the [`RemoteStore`] for one account's configured destination.
///
/// Reads the account's secret from the OS keychain and constructs the backend's
/// client with `ctx`'s CA + proxy applied. Returns
/// [`StoreOutcome::NeedsReauth`] when the account has no stored credential -
/// never a fake, never a partially-configured client.
///
/// [`RemoteStore`]: driven_remote::remote_store::RemoteStore
pub fn build_store(
    account: &AccountBackend,
    ctx: BackendContext<'_>,
) -> anyhow::Result<StoreOutcome> {
    match account.kind {
        BackendKind::GoogleDrive => build_google_drive(&account.account_id, ctx),
    }
}

/// The Google Drive arm: keychain refresh token -> refreshing token source ->
/// `GoogleDriveStore`.
fn build_google_drive(account_id: &str, ctx: BackendContext<'_>) -> anyhow::Result<StoreOutcome> {
    // Wrapped in an `Arc` so a refresh-token ROTATION is persisted back to the
    // keychain (codex C-P2-4 / V-A3).
    let token_store = Arc::new(KeyringTokenStore::new(account_id.to_string()));
    let refresh_token = match token_store.load_refresh_token()? {
        Some(token) => token,
        None => {
            tracing::warn!(
                target: TARGET,
                %account_id,
                "no stored refresh token; account needs reauth (NOT falling back to a fake store)"
            );
            return Ok(StoreOutcome::NeedsReauth);
        }
    };

    // A1: prefer the account's persisted BYO client creds (the client that
    // minted this refresh token); fall back to env only when the account stored
    // none. A refresh token is bound to the client that minted it, so using the
    // wrong client fails with `invalid_client`.
    let (client_id, client_secret) = resolve_account_oauth_creds(account_id);
    let token_source = RefreshingTokenSource::from_stored_refresh_token(
        refresh_token,
        client_id,
        client_secret,
        ctx.ca,
        ctx.proxy,
    )?
    .with_store(token_store);
    let store = GoogleDriveStore::with_default_clients(token_source, ctx.ca, ctx.proxy)?;
    tracing::info!(
        target: TARGET,
        %account_id,
        "built real GoogleDriveStore (keyring refresh token)"
    );
    Ok(StoreOutcome::Store(Arc::new(store)))
}

/// R2-P2-1 (BYO-only): resolve the OAuth client creds from the ENV override only
/// (a TEST injection seam). There is NO baked-in production default client, so
/// this returns whatever the env carries (an empty client id when unset). A
/// production account always reaches [`resolve_account_oauth_creds`] with its
/// PERSISTED BYO creds; this env-only path is the fallback for the e2e seam, and
/// an empty client id surfaces a clear `invalid_client` rather than silently
/// using a Driven-owned client.
fn env_oauth_creds() -> (String, String) {
    (
        std::env::var(ENV_OAUTH_CLIENT_ID).unwrap_or_default(),
        std::env::var(ENV_OAUTH_CLIENT_SECRET).unwrap_or_default(),
    )
}

/// A1: resolve the OAuth client creds for `account_id`, preferring its PERSISTED
/// BYO client creds (loaded from the keychain) over the env seam.
///
/// The refresh token in the keychain was minted by a specific OAuth client; a
/// refresh against a different client fails (`invalid_client`). So an account
/// that brought its own client MUST refresh against that same client across
/// restarts. NEVER logs the secret.
pub fn resolve_account_oauth_creds(account_id: &str) -> (String, String) {
    match ClientCredsStore::new(account_id.to_string()).load() {
        Ok(Some(creds)) if !creds.client_id.trim().is_empty() => {
            (creds.client_id, creds.client_secret)
        }
        Ok(_) => env_oauth_creds(),
        Err(err) => {
            tracing::warn!(
                target: TARGET,
                %account_id,
                %err,
                "failed to load account BYO client creds from keychain; using env client"
            );
            env_oauth_creds()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_cover_every_kind_in_picker_order() {
        let d = descriptors();
        assert_eq!(d.len(), BackendKind::ALL.len());
        for (desc, kind) in d.iter().zip(BackendKind::ALL.iter().copied()) {
            assert_eq!(desc.kind, kind);
            assert_eq!(desc.id, kind.id());
        }
        assert_eq!(d[0].kind, BackendKind::default());
    }

    #[test]
    fn google_drive_accounts_carry_no_backend_config() {
        let a = AccountBackend::google_drive("acct-1");
        assert_eq!(a.kind, BackendKind::GoogleDrive);
        assert!(a.config_json.is_none());
    }

    #[test]
    fn picker_root_is_the_drive_root_alias() {
        assert_eq!(picker_root_id(BackendKind::GoogleDrive), "root");
    }
}
