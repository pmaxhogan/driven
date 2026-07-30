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
use driven_localfs::{LocalFsConfig, LocalFsStore};
use driven_remote::remote_store::RemoteStore;
use driven_remote::BackendKind;
use driven_s3::{S3Config, S3CredentialStore, S3Credentials, S3Store};
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
    /// Whether the destination can really keep previous versions of a changed
    /// file, so a point-in-time restore returns the older bytes rather than
    /// today's (`BackendKind::supports_version_history`; issue #220).
    pub supports_version_history: bool,
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
            supports_version_history: kind.supports_version_history(),
        })
        .collect()
}

/// The id of the destination-tree ROOT to start the folder picker at.
///
/// Drive names its root with the `"root"` alias rather than a real file id.
pub fn picker_root_id(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::GoogleDrive => "root",
        // An S3 "folder" is a key prefix; the destination root is the bucket
        // root (or the configured prefix, which the store applies itself), so
        // the empty prefix is the right starting point.
        BackendKind::S3 => "",
        // A local destination's ids are paths RELATIVE to the configured root,
        // so the root is the empty path. (`supports_folder_picker` is false for
        // it, so nothing browses this today - but the factory must still have an
        // answer, and a wrong one would be worse than none.)
        BackendKind::LocalFolder => "",
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
        BackendKind::S3 => build_s3(account, ctx),
        BackendKind::LocalFolder => build_local_folder(account),
    }
}

/// The S3 arm: persisted non-secret config + keychain access key pair ->
/// `S3Store`.
fn build_s3(account: &AccountBackend, ctx: BackendContext<'_>) -> anyhow::Result<StoreOutcome> {
    let json = account.config_json.as_deref().ok_or_else(|| {
        anyhow::anyhow!("s3.config_invalid: the account has no stored S3 backend configuration")
    })?;
    let config = S3Config::from_json(json)?;
    let Some(creds) = S3CredentialStore::new(account.account_id.clone()).load()? else {
        tracing::warn!(
            target: TARGET,
            account_id = %account.account_id,
            "no stored S3 credentials; account needs reauth (NOT falling back to a fake store)"
        );
        return Ok(StoreOutcome::NeedsReauth);
    };
    let store = S3Store::new(&config, &creds, ctx.ca, ctx.proxy)?;
    tracing::info!(
        target: TARGET,
        account_id = %account.account_id,
        // Endpoint and bucket are NOT secrets and are what an operator needs to
        // debug a misconfigured destination. The key pair is never logged.
        bucket = %config.bucket,
        endpoint = %config.endpoint,
        "built real S3Store (keyring access key)"
    );
    Ok(StoreOutcome::Store(Arc::new(store)))
}

/// Persist an account's S3 destination: validate + normalize the non-secret
/// config into the blob for `accounts.backend_config_json`, and put the access
/// key pair in the OS keychain.
///
/// Returns the config JSON for the caller to store on the account row. The
/// credentials are NOT returned and never reach the caller's storage.
pub fn store_s3_credentials(
    account_id: &str,
    config: S3Config,
    creds: &S3Credentials,
) -> anyhow::Result<String> {
    let config = config.normalized()?;
    S3CredentialStore::new(account_id.to_string()).store(creds)?;
    config.to_json()
}

/// Remove every keychain secret an account's backend owns.
///
/// Called on account removal. Idempotent per backend, and deliberately a
/// `match` on the kind so a new backend cannot be added without deciding what
/// its removal purges - a forgotten arm would leave a live credential in the
/// user's keychain after they deleted the account.
pub fn purge_account_secrets(account: &AccountBackend) -> anyhow::Result<()> {
    match account.kind {
        // The Drive refresh token + BYO client creds are purged by the accounts
        // command layer's own secret seam (which predates this factory and is
        // covered by its tests); nothing extra to do here.
        BackendKind::GoogleDrive => Ok(()),
        BackendKind::S3 => S3CredentialStore::new(account.account_id.clone()).delete(),
        // A folder the user already has write access to needs no credential, so
        // this backend never puts anything in the keychain and there is nothing
        // to purge. The destination MARKER stays on the drive on purpose: it is
        // what lets the folder be re-added later and ADOPT the objects already
        // on it, and it is not Driven's to delete from the user's disk.
        BackendKind::LocalFolder => Ok(()),
    }
}

