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
//! M4 scaffold: the type surface is in place; bodies are `todo!()`.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::oauth::Tokens;

/// keyring "service" namespace for Driven Google refresh tokens (SPEC s4.1).
const KEYRING_SERVICE: &str = "driven.google.refresh_token";

/// Keychain wrapper for an account's Google refresh token (SPEC s4.1).
///
/// One entry per account; the account id is the keychain "user" within the
/// [`KEYRING_SERVICE`] namespace. Tests bind keyring to an in-memory mock
/// store so they never touch a real OS keychain.
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

    /// Persists `refresh_token` in the keychain for this account (SPEC s4.1).
    pub fn store_refresh_token(&self, refresh_token: &str) -> anyhow::Result<()> {
        let _ = refresh_token;
        todo!("M4 implement: keyring::Entry::new(KEYRING_SERVICE, account).set_password(refresh_token)")
    }

    /// Loads the stored refresh token, or `None` if the account has never
    /// authenticated (SPEC s4.1).
    pub fn load_refresh_token(&self) -> anyhow::Result<Option<String>> {
        todo!("M4 implement: keyring::Entry::get_password, NoEntry -> None")
    }

    /// Deletes the stored refresh token (e.g. on sign-out / revoke). Absent
    /// entry is a no-op (SPEC s4.1).
    pub fn delete_refresh_token(&self) -> anyhow::Result<()> {
        todo!("M4 implement: keyring::Entry::delete_credential, NoEntry -> Ok")
    }
}

/// A thin wrapper around [`Tokens`] + the OAuth client that refreshes the
/// access token on demand (SPEC s4.1).
///
/// Refreshes when `expires_at - now < 60s` by POSTing
/// `grant_type=refresh_token` to the Google token endpoint. An
/// `invalid_grant` response marks the account `needs_reauth` (SPEC s24
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

    /// Returns a currently-valid access token, refreshing first if the cached
    /// one expires within 60s (SPEC s4.1).
    pub async fn access_token(&self) -> anyhow::Result<String> {
        let _ = (&self.inner, &self.http, &self.client_id, &self.client_secret);
        todo!("M4 implement: return cached access token, refreshing if within 60s of expiry")
    }

    /// Forces a refresh of the access token from the stored refresh token
    /// (SPEC s4.1). `invalid_grant` surfaces as `auth.invalid_grant`.
    pub async fn force_refresh(&self) -> anyhow::Result<()> {
        todo!("M4 implement: POST grant_type=refresh_token; invalid_grant -> needs_reauth (SPEC s24)")
    }
}
