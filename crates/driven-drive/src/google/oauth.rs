//! The PKCE loopback OAuth flow (SPEC s4).
//!
//! We use the `oauth2` crate (PKCE, bring-your-own reqwest client + loopback
//! handler) rather than `yup-oauth2` so the wizard drives the loopback and
//! emits progress events itself (SPEC s4 / s4.2). The flow binds both
//! `127.0.0.1` and `[::1]` on one port, opens the consent URL, validates the
//! returned `state` (CSRF) + `Host` against the exact registered redirect
//! URI, then exchanges the code for tokens.
//!
//! Security properties (SPEC s4):
//! - The registered `redirect_uri` is the literal `http://127.0.0.1:<port>/oauth/callback`
//!   IPv4 form. Google compares it byte-for-byte.
//! - The loopback handler runs on BOTH the v4 and v6 sockets (some setups
//!   resolve `localhost` to `::1`); whichever fires first wins, the other is
//!   dropped.
//! - `state` is compared CONSTANT-TIME against the CSRF token (defends a
//!   malicious tab redirecting its own code to our loopback).
//! - The `Host` header must equal the EXACT registered authority
//!   `127.0.0.1:<port>` even over the v6 socket - `localhost:<port>` is
//!   rejected (DNS-rebind / SSRF defence; we never register `localhost`).
//! - The code-exchange uses a `redirect::Policy::none()` reqwest client
//!   (SSRF defence via the OAuth server).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge, RedirectUrl,
    Scope, TokenResponse, TokenUrl,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::Sender;
use tracing::warn;

/// Tracing target for the OAuth flow.
const TARGET: &str = "driven::drive::oauth";

/// Google's installed-app authorization endpoint (SPEC s4).
const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Google's token endpoint (SPEC s4).
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The Drive scope Driven requests (full Drive access; SPEC s4).
const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";

/// The OpenID userinfo email scope (A5): lets Driven read the account's Google
/// email via the userinfo endpoint so the Accounts UI / needs_reauth banner show
/// the real address rather than a placeholder label.
const USERINFO_EMAIL_SCOPE: &str = "https://www.googleapis.com/auth/userinfo.email";

/// The OpenID userinfo profile scope (A5): lets Driven read the account's
/// display name from the userinfo endpoint.
const USERINFO_PROFILE_SCOPE: &str = "https://www.googleapis.com/auth/userinfo.profile";

/// Connect timeout for the code-exchange client (DESIGN s5.8.4; codex V-A1).
const EXCHANGE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Total-request timeout for the code-exchange client (DESIGN s5.8.4; V-A1).
const EXCHANGE_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// Overall ceiling on waiting for the loopback redirect (codex C-P2-3). If the
/// user never completes (or never reaches) the consent screen, the wizard must
/// not hang forever - it surfaces a timeout error after this. Generous so a
/// human reading the consent screen + signing in has ample time.
const WAIT_FOR_CODE_TIMEOUT: Duration = Duration::from_secs(300);