/// The local-folder arm: the persisted non-secret config is the whole
/// configuration.
///
/// There is no credential and therefore no [`StoreOutcome::NeedsReauth`] path -
/// a folder the user picked is either reachable or it is not, and THAT is
/// decided per operation by the store's destination-marker check, not here. A
/// removable drive is routinely unplugged, and refusing to build the store would
/// leave the account unable to start until the user happened to have the stick
/// plugged in.
fn build_local_folder(account: &AccountBackend) -> anyhow::Result<StoreOutcome> {
    let json = account.config_json.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "localfs.config_invalid: the account has no stored local-folder backend configuration"
        )
    })?;
    let config = LocalFsConfig::from_json(json)?;
    let store = LocalFsStore::new(&config)?;
    tracing::info!(
        target: TARGET,
        account_id = %account.account_id,
        // The folder path is not a secret and is what an operator needs to debug
        // a destination that stopped being reachable.
        root = %config.root,
        "built real LocalFsStore"
    );
    Ok(StoreOutcome::Store(Arc::new(store)))
}

/// Prepare a directory to be a local-folder destination and render the blob for
/// `accounts.backend_config_json`.
///
/// Validates that the path is a writable directory (by actually writing), then
/// stamps or ADOPTS its destination-identity marker - adopting so that
/// re-adding a drive which already holds a Driven backup keeps every object on
/// it. Returns the config JSON for the caller to store on the account row.
pub fn prepare_local_folder(root: &std::path::Path, now_ms: i64) -> anyhow::Result<String> {
    let (destination_id, outcome) = driven_localfs::prepare_destination(root, now_ms)?;
    tracing::info!(
        target: TARGET,
        root = %root.display(),
        ?outcome,
        "prepared a local-folder destination"
    );
    LocalFsConfig {
        root: root.to_string_lossy().into_owned(),
        destination_id,
    }
    .normalized()?
    .to_json()
}

/// The Google Drive arm: keychain refresh token -> refreshing token source ->
/// `GoogleDriveStore`.
///
/// This half performs the keychain READS; the decision it feeds them into lives
/// in [`google_drive_outcome`] so that decision is unit-testable without an OS
/// keychain (headless CI has none, and the driven-drive token-store module
/// documents the same constraint).
fn build_google_drive(account_id: &str, ctx: BackendContext<'_>) -> anyhow::Result<StoreOutcome> {
    // Wrapped in an `Arc` so a refresh-token ROTATION is persisted back to the
    // keychain (codex C-P2-4 / V-A3).
    let token_store = Arc::new(KeyringTokenStore::new(account_id.to_string()));
    let refresh_token = token_store.load_refresh_token()?;
    // A1: prefer the account's persisted BYO client creds (the client that
    // minted this refresh token); fall back to env only when the account stored
    // none. A refresh token is bound to the client that minted it, so using the
    // wrong client fails with `invalid_client`.
    let (client_id, client_secret) = resolve_account_oauth_creds(account_id);
    google_drive_outcome(
        account_id,
        token_store,
        refresh_token,
        client_id,
        client_secret,
        ctx,
    )
}

