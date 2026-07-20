//! Keychain-backed refresh-token storage + the [`RefreshingTokenSource`]
//! (SPEC s4.1).
//!
//! Refresh tokens persist in the OS keychain ONLY (SPEC s4.1); the access
//! token lives in memory and is regenerated on demand. [`KeyringTokenStore`]
//! is the keychain wrapper; [`RefreshingTokenSource`] holds the in-memory
//! [`Tokens`] plus the OAuth client and refreshes when the access token is
//! within 60s of expiry. On an `invalid_grant` it returns a classified
//! [`super::DriveError`] carrying
//! [`crate::remote_store::DriveErrorClassification::AuthInvalidGrant`] (SPEC
//! s24 `auth.invalid_grant`) - this SURFACES the condition for the M5 prod
//! shell to act on (move the account to `needs_reauth` / emit
//! `account:needs_reauth`); no production binary assembles the
//! orchestrator+executor+store yet, so nothing performs that account-state
//! transition today (codex V-F; tracked in design/CODEX_NOTES.md).
//!
//! ## Testability note (keyring 4.1.2 / keyring-core 1.0.0)
//!
//! The workspace resolves `keyring = "4"` to 4.1.2, which is the
//! `keyring-core` 1.0.0 re-architecture. The brief's
//! `set_default_credential_builder(mock)` recipe is the keyring 2.x/3.x API
//! and does NOT exist in 4.1.2; the 4.1.2 mock store lives in
//! `keyring_core::mock`, which is NOT a declared dependency of `driven-drive`
//! (and `keyring` does not re-export it), and Cargo.toml is frozen for this
//! phase. So the keychain ROUND-TRIP itself (keyring's own tested job) is not
//! re-tested here; instead the keyring-result -> domain mapping that IS
//! Driven's responsibility is extracted into pure free fns
//! ([`map_load_result`], [`map_delete_result`]) and unit-tested in-process
//! with constructed `keyring::Error` values - a real, non-skipped test that
//! needs no OS keychain (and would otherwise be flaky on headless CI where no
//! default store initialises). Production uses `keyring::Entry` exactly like
//! `driven-crypto::keystore`.

use std::sync::Arc;
use std::time::Duration;

use driven_tls::{CustomCaConfig, ProxyConfig};
use keyring::Entry;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::warn;

use super::oauth::Tokens;

/// Tracing target for the token store / refresh path.
const TARGET: &str = "driven::drive::token";

/// keyring "service" namespace for Driven Google refresh tokens (SPEC s4.1).
const KEYRING_SERVICE: &str = "driven.google.refresh_token";

/// keyring "service" namespace for Driven per-account BYO OAuth client
/// credentials (A1 / DESIGN s6.1). A refresh token is bound to the OAuth client
/// that minted it, so the account's `client_id` + `client_secret` MUST persist
/// alongside the refresh token - otherwise a restart falls back to the
/// env/default client and every BYO-client refresh fails (`invalid_client`).
const KEYRING_CLIENT_CREDS_SERVICE: &str = "driven.google.client_creds";

/// Google's OAuth token endpoint (SPEC s4.1 refresh path).
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Refresh when the access token is within this many seconds of expiry
/// (SPEC s4.1 "refreshes when `expires_at - now < 60s`").
const REFRESH_SKEW_SECS: i64 = 60;

/// Connect timeout for the OAuth refresh client (DESIGN s5.8.4). The refresh
/// holds the token mutex across the await, so a black-holed token endpoint
/// would otherwise wedge EVERY Drive request indefinitely (codex V-A1).
const REFRESH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Total-request timeout for the OAuth refresh client (DESIGN s5.8.4). Bounds
/// the mutex-hold so a hung token endpoint cannot stall Drive traffic past
/// this window (codex V-A1).
const REFRESH_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Keychain wrapper for an account's Google refresh token (SPEC s4.1).
///
/// One entry per account; the account id is the keychain "user" within the
/// [`KEYRING_SERVICE`] namespace. Tests exercise the result-mapping free fns
/// rather than a real keychain (see the module docs).
pub struct KeyringTokenStore {
    account: String,
}