/// The loopback callback path the redirect URI registers.
const CALLBACK_PATH: &str = "/oauth/callback";

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
    let (listener_v4, listener_v6, port) = bind_dual_loopback().await?;
    // The redirect URI we register with Google MUST be one literal string and
    // Google compares it byte-for-byte. We always use the IPv4 (127.0.0.1)
    // form; the v6 socket only serves browsers that resolve `localhost` to
    // `::1` and then connect to `::1`, and we validate the Host header against
    // this exact authority either way.
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let expected_host = format!("127.0.0.1:{port}");

    // A redirect-disabled reqwest client (SSRF defence via the OAuth server;
    // SPEC s4 / oauth2 v5 upgrade notes) with bounded connect/total timeouts
    // (DESIGN s5.8.4; codex V-A1) so a hung token endpoint cannot stall the
    // code exchange indefinitely.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(EXCHANGE_CONNECT_TIMEOUT)
        .timeout(EXCHANGE_TOTAL_TIMEOUT)
        .build()?;

    let client = BasicClient::new(ClientId::new(client_id.to_string()))
        .set_client_secret(ClientSecret::new(client_secret.to_string()))
        .set_auth_uri(AuthUrl::new(GOOGLE_AUTH_URL.to_string())?)
        .set_token_uri(TokenUrl::new(GOOGLE_TOKEN_URL.to_string())?)
        .set_redirect_uri(RedirectUrl::new(redirect_uri.clone())?);

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let (auth_url, csrf_state) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(DRIVE_SCOPE.to_string()))
        // A5: request the userinfo email + profile scopes so the account's real
        // Google email + display name can be fetched from the userinfo endpoint.
        .add_scope(Scope::new(USERINFO_EMAIL_SCOPE.to_string()))
        .add_scope(Scope::new(USERINFO_PROFILE_SCOPE.to_string()))
        // `access_type=offline` + `prompt=consent` force Google to mint a
        // refresh token (otherwise re-auth yields only an access token).
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .set_pkce_challenge(pkce_challenge)
        .url();

    progress_tx.send(OAuthProgress::OpeningBrowser).await.ok();
    open_browser(auth_url.as_str())?;

    progress_tx
        .send(OAuthProgress::WaitingForRedirect)
        .await
        .ok();
    let code = wait_for_code(
        listener_v4,
        listener_v6,
        csrf_state.secret(),
        &expected_host,
    )
    .await?;

    progress_tx.send(OAuthProgress::ExchangingCode).await.ok();
    let token = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http)
        .await
        .map_err(|e| anyhow::anyhow!("oauth: code exchange failed: {e}"))?;

    let refresh = token
        .refresh_token()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "auth.consent_required: Google returned no refresh token; revoke + reauth with prompt=consent"
            )
        })?
        .secret()
        .to_string();

    let tokens = Tokens {
        access_token: token.access_token().secret().to_string(),
        refresh_token: refresh,
        expires_at: token
            .expires_in()
            .map(|d| now_unix() + d.as_secs() as i64)
            .unwrap_or_else(|| now_unix() + 3600),
    };
    progress_tx.send(OAuthProgress::Completed).await.ok();
    Ok(tokens)
}

/// Binds the loopback listeners for the redirect URI (SPEC s4).
///
/// Picks a port that can bind on BOTH `127.0.0.1` and `[::1]`, returning both
/// listeners and the shared port. Falls back to IPv4-only with a logged note
/// if a dual bind is not possible (SPEC s4: some setups resolve `localhost`
/// to `::1` and an IPv4-only bind fails silently with "connection refused").
///
/// Strategy: bind the v4 listener on an OS-chosen ephemeral port, read the
/// port back, then try to bind the v6 listener on the SAME port. If the v6
/// bind fails (no IPv6 loopback, or the port is taken on the v6 stack), we log
/// and proceed v4-only.
async fn bind_dual_loopback() -> anyhow::Result<(TcpListener, Option<TcpListener>, u16)> {
    let listener_v4 = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .map_err(|e| anyhow::anyhow!("oauth: failed to bind 127.0.0.1 loopback: {e}"))?;
    let port = listener_v4
        .local_addr()
        .map_err(|e| anyhow::anyhow!("oauth: failed to read loopback port: {e}"))?
        .port();

    let listener_v6 = match TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port))).await {
        Ok(l) => Some(l),
        Err(e) => {
            warn!(
                target: TARGET,
                port,
                error = %e,
                "could not dual-bind [::1] on the loopback port; proceeding IPv4-only"
            );
            None
        }
    };
    Ok((listener_v4, listener_v6, port))
}

/// The outcome of handling one accepted loopback connection.
enum CallbackOutcome {
    /// A valid callback carrying the authorization code (success page served).
    Code(String),
    /// A request we answered but that carried no valid code (bad
    /// path/host/state, or a transport read glitch): keep listening for the
    /// real redirect.
    Retry,
    /// The OAuth PROVIDER reported an error on the callback (e.g. the user
    /// clicked Deny -> `error=access_denied`). This is FATAL: the flow cannot
    /// succeed, so we stop waiting and surface it instead of looping forever
    /// (codex C-P2-3).
    ProviderError(String),
}

