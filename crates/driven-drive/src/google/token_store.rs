//! Keychain-backed refresh-token storage + the [`RefreshingTokenSource`]
//! (SPEC s4.1).
//!
//! Refresh tokens persist in the OS keychain ONLY (SPEC s4.1); the access
//! token lives in memory and is regenerated on demand. [`KeyringTokenStore`]
//! is the keychain wrapper; [`RefreshingTokenSource`] holds the in-memory
//! [`Tokens`] plus the OAuth client and refreshes when the access token is
//! within 60s of expiry, marking the account `needs_reauth` on an
//! `invalid_grant` (SPEC s24 `auth.invalid_grant`).
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

use keyring::Entry;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::warn;

use super::oauth::Tokens;

/// Tracing target for the token store / refresh path.
const TARGET: &str = "driven::drive::token";

/// keyring "service" namespace for Driven Google refresh tokens (SPEC s4.1).
const KEYRING_SERVICE: &str = "driven.google.refresh_token";

/// Google's OAuth token endpoint (SPEC s4.1 refresh path).
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Refresh when the access token is within this many seconds of expiry
/// (SPEC s4.1 "refreshes when `expires_at - now < 60s`").
const REFRESH_SKEW_SECS: i64 = 60;

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
/// so the executor moves the account to `needs_reauth` (SPEC s24
/// `auth.invalid_grant`). The current [`Tokens`] are held behind an async
/// [`Mutex`] so concurrent Drive requests share one refresh.
#[derive(Clone)]
pub struct RefreshingTokenSource {
    inner: Arc<Mutex<Tokens>>,
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
}

impl RefreshingTokenSource {
    /// Builds a [`RefreshingTokenSource`] from the initial [`Tokens`], the
    /// authorized HTTP client, and the OAuth client credentials (SPEC s4.1).
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
        }
    }

    /// Builds a [`RefreshingTokenSource`] from a stored refresh token, with an
    /// internally-constructed OAuth refresh HTTP client (so callers - e.g.
    /// `driven-cli` - need no `reqwest` dependency). The initial access token
    /// is empty and already-expired, so the first [`Self::access_token`] call
    /// performs the refresh.
    pub fn from_stored_refresh_token(
        refresh_token: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| anyhow::anyhow!("drive: failed to build OAuth refresh client: {e}"))?;
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
            *guard = fresh;
        }
        Ok(guard.access_token.clone())
    }

    /// Forces a refresh of the access token from the stored refresh token
    /// (SPEC s4.1). `invalid_grant` surfaces as `auth.invalid_grant`.
    pub async fn force_refresh(&self) -> anyhow::Result<()> {
        let mut guard = self.inner.lock().await;
        let fresh = self.refresh_locked(&guard).await?;
        *guard = fresh;
        Ok(())
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
}
