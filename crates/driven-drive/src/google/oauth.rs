//! The PKCE loopback OAuth flow (SPEC s4).
//!
//! We use the `oauth2` crate (PKCE, bring-your-own reqwest client + loopback
//! handler) rather than `yup-oauth2` so the wizard drives the loopback and
//! emits progress events itself (SPEC s4 / s4.2). The flow binds both
//! `127.0.0.1` and `[::1]` on one port, opens the consent URL, validates the
//! returned `state` (CSRF) + `Host` against the exact registered redirect
//! URI, then exchanges the code for tokens.
//!
//! M4 scaffold: the public [`run_pkce_loopback_flow`] signature matches SPEC
//! s4 verbatim; the body and the private helpers are `todo!()`.

use tokio::net::TcpListener;
use tokio::sync::mpsc::Sender;

/// The token triple returned by a successful OAuth flow (SPEC s4).
///
/// The refresh token persists in the keychain (SPEC s4.1); the access token
/// lives in memory and is regenerated on demand from the refresh token.
#[derive(Debug, Clone)]
pub struct Tokens {
    /// The short-lived access token (regenerated on demand; SPEC s4.1).
    pub access_token: String,
    /// The long-lived refresh token (persisted in the keychain only).
    pub refresh_token: String,
    /// Unix epoch seconds at which `access_token` expires (SPEC s4.1 refreshes
    /// when `expires_at - now < 60s`).
    pub expires_at: i64,
}

/// Progress milestones emitted to the wizard while the loopback flow runs
/// (SPEC s4: the flow drives the loopback and emits progress itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthProgress {
    /// The consent URL is being opened in the user's browser.
    OpeningBrowser,
    /// Waiting for the browser to redirect back to the loopback listener with
    /// the authorization code.
    WaitingForRedirect,
    /// Exchanging the received authorization code for tokens.
    ExchangingCode,
    /// The flow completed and tokens were obtained.
    Completed,
}

/// Runs the PKCE loopback installed-app OAuth flow (SPEC s4).
///
/// Binds both `127.0.0.1` and `[::1]` on one port (RFC 8252 s7.3), opens the
/// consent URL via `open_browser`, waits for the loopback redirect, validates
/// the `state` (CSRF, constant-time) + `Host` against the exact registered
/// `http://127.0.0.1:<port>/oauth/callback` redirect URI, then exchanges the
/// authorization code for [`Tokens`]. Progress milestones are emitted on
/// `progress_tx` so the wizard can render the consent step.
///
/// `open_browser` is injected so a headless test (or the CLI) can supply its
/// own opener; production passes the platform browser-launcher.
pub async fn run_pkce_loopback_flow(
    client_id: &str,
    client_secret: &str,
    open_browser: impl FnOnce(&str) -> anyhow::Result<()>,
    progress_tx: Sender<OAuthProgress>,
) -> anyhow::Result<Tokens> {
    let _ = (client_id, client_secret, open_browser, progress_tx);
    todo!("M4 implement: PKCE loopback flow per SPEC s4 (dual-bind, consent, code exchange)")
}

/// Binds the loopback listeners for the redirect URI (SPEC s4).
///
/// Picks a port that can bind on BOTH `127.0.0.1` and `[::1]`, returning both
/// listeners and the shared port. Falls back to IPv4-only with a logged note
/// if a dual bind is not possible (SPEC s4: some setups resolve `localhost`
/// to `::1` and an IPv4-only bind fails silently with "connection refused").
//
// Scaffold: called only from the `todo!()` body of `run_pkce_loopback_flow`,
// so it reads as dead code until the implement phase wires the flow body.
#[allow(dead_code)]
async fn bind_dual_loopback() -> anyhow::Result<(TcpListener, Option<TcpListener>, u16)> {
    todo!("M4 implement: bind 127.0.0.1 + [::1] on one shared port (SPEC s4)")
}

/// Runs the one-shot loopback HTTP handler on both listeners and returns the
/// authorization code (SPEC s4).
///
/// Accepts `GET /oauth/callback?code=...&state=...`, validates `state` against
/// `expected_state` (constant-time) and the `Host` header against the exact
/// registered redirect authority `127.0.0.1:<port>` (rejecting
/// `localhost:<port>` even over the v6 socket; SPEC s4 DNS-rebinding defence),
/// serves a "you can close this tab" page, returns the code, and shuts both
/// listeners down. Hand-rolled with plain `tokio` (no axum; SPEC s1 layout).
//
// Scaffold: called only from the `todo!()` body of `run_pkce_loopback_flow`,
// so it reads as dead code until the implement phase wires the flow body.
#[allow(dead_code)]
async fn wait_for_code(
    listener_v4: TcpListener,
    listener_v6: Option<TcpListener>,
    expected_state: &str,
) -> anyhow::Result<String> {
    let _ = (listener_v4, listener_v6, expected_state);
    todo!("M4 implement: one-shot loopback handler, CSRF + Host validation, return code (SPEC s4)")
}