/// Runs the one-shot loopback HTTP handler on both listeners and returns the
/// authorization code (SPEC s4).
///
/// Accepts `GET /oauth/callback?code=...&state=...`, validates `state` against
/// `expected_state` (constant-time) and the `Host` header against the exact
/// registered redirect authority `expected_host` (rejecting
/// `localhost:<port>` even over the v6 socket; SPEC s4 DNS-rebinding defence),
/// serves a "you can close this tab" page, returns the code, and shuts both
/// listeners down. Hand-rolled with plain `tokio` (no axum; SPEC s1 layout).
///
/// Robustness (codex C-P2-3): the whole wait is bounded by
/// [`WAIT_FOR_CODE_TIMEOUT`] so it cannot hang regardless of input, and a
/// PROVIDER error on the callback (the user clicked Deny ->
/// `error=access_denied`) returns a fatal auth error immediately rather than
/// looping. A stray probe / malformed request is still answered and ignored.
async fn wait_for_code(
    listener_v4: TcpListener,
    listener_v6: Option<TcpListener>,
    expected_state: &str,
    expected_host: &str,
) -> anyhow::Result<String> {
    let accept_loop = async {
        loop {
            let accepted = match &listener_v6 {
                Some(v6) => {
                    tokio::select! {
                        r = listener_v4.accept() => r,
                        r = v6.accept() => r,
                    }
                }
                None => listener_v4.accept().await,
            };
            let (stream, _peer) = match accepted {
                Ok(pair) => pair,
                Err(e) => {
                    warn!(target: TARGET, error = %e, "loopback accept failed; retrying");
                    continue;
                }
            };
            match handle_one_connection(stream, expected_state, expected_host).await {
                Ok(CallbackOutcome::Code(code)) => return Ok(code),
                // A request we answered (bad state/host/path) but that did not
                // carry a valid code: keep listening for the real redirect.
                Ok(CallbackOutcome::Retry) => continue,
                // The user denied consent (or the provider reported an error):
                // fatal, stop waiting (C-P2-3).
                Ok(CallbackOutcome::ProviderError(err)) => {
                    return Err(anyhow::anyhow!("oauth: authorization denied: {err}"));
                }
                Err(e) => {
                    warn!(target: TARGET, error = %e, "loopback connection error; retrying");
                    continue;
                }
            }
        }
    };

    // Bound the overall wait so a consent screen that is never completed (or a
    // browser that never redirects back) cannot hang the wizard forever
    // (C-P2-3).
    match tokio::time::timeout(WAIT_FOR_CODE_TIMEOUT, accept_loop).await {
        Ok(result) => result,
        Err(_elapsed) => Err(anyhow::anyhow!(
            "oauth: timed out after {WAIT_FOR_CODE_TIMEOUT:?} waiting for the browser redirect; \
             no authorization code received"
        )),
    }
}