/// The DECISION half of the Drive arm: turn an already-resolved credential set
/// into a [`StoreOutcome`].
///
/// Split from the keychain reads in [`build_google_drive`] so the C5-P1-1
/// invariant - a missing refresh token yields
/// [`StoreOutcome::NeedsReauth`] and NEVER a fake store - is covered by a real
/// unit test. Assembling the store itself touches neither the keychain nor the
/// network: it only builds `reqwest` clients, so a test can exercise the whole
/// success path offline.
fn google_drive_outcome(
    account_id: &str,
    token_store: Arc<KeyringTokenStore>,
    refresh_token: Option<String>,
    client_id: String,
    client_secret: String,
    ctx: BackendContext<'_>,
) -> anyhow::Result<StoreOutcome> {
    let Some(refresh_token) = refresh_token else {
        tracing::warn!(
            target: TARGET,
            %account_id,
            "no stored refresh token; account needs reauth (NOT falling back to a fake store)"
        );
        return Ok(StoreOutcome::NeedsReauth);
    };
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

/// A1: the rule for choosing between an account's PERSISTED BYO client creds and
/// the env seam.
///
/// Extracted from [`resolve_account_oauth_creds`] (which supplies `stored` from
/// the keychain) so the rule itself is testable in-process. A stored record only
/// wins when its client id is non-blank: a half-written record with an empty id
/// would authenticate as no one, and falling through to the env seam surfaces a
/// clear `invalid_client` instead.
fn choose_oauth_creds(stored: Option<(String, String)>, env: (String, String)) -> (String, String) {
    match stored {
        Some((id, secret)) if !id.trim().is_empty() => (id, secret),
        _ => env,
    }
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
        Ok(stored) => choose_oauth_creds(
            stored.map(|c| (c.client_id, c.client_secret)),
            env_oauth_creds(),
        ),
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
    use std::sync::{Mutex, OnceLock};

    /// Install `keyring-core`'s IN-MEMORY store as the process default, so the
    /// keychain-backed paths run for real without touching (or requiring) an OS
    /// keychain.
    ///
    /// Returns `None` when the mock could not be made the EFFECTIVE store, in
    /// which case the caller MUST skip - the alternative is writing test
    /// secrets into the developer's real login keychain, which on macOS also
    /// blocks the run on a modal permission prompt.
    ///
    /// ## Ordering is load-bearing
    ///
    /// `keyring` 4.x's `Entry::new` installs the PLATFORM-NATIVE store on its
    /// FIRST call, overwriting whatever default is already set (see
    /// `keyring-4.1.5/src/v1.rs`, the `SET_CREDENTIAL_STORE` latch). Installing
    /// the mock first is therefore silently undone by the first real `Entry`,
    /// and every "mock" write lands in the OS keychain instead. So this burns
    /// that latch first with a throwaway `Entry` (constructing one performs no
    /// credential I/O), THEN installs the mock, and finally PROVES the mock is
    /// in effect with a sentinel round trip before any test stores a secret.
    fn keychain() -> Option<std::sync::MutexGuard<'static, ()>> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        static ACTIVE: OnceLock<bool> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let active = *ACTIVE.get_or_init(|| {
            // 1. Burn keyring's one-shot platform-store latch. Its error on a
            //    headless box (no secret service) is expected and ignored.
            let _ = keyring::Entry::new("driven.test.keyring-latch", "latch");
            // 2. Now the mock sticks.
            match keyring_core::mock::Store::new() {
                Ok(store) => keyring_core::set_default_store(store),
                Err(_) => return false,
            }
            // 3. Prove it, through the same `keyring` facade production uses.
            let sentinel = match keyring::Entry::new("driven.test.sentinel", "probe") {
                Ok(e) => e,
                Err(_) => return false,
            };
            if sentinel.set_password("mock-is-active").is_err() {
                return false;
            }
            let ok = sentinel.get_password().ok().as_deref() == Some("mock-is-active");
            let _ = sentinel.delete_credential();
            ok
        });
        if !active {
            eprintln!(
                "skipping the keychain test: the in-memory keyring store is not the effective \
                 default, and this test will not write to a real OS keychain"
            );
            return None;
        }
        Some(guard)
    }

    /// Env vars are process-global too; `env_oauth_creds` reads them, so the
    /// tests that manipulate them share the keychain lock rather than racing.
    fn set_env_client(id: Option<&str>, secret: Option<&str>) {
        match id {
            Some(v) => std::env::set_var(ENV_OAUTH_CLIENT_ID, v),
            None => std::env::remove_var(ENV_OAUTH_CLIENT_ID),
        }
        match secret {
            Some(v) => std::env::set_var(ENV_OAUTH_CLIENT_SECRET, v),
            None => std::env::remove_var(ENV_OAUTH_CLIENT_SECRET),
        }
    }

    #[test]
    fn descriptors_cover_every_kind_in_picker_order() {
        let d = descriptors();
        assert_eq!(d.len(), BackendKind::ALL.len());
        for (desc, kind) in d.iter().zip(BackendKind::ALL.iter().copied()) {
            assert_eq!(desc.kind, kind);
            assert_eq!(desc.id, kind.id());
            // Every capability flag must be COPIED from the kind, never
            // defaulted: a descriptor that under-reports would hide a control,
            // and one that over-reports would offer a promise the destination
            // cannot keep (issue #220).
            assert_eq!(desc.uses_oauth, kind.uses_oauth());
            assert_eq!(desc.supports_folder_picker, kind.supports_folder_picker());
            assert_eq!(
                desc.supports_version_history,
                kind.supports_version_history()
            );
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
        assert_eq!(picker_root_id(BackendKind::S3), "");
        assert_eq!(picker_root_id(BackendKind::LocalFolder), "");
    }

    #[test]
    fn an_s3_account_without_config_is_an_error_not_a_needs_reauth() {
        // A missing CONFIG is a bug (the account row was written wrong); a
        // missing CREDENTIAL is a reauth. Conflating them would send the user
        // round the credential prompt forever on a malformed row.
        let account = AccountBackend {
            account_id: "acct-s3".to_string(),
            kind: BackendKind::S3,
            config_json: None,
        };
        let ca = CustomCaConfig::none();
        let proxy = ProxyConfig::system();
        // `StoreOutcome` holds a `dyn RemoteStore`, which has no `Debug`, so
        // unwrap the Result by hand rather than via `expect_err`.
        let err = match build_store(
            &account,
            BackendContext {
                ca: &ca,
                proxy: &proxy,
            },
        ) {
            Ok(_) => panic!("a missing S3 config must be an error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("s3.config_invalid"), "{err}");
    }

    #[test]
    fn an_invalid_s3_config_blob_is_rejected_before_any_keychain_read() {
        let account = AccountBackend {
            account_id: "acct-s3".to_string(),
            kind: BackendKind::S3,
            config_json: Some(r#"{"endpoint":"not a url","bucket":"b"}"#.to_string()),
        };
        let ca = CustomCaConfig::none();
        let proxy = ProxyConfig::system();
        assert!(build_store(
            &account,
            BackendContext {
                ca: &ca,
                proxy: &proxy,
            }
        )
        .is_err());
    }

    #[test]
    fn a_persisted_s3_config_blob_carries_no_credential_material() {
        // The blob is what lands in SQLite and the diagnostic bundle.
        let config = S3Config {
            endpoint: "https://example.com/".to_string(),
            bucket: "bkt".to_string(),
            region: String::new(),
            path_style: true,
            prefix: Some("/backups/".to_string()),
        };
        let blob = config.normalized().expect("valid").to_json().expect("json");
        assert!(!blob.to_lowercase().contains("secret"));
        assert!(!blob.contains("AKIA"));
        assert!(blob.contains("backups/"));
    }

    #[test]
    fn store_s3_credentials_validates_before_touching_the_keychain() {
        // A bad config must not leave a stored credential behind for an account
        // that could never work.
        let creds = S3Credentials {
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: "super-secret".to_string(),
        };
        let bad = S3Config {
            endpoint: String::new(),
            bucket: "bkt".to_string(),
            region: String::new(),
            path_style: true,
            prefix: None,
        };
        assert!(store_s3_credentials("acct-never-created", bad, &creds).is_err());
    }

    #[test]
    fn a_local_folder_account_without_config_is_an_error_not_a_needs_reauth() {
        // A missing CONFIG is a bug (the account row was written wrong). There
        // is no credential for this backend, so it can never be a reauth, and
        // conflating the two would send the user round a prompt that cannot fix
        // anything.
        let account = AccountBackend {
            account_id: "acct-local".to_string(),
            kind: BackendKind::LocalFolder,
            config_json: None,
        };
        let ca = CustomCaConfig::none();
        let proxy = ProxyConfig::system();
        // `StoreOutcome` holds a `dyn RemoteStore`, which has no `Debug`, so
        // unwrap the Result by hand rather than via `expect_err`.
        let err = match build_store(
            &account,
            BackendContext {
                ca: &ca,
                proxy: &proxy,
            },
        ) {
            Ok(_) => panic!("a missing local-folder config must be an error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("localfs.config_invalid"), "{err}");
    }

    #[test]
    fn a_relative_local_folder_config_is_rejected() {
        // A relative destination would resolve against whatever the app's
        // working directory happened to be, which is not a property a backup
        // destination may have.
        let account = AccountBackend {
            account_id: "acct-local".to_string(),
            kind: BackendKind::LocalFolder,
            config_json: Some(r#"{"root":"backups","destinationId":"d"}"#.to_string()),
        };
        let ca = CustomCaConfig::none();
        let proxy = ProxyConfig::system();
        assert!(build_store(
            &account,
            BackendContext {
                ca: &ca,
                proxy: &proxy,
            }
        )
        .is_err());
    }

    #[test]
    fn preparing_a_local_folder_stamps_a_marker_and_yields_a_credential_free_blob() {
        let dir = tempfile::tempdir().unwrap();
        let blob = prepare_local_folder(dir.path(), 1_700_000_000_000).expect("prepare");
        assert!(blob.contains("destinationId"), "{blob}");
        assert!(dir.path().join(".driven-destination.json").exists());
        // The blob lands in SQLite and the diagnostic bundle; it must carry no
        // credential material (this backend has none, and it must stay that
        // way).
        for forbidden in ["secret", "accesskey", "password", "token"] {
            assert!(!blob.to_lowercase().contains(forbidden), "{blob}");
        }

        // Re-preparing the same folder ADOPTS its identity rather than
        // re-stamping it - a new id would orphan every object already there.
        let again = prepare_local_folder(dir.path(), 1_700_000_001_000).expect("prepare again");
        assert_eq!(again, blob);
    }

    #[test]
    fn a_prepared_local_folder_builds_a_real_store() {
        let dir = tempfile::tempdir().unwrap();
        let config_json = prepare_local_folder(dir.path(), 0).expect("prepare");
        let account = AccountBackend {
            account_id: "acct-local".to_string(),
            kind: BackendKind::LocalFolder,
            config_json: Some(config_json),
        };
        let ca = CustomCaConfig::none();
        let proxy = ProxyConfig::system();
        let outcome = build_store(
            &account,
            BackendContext {
                ca: &ca,
                proxy: &proxy,
            },
        )
        .expect("build");
        assert!(
            outcome.store().is_some(),
            "a local folder has no credential, so it can never need reauth"
        );
    }

    #[test]
    fn removing_a_local_folder_account_purges_nothing_from_the_keychain() {
        // This backend stores no secret at all, and the destination MARKER on
        // the drive is deliberately left alone: it is what lets the folder be
        // re-added later and ADOPT the objects already on it.
        let account = AccountBackend {
            account_id: "acct-local".to_string(),
            kind: BackendKind::LocalFolder,
            config_json: None,
        };
        purge_account_secrets(&account).expect("purging a credential-free backend is a no-op");
    }

    fn ctx<'a>(ca: &'a CustomCaConfig, proxy: &'a ProxyConfig) -> BackendContext<'a> {
        BackendContext { ca, proxy }
    }

    #[test]
    fn a_missing_refresh_token_yields_needs_reauth_never_a_store() {
        // C5-P1-1, the single most important rule in this crate: an account with
        // no credential must NOT get some degraded store. Marking files `synced`
        // against a store that cannot really hold them is silent data loss.
        let ca = CustomCaConfig::none();
        let proxy = ProxyConfig::system();
        let outcome = google_drive_outcome(
            "acct-1",
            Arc::new(KeyringTokenStore::new("acct-1".to_string())),
            None,
            "client-id".to_string(),
            "client-secret".to_string(),
            ctx(&ca, &proxy),
        )
        .expect("a missing token is not an error, it is a reauth");
        assert!(matches!(outcome, StoreOutcome::NeedsReauth));
        assert!(
            outcome.store().is_none(),
            "NeedsReauth must not yield a store"
        );
    }

    #[test]
    fn a_stored_refresh_token_yields_a_live_store() {
        let ca = CustomCaConfig::none();
        let proxy = ProxyConfig::system();
        let outcome = google_drive_outcome(
            "acct-1",
            Arc::new(KeyringTokenStore::new("acct-1".to_string())),
            Some("refresh-token".to_string()),
            "client-id".to_string(),
            "client-secret".to_string(),
            ctx(&ca, &proxy),
        )
        .expect("building the store must not need the network");
        assert!(matches!(outcome, StoreOutcome::Store(_)));
        assert!(outcome.store().is_some());
    }

    #[test]
    fn a_broken_custom_ca_fails_closed_rather_than_building_a_store() {
        // Issue #34 invariant: a configured-but-unreadable CA must NOT silently
        // downgrade to the system trust store. Backup traffic would then bypass
        // the trust boundary the user deliberately set.
        let ca = CustomCaConfig::from_path(Some(std::path::PathBuf::from(
            "/definitely/not/a/real/ca.pem",
        )));
        let proxy = ProxyConfig::system();
        let err = google_drive_outcome(
            "acct-1",
            Arc::new(KeyringTokenStore::new("acct-1".to_string())),
            Some("refresh-token".to_string()),
            "client-id".to_string(),
            "client-secret".to_string(),
            ctx(&ca, &proxy),
        );
        assert!(err.is_err(), "an unusable custom CA must fail the build");
    }

    #[test]
    fn stored_byo_creds_win_over_the_env_seam_unless_the_id_is_blank() {
        let env = ("env-id".to_string(), "env-secret".to_string());

        // A real stored record wins: the refresh token is bound to the client
        // that minted it, so using the env client would fail every refresh.
        assert_eq!(
            choose_oauth_creds(
                Some(("byo-id".to_string(), "byo-secret".to_string())),
                env.clone()
            ),
            ("byo-id".to_string(), "byo-secret".to_string())
        );

        // No stored record at all: the env seam.
        assert_eq!(choose_oauth_creds(None, env.clone()), env.clone());

        // A half-written record with a blank id would authenticate as no one;
        // falling through to the env seam surfaces a clear `invalid_client`
        // instead of a confusing empty-credential failure.
        for blank in ["", "   "] {
            assert_eq!(
                choose_oauth_creds(
                    Some((blank.to_string(), "orphan-secret".to_string())),
                    env.clone()
                ),
                env.clone(),
                "a blank stored client id must not win"
            );
        }
    }

    #[test]
    fn store_outcome_unwraps_to_the_store_it_holds() {
        assert!(StoreOutcome::NeedsReauth.store().is_none());
    }

    #[test]
    fn build_store_needs_reauth_for_a_drive_account_with_an_empty_keychain() {
        // The end-to-end factory path over a real (in-memory) keychain: an
        // account whose refresh token was never stored - or was revoked and
        // deleted - must come back as NeedsReauth.
        let Some(_g) = keychain() else { return };
        let ca = CustomCaConfig::none();
        let proxy = ProxyConfig::system();
        let account = AccountBackend::google_drive("acct-no-token");
        let outcome =
            build_store(&account, ctx(&ca, &proxy)).expect("an empty keychain is not an error");
        assert!(matches!(outcome, StoreOutcome::NeedsReauth));
    }

    #[test]
    fn build_store_returns_a_drive_store_once_a_refresh_token_is_stored() {
        let Some(_g) = keychain() else { return };
        let account_id = "acct-with-token";
        driven_drive::google::token_store::KeyringTokenStore::new(account_id.to_string())
            .store_refresh_token("a-refresh-token")
            .expect("store the token in the in-memory keychain");

        let ca = CustomCaConfig::none();
        let proxy = ProxyConfig::system();
        let account = AccountBackend::google_drive(account_id);
        let outcome = build_store(&account, ctx(&ca, &proxy)).expect("build the store");
        assert!(matches!(outcome, StoreOutcome::Store(_)));
    }

    #[test]
    fn account_creds_prefer_the_keychain_record_and_fall_back_to_the_env_seam() {
        use driven_drive::google::token_store::{ClientCreds, ClientCredsStore};
        let Some(_g) = keychain() else { return };
        set_env_client(Some("env-id"), Some("env-secret"));

        // No stored record: the env seam supplies the client.
        assert_eq!(
            resolve_account_oauth_creds("acct-env-only"),
            ("env-id".to_string(), "env-secret".to_string())
        );

        // A stored BYO record wins - the refresh token is bound to the client
        // that minted it, so the env client would fail every refresh.
        let account_id = "acct-byo";
        ClientCredsStore::new(account_id.to_string())
            .store(&ClientCreds {
                client_id: "byo-id".to_string(),
                client_secret: "byo-secret".to_string(),
            })
            .expect("store the BYO creds");
        assert_eq!(
            resolve_account_oauth_creds(account_id),
            ("byo-id".to_string(), "byo-secret".to_string())
        );

        // With the env seam UNSET and no stored record, the resolution is empty
        // rather than some baked-in Driven-owned client: Driven is BYO-only, and
        // an empty client id surfaces a clear `invalid_client`.
        set_env_client(None, None);
        assert_eq!(
            resolve_account_oauth_creds("acct-nothing"),
            (String::new(), String::new())
        );
        // The stored record still wins with the env cleared.
        assert_eq!(
            resolve_account_oauth_creds(account_id),
            ("byo-id".to_string(), "byo-secret".to_string())
        );
    }
}