impl KeyringTokenStore {
    /// Builds a token store for `account` (the keychain lookup key within the
    /// Driven refresh-token namespace).
    pub fn new(account: impl Into<String>) -> Self {
        let _ = KEYRING_SERVICE;
        Self {
            account: account.into(),
        }
    }

    /// The account id this store is scoped to.
    pub fn account(&self) -> &str {
        &self.account
    }

    /// Opens the keychain entry for this account.
    fn entry(&self) -> anyhow::Result<Entry> {
        Entry::new(KEYRING_SERVICE, &self.account)
            .map_err(|e| anyhow::anyhow!("keychain: failed to open entry: {e}"))
    }

    /// Persists `refresh_token` in the keychain for this account (SPEC s4.1).
    pub fn store_refresh_token(&self, refresh_token: &str) -> anyhow::Result<()> {
        self.entry()?
            .set_password(refresh_token)
            .map_err(|e| anyhow::anyhow!("keychain: failed to store refresh token: {e}"))
    }

    /// Loads the stored refresh token, or `None` if the account has never
    /// authenticated (SPEC s4.1). A `NoEntry` is mapped to `Ok(None)`.
    pub fn load_refresh_token(&self) -> anyhow::Result<Option<String>> {
        map_load_result(self.entry()?.get_password())
    }

    /// Deletes the stored refresh token (e.g. on sign-out / revoke). Absent
    /// entry is a no-op (SPEC s4.1).
    pub fn delete_refresh_token(&self) -> anyhow::Result<()> {
        map_delete_result(self.entry()?.delete_credential())
    }
}

/// A persisted per-account BYO OAuth client credential pair (A1).
///
/// `client_secret` is empty for a PKCE installed-app client (no real secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCreds {
    /// The OAuth client id that minted the account's refresh token.
    pub client_id: String,
    /// The OAuth client secret (empty for a PKCE installed-app client).
    pub client_secret: String,
}

/// Keychain wrapper for an account's BYO OAuth client credentials (A1 / DESIGN
/// s6.1). One entry per account, in the [`KEYRING_CLIENT_CREDS_SERVICE`]
/// namespace, keyed by account id. The two fields are stored as a single
/// newline-separated record (`client_id\nclient_secret`) so one keychain entry
/// holds the pair. The secret is NEVER logged.
pub struct ClientCredsStore {
    account: String,
}

impl ClientCredsStore {
    /// Builds a client-creds store scoped to `account` (the keychain lookup key
    /// within the Driven client-creds namespace).
    pub fn new(account: impl Into<String>) -> Self {
        Self {
            account: account.into(),
        }
    }

    /// Opens the keychain entry for this account's client creds.
    fn entry(&self) -> anyhow::Result<Entry> {
        Entry::new(KEYRING_CLIENT_CREDS_SERVICE, &self.account)
            .map_err(|e| anyhow::anyhow!("keychain: failed to open client-creds entry: {e}"))
    }

    /// Persists `creds` for this account (A1). Stored as
    /// `client_id\nclient_secret` in one keychain entry.
    pub fn store(&self, creds: &ClientCreds) -> anyhow::Result<()> {
        let record = encode_client_creds(creds);
        self.entry()?
            .set_password(&record)
            .map_err(|e| anyhow::anyhow!("keychain: failed to store client creds: {e}"))
    }

    /// Loads the stored client creds, or `None` if the account never persisted
    /// any (a default/env-client account). A `NoEntry` maps to `Ok(None)`.
    pub fn load(&self) -> anyhow::Result<Option<ClientCreds>> {
        match self.entry()?.get_password() {
            Ok(record) => Ok(Some(decode_client_creds(&record))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow::anyhow!(
                "keychain: failed to load client creds: {e}"
            )),
        }
    }

    /// Deletes the stored client creds (e.g. on account removal). Absent entry
    /// is a no-op.
    pub fn delete(&self) -> anyhow::Result<()> {
        map_delete_result(self.entry()?.delete_credential())
    }
}