/// Handles a single accepted loopback connection. Returns
/// [`CallbackOutcome::Code`] on a valid callback (after serving the success
/// page), [`CallbackOutcome::ProviderError`] when the provider reported an
/// error (e.g. the user clicked Deny), [`CallbackOutcome::Retry`] when the
/// request was answered but carried no valid code (bad path/host/state), or
/// `Err` on an I/O failure reading the request.
async fn handle_one_connection(
    mut stream: tokio::net::TcpStream,
    expected_state: &str,
    expected_host: &str,
) -> anyhow::Result<CallbackOutcome> {
    // Read the request head (we only need the request line + headers; the
    // callback is a GET with no body). Bound the read so a malicious client
    // cannot make us buffer unboundedly.
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream
            .read(&mut tmp)
            .await
            .map_err(|e| anyhow::anyhow!("oauth: loopback read failed: {e}"))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        // The header section ends at the first CRLFCRLF.
        if find_header_end(&buf).is_some() || buf.len() > 16 * 1024 {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buf);
    let request_line = request.lines().next().unwrap_or("");
    // Request line: "GET /oauth/callback?code=..&state=.. HTTP/1.1".
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    // Validate Host header == the EXACT registered authority. Reject
    // `localhost:<port>` even over the v6 socket (DNS-rebind / SSRF defence).
    let host = header_value(&request, "host").unwrap_or_default();
    if !host.eq_ignore_ascii_case(expected_host) {
        warn!(
            target: TARGET,
            got = %host,
            expected = %expected_host,
            "rejecting loopback callback with unexpected Host header"
        );
        write_response(
            &mut stream,
            400,
            "Bad Request",
            "Invalid Host. This callback only accepts the registered loopback address.",
        )
        .await?;
        return Ok(CallbackOutcome::Retry);
    }

    if method != "GET" || !path_is_callback(target) {
        write_response(
            &mut stream,
            404,
            "Not Found",
            "Not the OAuth callback path.",
        )
        .await?;
        return Ok(CallbackOutcome::Retry);
    }

    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params = parse_query(query);

    // An OAuth error redirect carries `error=access_denied` etc. This is the
    // provider telling us the flow failed (e.g. the user clicked Deny). It is
    // FATAL - returning Retry here would hang the wizard forever (codex
    // C-P2-3). We still validate state first so a forged error from an
    // attacker tab cannot abort a legitimate flow.
    if let Some(err) = params.get("error") {
        let state = params.get("state").map(String::as_str).unwrap_or("");
        if !constant_time_eq(state.as_bytes(), expected_state.as_bytes()) {
            warn!(
                target: TARGET,
                "ignoring OAuth error callback with mismatched CSRF state"
            );
            write_response(
                &mut stream,
                400,
                "Bad Request",
                "State mismatch (CSRF). The authorization could not be verified.",
            )
            .await?;
            return Ok(CallbackOutcome::Retry);
        }
        let msg = format!("Authorization failed: {err}");
        write_response(&mut stream, 200, "OK", &page_html(&msg, false)).await?;
        return Ok(CallbackOutcome::ProviderError(err.clone()));
    }

    let state = params.get("state").map(String::as_str).unwrap_or("");
    if !constant_time_eq(state.as_bytes(), expected_state.as_bytes()) {
        warn!(target: TARGET, "rejecting loopback callback with mismatched CSRF state");
        write_response(
            &mut stream,
            400,
            "Bad Request",
            "State mismatch (CSRF). The authorization could not be verified.",
        )
        .await?;
        return Ok(CallbackOutcome::Retry);
    }

    let code = match params.get("code") {
        Some(c) if !c.is_empty() => c.clone(),
        _ => {
            write_response(
                &mut stream,
                400,
                "Bad Request",
                "Missing authorization code.",
            )
            .await?;
            return Ok(CallbackOutcome::Retry);
        }
    };

    // Serve the friendly success page, then return the code.
    write_response(
        &mut stream,
        200,
        "OK",
        &page_html(
            "Driven is now connected to Google Drive. You can close this tab.",
            true,
        ),
    )
    .await?;
    Ok(CallbackOutcome::Code(code))
}

/// Whether the request target's path equals the callback path (ignoring the
/// query string).
fn path_is_callback(target: &str) -> bool {
    let path = target.split('?').next().unwrap_or(target);
    path == CALLBACK_PATH
}

/// Finds the byte index just past the `\r\n\r\n` header terminator.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Extracts a (case-insensitive) header value from the raw request text.
fn header_value(request: &str, name: &str) -> Option<String> {
    for line in request.lines().skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// Parses a `key=value&key2=value2` query string into a map, percent-decoding
/// the values.
fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), percent_decode(v));
    }
    map
}

/// Minimal percent-decoding for `application/x-www-form-urlencoded` query
/// values (`%XX` escapes and `+` for space). Drive's `code`/`state` are
/// URL-safe but may contain `%`-escapes.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Constant-time byte-slice equality (CSRF state comparison; SPEC s4). Compares
/// every byte regardless of an early mismatch and folds the length difference
/// into the result so timing does not leak the prefix or length.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

/// Writes a minimal HTTP/1.1 response with a `Connection: close` so the
/// browser does not keep the socket open.
async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> anyhow::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("oauth: loopback write failed: {e}"))?;
    stream
        .flush()
        .await
        .map_err(|e| anyhow::anyhow!("oauth: loopback flush failed: {e}"))?;
    Ok(())
}

/// The "you can close this tab" HTML page (SPEC s4). `success` styles it as a
/// confirmation; otherwise as an error notice. No external assets so it
/// renders offline.
fn page_html(message: &str, success: bool) -> String {
    let title = if success {
        "Connected"
    } else {
        "Authorization error"
    };
    let color = if success { "#137333" } else { "#b3261e" };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Driven - {title}</title></head>\
         <body style=\"font-family: system-ui, -apple-system, sans-serif; \
         margin: 0; display: flex; align-items: center; justify-content: center; \
         min-height: 100vh; background: #f8f9fa;\">\
         <main style=\"text-align: center; padding: 2rem;\">\
         <h1 style=\"color: {color}; font-size: 1.4rem; margin-bottom: 0.5rem;\">{title}</h1>\
         <p style=\"color: #5f6368;\">{message}</p>\
         </main></body></html>"
    )
}

/// Current Unix epoch seconds (for the token expiry).
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_std_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn parse_query_decodes_values() {
        let q = parse_query("code=4%2F0Ab&state=xyz&empty=");
        assert_eq!(q.get("code").map(String::as_str), Some("4/0Ab"));
        assert_eq!(q.get("state").map(String::as_str), Some("xyz"));
        assert_eq!(q.get("empty").map(String::as_str), Some(""));
    }

    #[test]
    fn header_value_is_case_insensitive() {
        let req =
            "GET /oauth/callback?code=x HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nUser-Agent: t\r\n\r\n";
        assert_eq!(header_value(req, "host").as_deref(), Some("127.0.0.1:1234"));
        assert_eq!(header_value(req, "HOST").as_deref(), Some("127.0.0.1:1234"));
        assert_eq!(header_value(req, "missing"), None);
    }

    #[test]
    fn path_is_callback_ignores_query() {
        assert!(path_is_callback("/oauth/callback?code=x&state=y"));
        assert!(path_is_callback("/oauth/callback"));
        assert!(!path_is_callback("/other?code=x"));
        assert!(!path_is_callback("/"));
    }

    #[test]
    fn percent_decode_handles_escapes_and_plus() {
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
        // A malformed escape is passed through literally.
        assert_eq!(percent_decode("a%zz"), "a%zz");
    }

    #[tokio::test]
    async fn dual_loopback_binds_a_port() {
        let (v4, _v6, port) = bind_dual_loopback().await.unwrap();
        assert!(port > 0);
        assert_eq!(v4.local_addr().unwrap().port(), port);
    }

    /// Drives the hand-rolled loopback handler end-to-end against a real
    /// localhost socket: a valid GET callback returns the code and serves the
    /// success page; this exercises Host + state + path validation without any
    /// network egress.
    #[tokio::test]
    async fn wait_for_code_accepts_valid_callback() {
        let (v4, v6, port) = bind_dual_loopback().await.unwrap();
        let expected_host = format!("127.0.0.1:{port}");
        let state = "csrf-state-token";
        let host = expected_host.clone();

        let server =
            tokio::spawn(async move { wait_for_code(v4, v6, "csrf-state-token", &host).await });

        // Client: connect and send the callback request.
        let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let req = format!(
            "GET /oauth/callback?code=auth-code-123&state={state} HTTP/1.1\r\nHost: {expected_host}\r\nConnection: close\r\n\r\n"
        );
        conn.write_all(req.as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        conn.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8_lossy(&resp);
        assert!(resp.contains("200 OK"));
        assert!(resp.contains("close this tab"));

        let code = server.await.unwrap().unwrap();
        assert_eq!(code, "auth-code-123");
    }

    /// A callback with the wrong CSRF state is rejected (400) and the handler
    /// keeps waiting; a follow-up valid callback then succeeds.
    #[tokio::test]
    async fn wait_for_code_rejects_bad_state_then_accepts() {
        let (v4, v6, port) = bind_dual_loopback().await.unwrap();
        let expected_host = format!("127.0.0.1:{port}");
        let host = expected_host.clone();

        let server = tokio::spawn(async move { wait_for_code(v4, v6, "good-state", &host).await });

        // First: bad state -> 400, handler keeps waiting.
        let mut bad = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let bad_req = format!(
            "GET /oauth/callback?code=c&state=WRONG HTTP/1.1\r\nHost: {expected_host}\r\nConnection: close\r\n\r\n"
        );
        bad.write_all(bad_req.as_bytes()).await.unwrap();
        let mut bad_resp = Vec::new();
        bad.read_to_end(&mut bad_resp).await.unwrap();
        assert!(String::from_utf8_lossy(&bad_resp).contains("400"));

        // Then: valid state -> success.
        let mut good = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let good_req = format!(
            "GET /oauth/callback?code=real-code&state=good-state HTTP/1.1\r\nHost: {expected_host}\r\nConnection: close\r\n\r\n"
        );
        good.write_all(good_req.as_bytes()).await.unwrap();
        let mut good_resp = Vec::new();
        good.read_to_end(&mut good_resp).await.unwrap();

        let code = server.await.unwrap().unwrap();
        assert_eq!(code, "real-code");
    }

    /// A callback with a `localhost:<port>` Host (not the registered
    /// `127.0.0.1:<port>`) is rejected even though it reached the socket -
    /// the DNS-rebind / SSRF defence (SPEC s4).
    #[tokio::test]
    async fn wait_for_code_rejects_localhost_host() {
        let (v4, v6, port) = bind_dual_loopback().await.unwrap();
        let expected_host = format!("127.0.0.1:{port}");
        let host = expected_host.clone();

        let server = tokio::spawn(async move { wait_for_code(v4, v6, "good-state", &host).await });

        // Wrong Host (localhost) -> 400, handler keeps waiting.
        let mut bad = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let bad_req = format!(
            "GET /oauth/callback?code=c&state=good-state HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
        );
        bad.write_all(bad_req.as_bytes()).await.unwrap();
        let mut bad_resp = Vec::new();
        bad.read_to_end(&mut bad_resp).await.unwrap();
        assert!(
            String::from_utf8_lossy(&bad_resp).contains("400"),
            "localhost Host must be rejected"
        );

        // Then a correct-Host valid callback succeeds.
        let mut good = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let good_req = format!(
            "GET /oauth/callback?code=real-code&state=good-state HTTP/1.1\r\nHost: {expected_host}\r\nConnection: close\r\n\r\n"
        );
        good.write_all(good_req.as_bytes()).await.unwrap();
        let mut good_resp = Vec::new();
        good.read_to_end(&mut good_resp).await.unwrap();

        let code = server.await.unwrap().unwrap();
        assert_eq!(code, "real-code");
    }

    /// The user clicks Deny: the callback carries `error=access_denied` with a
    /// valid state. `wait_for_code` must return an Err (fatal) rather than
    /// hang forever (codex C-P2-3). The handler still serves a 200 error page.
    #[tokio::test]
    async fn wait_for_code_denied_returns_error_not_hang() {
        let (v4, v6, port) = bind_dual_loopback().await.unwrap();
        let expected_host = format!("127.0.0.1:{port}");
        let host = expected_host.clone();

        let server = tokio::spawn(async move { wait_for_code(v4, v6, "good-state", &host).await });

        let mut deny = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let deny_req = format!(
            "GET /oauth/callback?error=access_denied&state=good-state HTTP/1.1\r\nHost: {expected_host}\r\nConnection: close\r\n\r\n"
        );
        deny.write_all(deny_req.as_bytes()).await.unwrap();
        let mut deny_resp = Vec::new();
        deny.read_to_end(&mut deny_resp).await.unwrap();
        assert!(String::from_utf8_lossy(&deny_resp).contains("200 OK"));

        let result = server.await.unwrap();
        let err = result.expect_err("deny must surface a fatal error, not a code");
        assert!(
            err.to_string().contains("access_denied"),
            "error must name the provider denial: {err}"
        );
    }

    /// An `error=` callback with a MISMATCHED state is treated as a stray /
    /// forged request: the handler answers 400 and keeps waiting, so a later
    /// valid callback still succeeds (a forged deny cannot abort the flow).
    #[tokio::test]
    async fn wait_for_code_ignores_denied_with_bad_state() {
        let (v4, v6, port) = bind_dual_loopback().await.unwrap();
        let expected_host = format!("127.0.0.1:{port}");
        let host = expected_host.clone();

        let server = tokio::spawn(async move { wait_for_code(v4, v6, "good-state", &host).await });

        // Forged error with wrong state -> 400, keep waiting.
        let mut bad = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let bad_req = format!(
            "GET /oauth/callback?error=access_denied&state=WRONG HTTP/1.1\r\nHost: {expected_host}\r\nConnection: close\r\n\r\n"
        );
        bad.write_all(bad_req.as_bytes()).await.unwrap();
        let mut bad_resp = Vec::new();
        bad.read_to_end(&mut bad_resp).await.unwrap();
        assert!(String::from_utf8_lossy(&bad_resp).contains("400"));

        // Then a valid callback still succeeds.
        let mut good = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let good_req = format!(
            "GET /oauth/callback?code=real-code&state=good-state HTTP/1.1\r\nHost: {expected_host}\r\nConnection: close\r\n\r\n"
        );
        good.write_all(good_req.as_bytes()).await.unwrap();
        let mut good_resp = Vec::new();
        good.read_to_end(&mut good_resp).await.unwrap();

        let code = server.await.unwrap().unwrap();
        assert_eq!(code, "real-code");
    }
}