/// Encode a [`ClientCreds`] pair as the single keychain record
/// `client_id\nclient_secret`. Pure so it is unit-testable without a keychain.
fn encode_client_creds(creds: &ClientCreds) -> String {
    format!("{}\n{}", creds.client_id, creds.client_secret)
}

/// Decode a keychain record (`client_id\nclient_secret`) into [`ClientCreds`].
/// A record with no newline is treated as a bare client id (empty secret). Pure
/// so it is unit-testable without a keychain.
fn decode_client_creds(record: &str) -> ClientCreds {
    match record.split_once('\n') {
        Some((id, secret)) => ClientCreds {
            client_id: id.to_string(),
            client_secret: secret.to_string(),
        },
        None => ClientCreds {
            client_id: record.to_string(),
            client_secret: String::new(),
        },
    }
}

/// Maps a `keyring` `get_password` result to the load-token domain result:
/// `Ok(pw) -> Ok(Some(pw))`, `Err(NoEntry) -> Ok(None)`, other `Err -> Err`.
/// Pure so it is unit-testable without a keychain.
fn map_load_result(result: keyring::Result<String>) -> anyhow::Result<Option<String>> {
    match result {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!(
            "keychain: failed to load refresh token: {e}"
        )),
    }
}

/// Maps a `keyring` `delete_credential` result to the delete domain result:
/// `Ok(()) | Err(NoEntry) -> Ok(())` (idempotent), other `Err -> Err`. Pure so
/// it is unit-testable without a keychain.
fn map_delete_result(result: keyring::Result<()>) -> anyhow::Result<()> {
    match result {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "keychain: failed to delete refresh token: {e}"
        )),
    }
}

/// Google's token-refresh response shape (the subset Driven reads).
#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    /// Seconds until the new access token expires.
    #[serde(default)]
    expires_in: Option<i64>,
    /// Google usually does NOT return a new refresh token on refresh; when it
    /// does (rotation), we adopt it.
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Google's OAuth error response shape (the `error` discriminator).
#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// A thin wrapper around [`Tokens`] + the OAuth client that refreshes the
/// access token on demand (SPEC s4.1).
///
/// Refreshes when `expires_at - now < 60s` by POSTing
/// `grant_type=refresh_token` to the Google token endpoint. An
/// `invalid_grant` response surfaces as a [`super::DriveError::Classified`]
/// carrying [`crate::remote_store::DriveErrorClassification::AuthInvalidGrant`]
/// (SPEC s24 `auth.invalid_grant`); the M5 prod shell acts on it (move the
/// account to `needs_reauth`). The current [`Tokens`] are held behind an async
/// [`Mutex`] so concurrent Drive requests share one refresh.
///
/// If a [`KeyringTokenStore`] is wired (via [`Self::with_store`]) and Google
/// ROTATES the refresh token on a refresh, the new token is persisted to the
/// keychain BEFORE the in-memory swap so a restart reloads the live token, not
/// the stale one (codex C-P2-4 / V-A3). When no store is wired (the CLI debug
/// path), the rotated token is adopted in memory only - acceptable because
/// Google does not rotate installed-app refresh tokens in practice.
#[derive(Clone)]
pub struct RefreshingTokenSource {
    inner: Arc<Mutex<Tokens>>,
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    /// Optional keychain store for persisting a rotated refresh token
    /// (C-P2-4 / V-A3). `None` on the no-store contract / CLI debug path.
    store: Option<Arc<KeyringTokenStore>>,
}

impl RefreshingTokenSource {
    /// Builds a [`RefreshingTokenSource`] from the initial [`Tokens`], the
    /// authorized HTTP client, and the OAuth client credentials (SPEC s4.1).
    /// No keychain store is wired (rotated tokens are adopted in memory only);
    /// use [`Self::with_store`] to also persist rotations.
    pub fn new(
        tokens: Tokens,
        http: reqwest::Client,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(tokens)),
            http,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            store: None,
        }
    }

    /// Wires a [`KeyringTokenStore`] so a rotated refresh token is persisted to
    /// the keychain before the in-memory swap (codex C-P2-4 / V-A3). Returns
    /// `self` for chaining off a constructor.
    pub fn with_store(mut self, store: Arc<KeyringTokenStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Builds a [`RefreshingTokenSource`] from a stored refresh token, with an
    /// internally-constructed OAuth refresh HTTP client (so callers - e.g.
    /// `driven-cli` - need no `reqwest` dependency). The initial access token
    /// is empty and already-expired, so the first [`Self::access_token`] call
    /// performs the refresh.
    ///
    /// The refresh client is built with DESIGN s5.8.4 timeouts + a
    /// redirect::none policy (codex V-A1): the refresh holds the token mutex
    /// across its await, so a black-holed / hung token endpoint must be bounded
    /// (otherwise it wedges every Drive request), and the credential-bearing
    /// client must not follow redirects (SPEC s4 SSRF defence).
    pub fn from_stored_refresh_token(
        refresh_token: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        ca: &CustomCaConfig,
        proxy: &ProxyConfig,
    ) -> anyhow::Result<Self> {
        let http = build_refresh_client(ca, proxy)?;
        let tokens = Tokens {
            access_token: String::new(),
            refresh_token: refresh_token.into(),
            expires_at: 0,
        };
        Ok(Self::new(tokens, http, client_id, client_secret))
    }

    /// Returns a currently-valid access token, refreshing first if the cached
    /// one expires within 60s (SPEC s4.1). Holds the async mutex across the
    /// refresh so concurrent callers share one network round-trip.
    pub async fn access_token(&self) -> anyhow::Result<String> {
        let mut guard = self.inner.lock().await;
        if is_expiring(guard.expires_at, now_unix()) {
            let fresh = self.refresh_locked(&guard).await?;
            self.persist_rotation_if_changed(&guard, &fresh);
            *guard = fresh;
        }
        Ok(guard.access_token.clone())
    }

    /// Forces a refresh of the access token from the stored refresh token
    /// (SPEC s4.1). `invalid_grant` surfaces as `auth.invalid_grant`.
    pub async fn force_refresh(&self) -> anyhow::Result<()> {
        let mut guard = self.inner.lock().await;
        let fresh = self.refresh_locked(&guard).await?;
        self.persist_rotation_if_changed(&guard, &fresh);
        *guard = fresh;
        Ok(())
    }

    /// Persists a ROTATED refresh token to the keychain before the in-memory
    /// swap (codex C-P2-4 / V-A3). A no-op when no store is wired or when
    /// Google did not rotate the token (the common case). A keychain write
    /// failure is LOGGED, not silently dropped, and does not fail the refresh
    /// (the new token still works in memory for this run; the risk is only a
    /// stale token after a restart, which the log surfaces).
    fn persist_rotation_if_changed(&self, current: &Tokens, fresh: &Tokens) {
        if fresh.refresh_token == current.refresh_token {
            return;
        }
        let Some(store) = &self.store else {
            warn!(
                target: TARGET,
                "Google rotated the refresh token but no keychain store is wired; \
                 the rotation is in-memory only and will be lost on restart"
            );
            return;
        };
        match store.store_refresh_token(&fresh.refresh_token) {
            Ok(()) => {
                tracing::info!(
                    target: TARGET,
                    "persisted rotated Google refresh token to keychain"
                );
            }
            Err(e) => {
                warn!(
                    target: TARGET,
                    error = %e,
                    "failed to persist rotated refresh token to keychain; \
                     the rotation is in-memory only and may be lost on restart"
                );
            }
        }
    }

    /// Performs the `grant_type=refresh_token` POST and returns the new
    /// [`Tokens`] (preserving the refresh token unless Google rotated it).
    /// Caller holds the inner mutex.
    async fn refresh_locked(&self, current: &Tokens) -> anyhow::Result<Tokens> {
        let params = [
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("refresh_token", current.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ];
        let resp = self
            .http
            .post(GOOGLE_TOKEN_URL)
            .form(&params)
            .send()
            .await
            .map_err(super::DriveError::from_transport)?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(super::DriveError::from_transport)?;
        if !(200..300).contains(&status) {
            // Distinguish invalid_grant (the account must reauth) from a
            // transient/other failure so the executor reacts correctly.
            let err: Option<OAuthErrorResponse> = serde_json::from_slice(&body).ok();
            let error_code = err.as_ref().and_then(|e| e.error.as_deref()).unwrap_or("");
            if error_code == "invalid_grant" {
                warn!(
                    target: TARGET,
                    "refresh token rejected (invalid_grant); account needs reauth"
                );
                return Err(anyhow::Error::new(super::DriveError::Classified {
                    kind: crate::remote_store::DriveErrorClassification::AuthInvalidGrant,
                    source: anyhow::anyhow!(
                        "auth.invalid_grant: {}",
                        err.and_then(|e| e.error_description).unwrap_or_default()
                    ),
                }));
            }
            return Err(anyhow::Error::new(super::DriveError::from_response(
                status, &body, None,
            )));
        }

        let parsed: RefreshResponse = serde_json::from_slice(&body)
            .map_err(|e| anyhow::anyhow!("drive: failed to parse token refresh response: {e}"))?;
        let expires_at = now_unix() + parsed.expires_in.unwrap_or(3600);
        Ok(Tokens {
            access_token: parsed.access_token,
            // Google omits refresh_token on refresh unless it rotated; keep
            // the existing one in that case.
            refresh_token: parsed
                .refresh_token
                .unwrap_or_else(|| current.refresh_token.clone()),
            expires_at,
        })
    }
}

/// Whether an access token with `expires_at` (Unix epoch seconds) should be
/// refreshed at `now` (Unix epoch seconds): true when within
/// [`REFRESH_SKEW_SECS`] of expiry (or already expired). Pure for testing.
fn is_expiring(expires_at: i64, now: i64) -> bool {
    expires_at - now < REFRESH_SKEW_SECS
}

/// Current Unix epoch seconds.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Builds the OAuth refresh HTTP client with DESIGN s5.8.4 timeouts and a
/// redirect::none policy (codex V-A1 / SPEC s4). Bounding the connect/total
/// time keeps a hung token endpoint from wedging every Drive request (the
/// refresh holds the token mutex across the await); disabling redirects keeps
/// the credential-bearing client from being steered to an attacker endpoint.
fn build_refresh_client(
    ca: &CustomCaConfig,
    proxy: &ProxyConfig,
) -> anyhow::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .connect_timeout(REFRESH_CONNECT_TIMEOUT)
        .timeout(REFRESH_TOTAL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    // Issue #34: add the user's custom root CA additively then the configured
    // proxy; fail-closed if either cannot be applied (a corporate proxy would
    // break the refresh otherwise, and silently ignoring the CA/proxy is never
    // correct).
    let builder = driven_tls::apply_custom_ca(builder, ca)?;
    driven_tls::apply_proxy(builder, proxy)?
        .build()
        .map_err(|e| anyhow::anyhow!("drive: failed to build OAuth refresh client: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_store::DriveErrorClassification;

    #[test]
    fn map_load_ok_is_some() {
        let r = map_load_result(Ok("tok".to_string())).unwrap();
        assert_eq!(r, Some("tok".to_string()));
    }

    #[test]
    fn client_creds_encode_decode_round_trips() {
        // A1: a BYO client id + secret round-trips through the single keychain
        // record (`client_id\nclient_secret`).
        let creds = ClientCreds {
            client_id: "byo-id.apps.googleusercontent.com".to_string(),
            client_secret: "byo-secret".to_string(),
        };
        let record = encode_client_creds(&creds);
        assert_eq!(decode_client_creds(&record), creds);
    }

    #[test]
    fn client_creds_decode_tolerates_pkce_empty_secret() {
        // A PKCE installed-app client has an empty secret; the record is
        // `id\n` and decodes to an empty secret.
        let creds = ClientCreds {
            client_id: "pkce-id".to_string(),
            client_secret: String::new(),
        };
        let record = encode_client_creds(&creds);
        assert_eq!(decode_client_creds(&record), creds);
        // A bare record with no newline is treated as a client id, empty secret.
        assert_eq!(
            decode_client_creds("bare-id"),
            ClientCreds {
                client_id: "bare-id".to_string(),
                client_secret: String::new(),
            }
        );
    }

    #[test]
    fn map_load_no_entry_is_none() {
        let r = map_load_result(Err(keyring::Error::NoEntry)).unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn map_load_other_error_propagates() {
        let r = map_load_result(Err(keyring::Error::Invalid(
            "service".to_string(),
            "bad".to_string(),
        )));
        assert!(r.is_err());
    }

    #[test]
    fn map_delete_ok_and_no_entry_are_idempotent() {
        assert!(map_delete_result(Ok(())).is_ok());
        assert!(map_delete_result(Err(keyring::Error::NoEntry)).is_ok());
    }

    #[test]
    fn map_delete_other_error_propagates() {
        let r = map_delete_result(Err(keyring::Error::Invalid(
            "service".to_string(),
            "bad".to_string(),
        )));
        assert!(r.is_err());
    }

    #[test]
    fn is_expiring_within_skew() {
        let now = 1_000_000;
        // Expires in 30s -> within the 60s skew -> refresh.
        assert!(is_expiring(now + 30, now));
        // Expires in 120s -> outside the skew -> no refresh.
        assert!(!is_expiring(now + 120, now));
        // Already expired -> refresh.
        assert!(is_expiring(now - 5, now));
    }

    #[test]
    fn keyring_token_store_account_accessor() {
        let store = KeyringTokenStore::new("alice@example.com");
        assert_eq!(store.account(), "alice@example.com");
    }

    #[test]
    fn invalid_grant_classification_is_auth() {
        // Sanity: the classification we surface on invalid_grant is the one
        // the executor downcasts for needs_reauth.
        let e = anyhow::Error::new(super::super::DriveError::Classified {
            kind: DriveErrorClassification::AuthInvalidGrant,
            source: anyhow::anyhow!("auth.invalid_grant"),
        });
        assert_eq!(
            super::super::classification_of(&e),
            Some(DriveErrorClassification::AuthInvalidGrant)
        );
    }

    #[test]
    fn refresh_client_builds_offline_with_timeouts() {
        // V-A1: the refresh client must build with timeouts + redirect::none.
        // Building it is offline (no network); a failure would be a TLS-init
        // bug, so this is a real assertion, not a skip.
        let client = build_refresh_client(&CustomCaConfig::none(), &ProxyConfig::system());
        assert!(
            client.is_ok(),
            "refresh client must build offline: {:?}",
            client.err()
        );
    }

    #[test]
    fn refresh_client_fails_closed_with_a_bad_ca() {
        // Issue #34: a configured-but-unloadable custom CA must FAIL the client
        // build (fail-closed), not silently fall back to system-trust-only. This
        // is the representative wiring assertion for the driven-tls threading.
        let missing = std::path::PathBuf::from("/driven/no/such/ca-bundle.pem");
        let ca = CustomCaConfig::from_path(Some(missing));
        assert!(
            build_refresh_client(&ca, &ProxyConfig::system()).is_err(),
            "a missing custom CA file must fail the refresh-client build"
        );
    }

    #[test]
    fn refresh_client_fails_closed_with_a_bad_proxy() {
        // Issue #34: a configured-but-invalid proxy URL must FAIL the refresh
        // client build closed, never building an unproxied credential client.
        let bad = ProxyConfig::Manual("ftp://nope:21".to_string());
        assert!(
            build_refresh_client(&CustomCaConfig::none(), &bad).is_err(),
            "an invalid proxy URL must fail the refresh-client build"
        );
    }

    #[test]
    fn with_store_wires_the_keychain_store() {
        // C-P2-4 / V-A3: with_store attaches a store; without it, none.
        let http = build_refresh_client(&CustomCaConfig::none(), &ProxyConfig::system()).unwrap();
        let tokens = Tokens {
            access_token: String::new(),
            refresh_token: "rt".to_string(),
            expires_at: 0,
        };
        let src = RefreshingTokenSource::new(tokens, http, "cid", "secret");
        assert!(src.store.is_none(), "new() wires no store");
        let store = Arc::new(KeyringTokenStore::new("acct@example.com"));
        let src = src.with_store(store);
        assert!(src.store.is_some(), "with_store wires the store");
    }
}
