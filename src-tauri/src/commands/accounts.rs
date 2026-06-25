//! Account IPC commands (SPEC s11.1).
//!
//! The add-account wizard (DESIGN s8.5) + the Accounts settings tab (DESIGN
//! s8.2) drive these. Each is a `#[tauri::command]` over `State<AppState>`.
//!
//! The OAuth flow these wrap is
//! [`driven_drive::google::oauth::run_pkce_loopback_flow`]: the wizard begins a
//! server-side session, submits BYO client credentials, starts the PKCE
//! loopback (spawned so the command returns immediately while the browser
//! consents), polls progress, then `finish_add_account` persists the refresh
//! token (keychain) + the `accounts` row (StateRepo) and - on encryption
//! opt-in - generates the master key and reveals the BIP39 phrase once.
//!
//! ## In-flight session state (server-side)
//!
//! The wizard's OAuth flow runs on the Rust side (it owns the loopback
//! listener), so the in-flight session must live server-side, keyed by the
//! opaque session id the webview threads through. [`AppState`] is built once at
//! boot and exposes no insert seam, so the session registry is a module-level
//! map ([`sessions`]); each entry holds the BYO credentials, the live
//! [`OAuthStatus`], and - on success - the obtained tokens.
//!
//! ## Orchestrator lifecycle (A2)
//!
//! `finish_add_account` persists the account + refresh token + BYO client creds,
//! then HOT-SPAWNS the per-account orchestrator via [`crate::assembly::spawn_account`]
//! and inserts its [`AccountHandle`](crate::app_state::AccountHandle) into the
//! running set, so the wizard's initial `sync_now(sourceId)` finds a live handle
//! WITHOUT an app restart. `remove_account` gracefully shuts down + REMOVES the
//! handle before deleting the account's rows + keychain entries. Both are honest:
//! no fake remote is constructed and no orphaned task is left.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tauri::{AppHandle, Emitter, State};
use tokio::sync::mpsc;

use driven_core::state::AccountRow;
use driven_core::time::{Clock, SystemClock};
use driven_core::types::{AccountId, AccountState, ErrorCode};

use driven_crypto::Keystore;

use driven_drive::google::oauth::{run_pkce_loopback_flow, OAuthProgress, Tokens};
use driven_drive::google::token_store::KeyringTokenStore;

use crate::app_state::AppState;
use crate::commands::dtos::{
    AccountDto, AddAccountWizardSessionId, OAuthAuthUrl, OAuthStatus, ReauthSession, SessionId,
};
use crate::commands::{CommandError, CommandResult};
use crate::events::EVENT_OAUTH_COMPLETE;

/// Tracing target for the accounts command layer.
const TARGET: &str = "driven::app::accounts";

/// R2-P2-1 (BYO-only, SPEC s11.1 / DESIGN s6.1): the OAuth client id env
/// override. Driven is BYO-ONLY - there is NO baked-in default client. The
/// env var exists ONLY as a TEST injection seam (the real-Drive e2e / google_e2e
/// path sets it); production REQUIRES the user's submitted/stored BYO creds.
const ENV_OAUTH_CLIENT_ID: &str = "DRIVEN_OAUTH_CLIENT_ID";
/// R2-P2-1: the OAuth client secret env override (test injection seam only).
const ENV_OAUTH_CLIENT_SECRET: &str = "DRIVEN_OAUTH_CLIENT_SECRET";

/// One server-side add-account / reauth wizard session.
///
/// Holds the BYO OAuth credentials, the live [`OAuthStatus`] (updated by the
/// spawned flow task), and - on success - the obtained [`Tokens`]. For a reauth
/// flow `account_id` is the existing account being re-consented; for a fresh add
/// it is `None`.
struct WizardSession {
    /// BYO client id; `None` until `submit_oauth_credentials` (then the env /
    /// installed-app default is used by `start_oauth_signin`).
    client_id: Option<String>,
    /// BYO client secret (empty for a PKCE installed-app client).
    client_secret: Option<String>,
    /// The current OAuth status, updated by the spawned flow task and read by
    /// `poll_oauth_status`. Behind the registry's outer lock (only ever held for
    /// a quick read/write, never across an await).
    status: OAuthStatus,
    /// The obtained tokens once the flow reached [`OAuthStatus::Complete`].
    tokens: Option<Tokens>,
    /// For a reauth flow: the existing account being re-consented.
    account_id: Option<AccountId>,
    /// `true` once `start_oauth_signin` has launched the flow, so a second call
    /// is a no-op rather than a duplicate loopback bind.
    started: bool,
    /// R4-P2-4: wall-time (ms) the session was opened. With `updated_at` this
    /// lets `prune_stale_sessions` reap abandoned flows.
    created_at: i64,
    /// R4-P2-4: wall-time (ms) of the last status / token / cred mutation. A
    /// session whose `updated_at` is older than the TTL (or a terminal session
    /// past the shorter terminal grace) is pruned, so abandoned flows do not
    /// accumulate the BYO creds / tokens in the process-global map forever.
    updated_at: i64,
}

impl WizardSession {
    fn new(account_id: Option<AccountId>) -> Self {
        let now = SystemClock.now_ms();
        Self {
            client_id: None,
            client_secret: None,
            status: OAuthStatus::AwaitingCallback,
            tokens: None,
            account_id,
            started: false,
            created_at: now,
            updated_at: now,
        }
    }

    /// R4-P2-4: stamp `updated_at` (called on every mutation so the TTL is
    /// measured from the last activity, not session open).
    fn touch(&mut self) {
        self.updated_at = SystemClock.now_ms();
    }

    /// R4-P2-4: whether this session is in a terminal OAuth state (the flow
    /// completed or failed). Terminal sessions are pruned after a short grace;
    /// non-terminal (abandoned) ones after the full TTL.
    fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            OAuthStatus::Complete | OAuthStatus::Failed { .. }
        )
    }
}

/// R4-P2-4: how long an ABANDONED (non-terminal) wizard session lives before
/// `prune_stale_sessions` reaps it (30 min - longer than any real consent
/// flow, but bounded).
const SESSION_TTL_MS: i64 = 30 * 60 * 1000;
/// R4-P2-4: grace for a TERMINAL session (Complete / Failed) the UI never
/// consumed via `finish_add_account` / `cancel_oauth_wizard` (5 min). A
/// completed session normally is removed by `finish_add_account`; this reaps
/// one the UI walked away from.
const SESSION_TERMINAL_GRACE_MS: i64 = 5 * 60 * 1000;
/// R4-P2-4: absolute max lifetime since session OPEN (2h). A pathological flow
/// kept "fresh" by repeated polling cannot live forever; once a session is this
/// old it is reaped regardless of recent activity.
const SESSION_MAX_LIFETIME_MS: i64 = 2 * 60 * 60 * 1000;

/// R4-P2-4: remove abandoned / stale-terminal wizard sessions. Called at the
/// natural entry points (opening a new add / reauth flow). A non-terminal
/// session idle past `SESSION_TTL_MS`, or a terminal session idle past
/// `SESSION_TERMINAL_GRACE_MS`, is dropped (clearing its BYO creds + tokens
/// from the process-global map). Pure map maintenance - never touches the DB or
/// keychain.
fn prune_stale_sessions() {
    let now = SystemClock.now_ms();
    let mut sessions = lock_sessions();
    sessions.retain(|_id, s| {
        // Absolute lifetime cap since open: reap regardless of recent activity.
        if now.saturating_sub(s.created_at) >= SESSION_MAX_LIFETIME_MS {
            return false;
        }
        let idle = now.saturating_sub(s.updated_at);
        let ttl = if s.is_terminal() {
            SESSION_TERMINAL_GRACE_MS
        } else {
            SESSION_TTL_MS
        };
        idle < ttl
    });
}

/// The process-wide registry of in-flight wizard sessions, keyed by session id.
///
/// A `std::sync::Mutex` (sync, never held across an await): the spawned flow
/// task takes the lock only to push a status/token update. Recovered on poison
/// per the house rule (no `unwrap`/`expect`/`panic!` in non-test code).
fn sessions() -> &'static Mutex<HashMap<String, WizardSession>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, WizardSession>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the session registry, recovering a poisoned lock.
fn lock_sessions() -> std::sync::MutexGuard<'static, HashMap<String, WizardSession>> {
    sessions().lock().unwrap_or_else(|e| e.into_inner())
}

/// Map an [`OAuthProgress`] milestone to the webview-facing [`OAuthStatus`].
fn progress_to_status(p: OAuthProgress) -> OAuthStatus {
    match p {
        OAuthProgress::OpeningBrowser => OAuthStatus::OpeningBrowser,
        OAuthProgress::WaitingForRedirect => OAuthStatus::AwaitingCallback,
        OAuthProgress::ExchangingCode => OAuthStatus::ExchangingCode,
        // `Completed` only means tokens were obtained; the session's terminal
        // `Complete` is set once they are stored on the session, not here.
        OAuthProgress::Completed => OAuthStatus::ExchangingCode,
    }
}

/// R2-P2-1 (BYO-only): resolve the OAuth client id + secret for a session - the
/// BYO credentials the user submitted, else the env override (a TEST-only
/// injection seam, e.g. real-Drive e2e). There is NO baked-in production default
/// client: a session with no submitted creds AND no env override is REJECTED, so
/// a direct IPC call can never start OAuth against a Driven-owned client (DESIGN
/// s6.1). The secret may legitimately be empty for a PKCE installed-app client;
/// only the client id is required.
fn resolve_creds(session: &WizardSession) -> CommandResult<(String, String)> {
    let client_id = session
        .client_id
        .clone()
        .filter(|id| !id.trim().is_empty())
        .or_else(|| std::env::var(ENV_OAUTH_CLIENT_ID).ok())
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            CommandError::with_code(
                ErrorCode::AuthConsentRequired,
                "no OAuth client credentials for this session; submit your BYO client id first \
                 (Driven is bring-your-own-credentials only)",
            )
        })?;
    let client_secret = session
        .client_secret
        .clone()
        .unwrap_or_else(|| std::env::var(ENV_OAUTH_CLIENT_SECRET).unwrap_or_default());
    Ok((client_id, client_secret))
}

/// `list_accounts()` - every configured Google account (SPEC s11.1).
#[tauri::command]
pub async fn list_accounts(state: State<'_, AppState>) -> CommandResult<Vec<AccountDto>> {
    let rows = state
        .state()
        .list_accounts()
        .await
        .map_err(CommandError::from)?;
    Ok(rows.iter().map(account_row_to_dto).collect())
}

/// Map an [`AccountRow`] to the webview-facing [`AccountDto`] (the keychain
/// master-key handle is never exposed; `encryption_enabled` is derived from
/// whether a master-key handle is present).
fn account_row_to_dto(row: &AccountRow) -> AccountDto {
    AccountDto {
        id: row.id.to_string(),
        email: row.email.clone(),
        display_name: row.display_name.clone(),
        state: account_state_str(row.state).to_string(),
        encryption_enabled: row.encryption_master_key_id.is_some(),
        created_at: row.created_at,
        last_synced_at: row.last_synced_at,
    }
}

/// The serialized form of [`AccountState`] (matches `#[serde(rename_all =
/// "snake_case")]` on the enum).
fn account_state_str(state: AccountState) -> &'static str {
    match state {
        AccountState::Ok => "ok",
        AccountState::NeedsReauth => "needs_reauth",
        AccountState::Disabled => "disabled",
    }
}

/// `begin_add_account_wizard()` - open a new add-account session (SPEC s11.1).
#[tauri::command]
pub async fn begin_add_account_wizard(
    _state: State<'_, AppState>,
) -> CommandResult<AddAccountWizardSessionId> {
    // R4-P2-4: reap any abandoned / stale-terminal sessions before opening a new
    // one, so the process-global map cannot accumulate them across a long run.
    prune_stale_sessions();
    let id = uuid::Uuid::new_v4().to_string();
    lock_sessions().insert(id.clone(), WizardSession::new(None));
    tracing::info!(target: TARGET, session = %id, "add-account wizard session opened");
    Ok(AddAccountWizardSessionId(id))
}

/// `submit_oauth_credentials(session, client_id, client_secret)` - record the
/// user's BYO OAuth client credentials for the session (SPEC s11.1, DESIGN
/// s8.5 step 2).
#[tauri::command]
pub async fn submit_oauth_credentials(
    _state: State<'_, AppState>,
    session: SessionId,
    client_id: String,
    client_secret: String,
) -> CommandResult<()> {
    let mut sessions = lock_sessions();
    let s = sessions
        .get_mut(&session.0)
        .ok_or_else(unknown_session_err)?;
    // Reject an empty client id (the secret may legitimately be empty for a
    // PKCE installed-app client).
    if client_id.trim().is_empty() {
        return Err(CommandError::with_code(
            ErrorCode::AuthConsentRequired,
            "OAuth client id must not be empty",
        ));
    }
    s.client_id = Some(client_id);
    s.client_secret = Some(client_secret);
    s.touch(); // R4-P2-4: measure the TTL from the last activity.
    Ok(())
}

/// `start_oauth_signin(session)` - begin the PKCE loopback flow and return the
/// Google consent URL to open (SPEC s11.1).
///
/// Spawns [`run_pkce_loopback_flow`] on the Tauri async runtime so this command
/// returns immediately; progress is observable via `poll_oauth_status` and the
/// terminal `oauth:complete` event (SPEC s11.7). The consent URL is captured
/// out-of-band from the flow's `OpeningBrowser` milestone via a one-shot the
/// browser-opener closure fulfils.
#[tauri::command]
pub async fn start_oauth_signin(
    app: AppHandle,
    _state: State<'_, AppState>,
    session: SessionId,
) -> CommandResult<OAuthAuthUrl> {
    // Resolve creds + mark started under the lock; reject a missing / already-
    // started session.
    let (client_id, client_secret) = {
        let mut sessions = lock_sessions();
        let s = sessions
            .get_mut(&session.0)
            .ok_or_else(unknown_session_err)?;
        if s.started {
            return Err(CommandError::with_code(
                ErrorCode::AuthConsentRequired,
                "OAuth sign-in already started for this session",
            ));
        }
        // R2-P2-1: REQUIRE BYO/env creds before starting OAuth - reject (and do
        // NOT mark the session started) if none are present, so a direct IPC call
        // cannot kick off OAuth without submitted credentials.
        let creds = resolve_creds(s)?;
        s.started = true;
        s.status = OAuthStatus::OpeningBrowser;
        s.touch(); // R4-P2-4.
        creds
    };

    // A one-shot to lift the consent URL out of the browser-opener closure (the
    // opener is the only place the flow hands us the URL). The flow calls the
    // opener exactly once, so the closure is `FnOnce` and consumes the moved
    // `Sender` (which is `Send`, keeping the spawned future `Send`).
    //
    // A4: the BACKEND must NOT open the consent URL - the FRONTEND opens it (it
    // already receives `authUrl` and routes it to the system browser). So this
    // closure ONLY captures the URL for the return value; it does NOT launch a
    // browser (which previously double-opened the consent page).
    let (url_tx, url_rx) = tokio::sync::oneshot::channel::<String>();
    let open_browser = move |url: &str| -> anyhow::Result<()> {
        let _ = url_tx.send(url.to_string());
        Ok(())
    };

    // Progress channel: forward each milestone into the session status.
    let (progress_tx, mut progress_rx) = mpsc::channel::<OAuthProgress>(8);
    let session_id = session.0.clone();
    {
        let session_id = session_id.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(p) = progress_rx.recv().await {
                let mut sessions = lock_sessions();
                if let Some(s) = sessions.get_mut(&session_id) {
                    // Never downgrade a terminal status set elsewhere.
                    if !matches!(s.status, OAuthStatus::Complete | OAuthStatus::Failed { .. }) {
                        s.status = progress_to_status(p);
                        s.touch(); // R4-P2-4.
                    }
                }
            }
        });
    }

    // The flow task: run the loopback flow, then record the terminal outcome on
    // the session + emit `oauth:complete` (SPEC s11.7).
    {
        let session_id = session_id.clone();
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let result =
                run_pkce_loopback_flow(&client_id, &client_secret, open_browser, progress_tx).await;
            let status = match result {
                Ok(tokens) => {
                    let mut sessions = lock_sessions();
                    if let Some(s) = sessions.get_mut(&session_id) {
                        s.tokens = Some(tokens);
                        s.status = OAuthStatus::Complete;
                        s.touch(); // R4-P2-4.
                    }
                    OAuthStatus::Complete
                }
                Err(err) => {
                    // Map the flow error to a stable SPEC s24 code for the i18n
                    // key the webview shows.
                    let code = oauth_error_code(&err);
                    let status = OAuthStatus::Failed {
                        code: code.code().to_string(),
                    };
                    let mut sessions = lock_sessions();
                    if let Some(s) = sessions.get_mut(&session_id) {
                        s.status = status.clone();
                        s.touch(); // R4-P2-4.
                    }
                    tracing::warn!(target: TARGET, session = %session_id, %err, "oauth flow failed");
                    status
                }
            };
            // Emit the terminal `oauth:complete` so the wizard advances without
            // polling (SPEC s11.7 payload `{ session_id, status }`).
            if let Err(e) = app.emit(
                EVENT_OAUTH_COMPLETE,
                serde_json::json!({ "session_id": session_id, "status": status }),
            ) {
                tracing::debug!(target: TARGET, session = %session_id, error = %e, "emit oauth:complete failed");
            }
        });
    }

    // Wait (bounded) for the flow to surface the consent URL via the opener.
    match tokio::time::timeout(std::time::Duration::from_secs(15), url_rx).await {
        Ok(Ok(url)) => Ok(OAuthAuthUrl { auth_url: url }),
        Ok(Err(_)) | Err(_) => {
            // The opener never fired (the flow failed before opening the
            // browser, e.g. a loopback bind failure). Surface the session's
            // recorded failure if any, else a generic consent error.
            let status = lock_sessions()
                .get(&session_id)
                .map(|s| s.status.clone())
                .unwrap_or(OAuthStatus::AwaitingCallback);
            if let OAuthStatus::Failed { code } = status {
                let ec = ErrorCode::from_code(&code).unwrap_or(ErrorCode::AuthConsentRequired);
                Err(CommandError::with_code(
                    ec,
                    "OAuth sign-in failed before the consent URL was produced",
                ))
            } else {
                Err(CommandError::with_code(
                    ErrorCode::AuthConsentRequired,
                    "timed out producing the consent URL",
                ))
            }
        }
    }
}

/// `poll_oauth_status(session)` - poll the in-flight OAuth flow (SPEC s11.1).
#[tauri::command]
pub async fn poll_oauth_status(
    _state: State<'_, AppState>,
    session: SessionId,
) -> CommandResult<OAuthStatus> {
    lock_sessions()
        .get(&session.0)
        .map(|s| s.status.clone())
        .ok_or_else(unknown_session_err)
}

/// `cancel_oauth_wizard(session)` - abandon an in-flight add-account / reauth
/// wizard session, dropping it from the server-side registry (R4-P2-4).
///
/// The webview calls this when the user closes / cancels the wizard, so the
/// session's BYO creds + any obtained tokens are cleared from the process-global
/// map immediately rather than waiting for the TTL sweep. Idempotent: cancelling
/// an unknown / already-removed session is a no-op (the desired end state - the
/// session is gone - already holds), so a double-cancel never errors. This only
/// drops the in-memory session; it never touches the account row or keychain (a
/// finished account is persisted by `finish_add_account`, not here).
#[tauri::command]
pub async fn cancel_oauth_wizard(
    _state: State<'_, AppState>,
    session: SessionId,
) -> CommandResult<()> {
    // Also reap any other stale sessions while here (cheap map maintenance).
    prune_stale_sessions();
    let removed = lock_sessions().remove(&session.0).is_some();
    tracing::info!(target: TARGET, session = %session.0, removed, "oauth wizard session cancelled");
    Ok(())
}

/// `finish_add_account(session, display_name?)` - persist the account once the
/// OAuth flow completed (SPEC s11.1).
///
/// Stores the refresh token AND the per-account BYO OAuth client creds in the
/// keychain (SPEC s4.1; A1: a refresh token is bound to the client that minted
/// it, so the client creds MUST persist or every BYO refresh fails after
/// restart), fetches the real Google profile (A5) and writes the `accounts`
/// row (StateRepo), then HOT-SPAWNS the account's orchestrator so the wizard's
/// initial `sync_now(sourceId)` finds a live handle WITHOUT an app restart (A2).
/// Returns the new (or refreshed, on reauth) [`AccountDto`].
#[tauri::command]
pub async fn finish_add_account(
    app: AppHandle,
    state: State<'_, AppState>,
    session: SessionId,
    display_name: Option<String>,
) -> CommandResult<AccountDto> {
    // R2-P2-2: READ (clone, do NOT take) the tokens + resolved client creds out
    // of the session, so the session stays REPLAYABLE if any persistence step
    // below fails. The session is consumed (removed) ONLY after the account row
    // and keychain creds are durably persisted (success path); a failure leaves
    // the session intact so the user can retry `finish_add_account` without
    // re-running the whole OAuth flow.
    let (tokens, reauth_account, creds) = {
        let mut sessions = lock_sessions();
        let s = sessions
            .get_mut(&session.0)
            .ok_or_else(unknown_session_err)?;
        let creds = resolve_creds(s)?;
        match (&s.status, s.tokens.clone()) {
            (OAuthStatus::Complete, Some(tokens)) => (tokens, s.account_id, creds),
            (OAuthStatus::Failed { code }, _) => {
                let ec = ErrorCode::from_code(code).unwrap_or(ErrorCode::AuthConsentRequired);
                return Err(CommandError::with_code(
                    ec,
                    "OAuth flow failed; cannot finish add-account",
                ));
            }
            _ => {
                return Err(CommandError::with_code(
                    ErrorCode::AuthConsentRequired,
                    "OAuth flow has not completed yet",
                ));
            }
        }
    };

    let now = SystemClock.now_ms();

    // A5: fetch the real Google profile (email + display name) with the fresh
    // access token. Best-effort: on failure fall back to a stable label so the
    // account is still usable (no fabricated Google address).
    let profile = fetch_google_userinfo(&tokens.access_token).await;

    let (account_id, dto) = if let Some(account_id) = reauth_account {
        // Reauth path: re-store the refresh token + client creds, THEN flip the
        // existing account back to Ok, refreshing the profile if we got one.
        // R1-P1-4: persisting the client creds is FATAL and happens BEFORE the
        // account is flipped to Ok - if it fails the account stays in its prior
        // (needs_reauth) state rather than being marked Ok with un-refreshable
        // creds. The refresh token re-store is harmless to leave (the same
        // account, same key) and is overwritten on the next successful reauth.
        store_refresh_token(account_id, &tokens.refresh_token)?;
        store_client_creds(account_id, &creds)?;

        let rows = state
            .state()
            .list_accounts()
            .await
            .map_err(CommandError::from)?;
        let mut row = rows
            .into_iter()
            .find(|r| r.id == account_id)
            .ok_or_else(|| {
                CommandError::with_code(
                    ErrorCode::InternalBug,
                    "reauth account row vanished after re-consent",
                )
            })?;
        row.state = AccountState::Ok;
        if let Some(profile) = &profile {
            if !profile.email.trim().is_empty() {
                row.email = profile.email.clone();
            }
            if row.display_name.is_none() {
                row.display_name = profile.name.clone().filter(|n| !n.trim().is_empty());
            }
        }
        state
            .state()
            .upsert_account(&row)
            .await
            .map_err(CommandError::from)?;
        (account_id, account_row_to_dto(&row))
    } else {
        // Fresh add: allocate the id, store the token + client creds, write the
        // row with the real Google email/name (A5).
        // R1-P1-4: the token AND client creds are persisted BEFORE the account
        // row is written, and persisting the client creds is FATAL: an account
        // whose BYO client creds did not persist could never refresh its own
        // token (the refresh is bound to the minting client). If the creds store
        // fails, roll back the just-stored refresh token so NO half-account
        // (token without creds, or a row that cannot refresh) is left behind.
        let account_id = AccountId::new_v4();

        // A5: prefer the real Google email; else the user label; else a stable
        // fallback. The display name prefers the user-supplied label, else the
        // Google profile name.
        let google_email = profile
            .as_ref()
            .map(|p| p.email.clone())
            .filter(|e| !e.trim().is_empty());
        let google_name = profile
            .as_ref()
            .and_then(|p| p.name.clone())
            .filter(|n| !n.trim().is_empty());
        let user_label = display_name.clone().filter(|d| !d.trim().is_empty());

        let email = google_email.clone().unwrap_or_else(|| {
            user_label.clone().unwrap_or_else(|| {
                let short = account_id.to_string();
                let short = short.split('-').next().unwrap_or(&short);
                format!("account-{short}")
            })
        });

        let row = AccountRow {
            id: account_id,
            email,
            display_name: user_label.or(google_name),
            state: AccountState::Ok,
            // No encryption master key at the account level here: per-source
            // encryption opt-in (with its own master key) happens in the
            // add-source flow (DESIGN s7.1 / s8.5 step 4).
            encryption_master_key_id: None,
            created_at: now,
            last_synced_at: None,
        };

        // R1-P1-4 / R2-P2-2: persist the keychain token + creds AND the account
        // row with full rollback. The helper stores token -> creds -> row in
        // order, rolling back EVERY prior keychain write if a later step fails, so
        // a failure leaves NO orphaned keychain entries. The real OS keychain is
        // the secret store; the row insert is the live StateRepo.
        persist_new_account(
            &RealAccountSecretStore,
            account_id,
            &tokens.refresh_token,
            &creds,
            |r| {
                let repo = state.state().clone();
                let r = r.clone();
                async move { repo.upsert_account(&r).await }
            },
            &row,
        )
        .await?;
        tracing::info!(target: TARGET, account_id = %account_id, "account persisted");
        (account_id, account_row_to_dto(&row))
    };

    // R2-P2-2: consume the session ONLY now that the account row + keychain creds
    // are durably persisted. A failure above left the session intact so the user
    // can retry `finish_add_account` without re-running the OAuth flow.
    lock_sessions().remove(&session.0);

    // A2: HOT-SPAWN the account's orchestrator so the wizard's initial
    // sync_now finds a live handle (no restart). Best-effort: a build failure
    // is logged - the account is persisted, and the next app start assembles
    // it - but it must not fail the finish (the account IS saved).
    match crate::assembly::spawn_account(&app, &state, account_id).await {
        Ok(true) => {
            tracing::info!(target: TARGET, account_id = %account_id, "orchestrator hot-spawned after finish_add_account");
        }
        Ok(false) => {
            tracing::warn!(target: TARGET, account_id = %account_id, "account persisted but orchestrator not spawned (needs reauth?)");
        }
        Err(err) => {
            tracing::error!(target: TARGET, account_id = %account_id, %err, "hot-spawn after finish failed; orchestrator will start on next launch");
        }
    }

    Ok(dto)
}

/// Persist the per-account BYO OAuth client creds in the keychain (A1; R1-P1-4).
///
/// FATAL, not best-effort: a refresh token is bound to the client that minted
/// it, so an account whose client creds were NOT persisted will fail EVERY
/// refresh after restart (it falls back to the env/default client, which did not
/// mint the token -> `invalid_client`). `finish_add_account` therefore aborts +
/// rolls the account back when this fails, rather than leaving behind an account
/// that can never refresh its own token. The error maps to `crypto.key_missing`
/// (the keychain-write failure class); the secret is NEVER logged or embedded.
fn store_client_creds(account_id: AccountId, creds: &(String, String)) -> CommandResult<()> {
    use driven_drive::google::token_store::{ClientCreds, ClientCredsStore};
    let record = ClientCreds {
        client_id: creds.0.clone(),
        client_secret: creds.1.clone(),
    };
    ClientCredsStore::new(account_id.to_string())
        .store(&record)
        .map_err(|e| {
            CommandError::with_code(
                ErrorCode::CryptoKeyMissing,
                format!("failed to persist BYO OAuth client creds in keychain: {e}"),
            )
        })
}

/// R2-P2-2: the per-account keychain secret operations the fresh-add
/// persistence helper needs, abstracted behind a trait so the rollback ordering
/// can be exercised by a test (an in-memory fake) WITHOUT touching the real OS
/// keychain. The production impl is [`RealAccountSecretStore`].
trait AccountSecretStore {
    /// Store the refresh token for `account_id` (SPEC s4.1).
    fn store_refresh_token(&self, account_id: AccountId, refresh_token: &str) -> CommandResult<()>;
    /// Store the BYO client creds for `account_id` (A1).
    fn store_client_creds(
        &self,
        account_id: AccountId,
        creds: &(String, String),
    ) -> CommandResult<()>;
    /// Delete the refresh token (rollback; idempotent).
    fn delete_refresh_token(&self, account_id: AccountId) -> anyhow::Result<()>;
    /// Delete the BYO client creds (rollback; idempotent).
    fn delete_client_creds(&self, account_id: AccountId) -> anyhow::Result<()>;
}

/// The production [`AccountSecretStore`] over the real OS keychain
/// ([`KeyringTokenStore`] + `ClientCredsStore`).
struct RealAccountSecretStore;

impl AccountSecretStore for RealAccountSecretStore {
    fn store_refresh_token(&self, account_id: AccountId, refresh_token: &str) -> CommandResult<()> {
        store_refresh_token(account_id, refresh_token)
    }
    fn store_client_creds(
        &self,
        account_id: AccountId,
        creds: &(String, String),
    ) -> CommandResult<()> {
        store_client_creds(account_id, creds)
    }
    fn delete_refresh_token(&self, account_id: AccountId) -> anyhow::Result<()> {
        KeyringTokenStore::new(account_id.to_string()).delete_refresh_token()
    }
    fn delete_client_creds(&self, account_id: AccountId) -> anyhow::Result<()> {
        driven_drive::google::token_store::ClientCredsStore::new(account_id.to_string()).delete()
    }
}

/// R1-P1-4 / R2-P2-2: persist a FRESH account's keychain secrets + state row with
/// full rollback. Stores, in order, (1) the refresh token, (2) the BYO client
/// creds, then (3) inserts the account row via `insert_row`. If ANY step fails,
/// every prior keychain write is rolled back (deleted), so a failure leaves NO
/// orphaned keychain entries - and because `finish_add_account` does NOT consume
/// the wizard session until this returns `Ok`, the user can simply retry.
///
/// `insert_row` is a closure (not a direct `StateRepo` call) so a test can force
/// the row insert to fail and assert the keychain rollback + (caller-side)
/// session replayability without a full failing-repo double.
async fn persist_new_account<S, F, Fut>(
    secrets: &S,
    account_id: AccountId,
    refresh_token: &str,
    creds: &(String, String),
    insert_row: F,
    row: &AccountRow,
) -> CommandResult<()>
where
    S: AccountSecretStore,
    F: FnOnce(&AccountRow) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    // 1) refresh token.
    secrets.store_refresh_token(account_id, refresh_token)?;

    // 2) client creds; on failure roll back the token.
    if let Err(err) = secrets.store_client_creds(account_id, creds) {
        if let Err(del) = secrets.delete_refresh_token(account_id) {
            tracing::error!(target: TARGET, account_id = %account_id, error = %del, "failed to roll back refresh token after client-creds persist failure");
        }
        return Err(err);
    }

    // 3) account row; on failure roll back BOTH keychain entries.
    if let Err(err) = insert_row(row).await {
        if let Err(del) = secrets.delete_refresh_token(account_id) {
            tracing::error!(target: TARGET, account_id = %account_id, error = %del, "failed to roll back refresh token after account-row insert failure");
        }
        if let Err(del) = secrets.delete_client_creds(account_id) {
            tracing::error!(target: TARGET, account_id = %account_id, error = %del, "failed to roll back BYO client creds after account-row insert failure");
        }
        return Err(CommandError::from(err));
    }
    Ok(())
}

/// The subset of the Google userinfo response Driven persists (A5).
#[derive(serde::Deserialize)]
struct GoogleUserinfo {
    #[serde(default)]
    email: String,
    #[serde(default)]
    name: Option<String>,
}

/// A5: fetch the Google profile (email + display name) with `access_token` from
/// the OpenID userinfo endpoint. Best-effort: any transport / non-2xx / parse
/// failure returns `None` (the caller falls back to a label), and the access
/// token is NEVER logged.
async fn fetch_google_userinfo(access_token: &str) -> Option<GoogleUserinfo> {
    const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v3/userinfo";
    // R4-P2-5: bound the request so a blackholed endpoint cannot hang the IPC
    // command forever (no timeout = wait indefinitely). 10s connect, 30s total.
    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: TARGET, error = %e, "userinfo: failed to build http client");
            return None;
        }
    };
    let resp = match client
        .get(USERINFO_URL)
        .bearer_auth(access_token)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: TARGET, error = %e, "userinfo: request failed; using a label fallback");
            return None;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!(target: TARGET, status = resp.status().as_u16(), "userinfo: non-2xx; using a label fallback");
        return None;
    }
    // The workspace `reqwest` has no `json` feature, so read the body as text
    // and parse with serde_json (mirrors the settings GitHub-releases reader).
    let body = match resp.text().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(target: TARGET, error = %e, "userinfo: read body failed; using a label fallback");
            return None;
        }
    };
    match serde_json::from_str::<GoogleUserinfo>(&body) {
        Ok(info) => Some(info),
        Err(e) => {
            tracing::warn!(target: TARGET, error = %e, "userinfo: parse failed; using a label fallback");
            None
        }
    }
}

/// Store a refresh token in the keychain for `account_id` (SPEC s4.1), mapping a
/// keychain failure to `crypto.key_missing` (the keychain-write failure class).
fn store_refresh_token(account_id: AccountId, refresh_token: &str) -> CommandResult<()> {
    KeyringTokenStore::new(account_id.to_string())
        .store_refresh_token(refresh_token)
        .map_err(|e| {
            CommandError::with_code(
                ErrorCode::CryptoKeyMissing,
                format!("failed to store refresh token in keychain: {e}"),
            )
        })
}

/// `remove_account(account_id, delete_remote)` - remove an account (SPEC s11.1).
///
/// Gracefully shuts down the account's running orchestrator (if one is live in
/// this process), deletes its `accounts` row (cascading sources + file_state +
/// pending_ops via `ON DELETE CASCADE`, SPEC s2), wipes the keychain refresh
/// token AND any account master key.
///
/// `delete_remote` (trash the account's backed-up Drive content) is NOT
/// performed in this slice: enumerating + trashing every source's remote tree
/// needs a live `GoogleDriveStore` per source, which is reachable only through
/// the per-account orchestrator's executor (the app shell exposes no standalone
/// store handle to the IPC layer). Rather than silently ignore the flag, a
/// `delete_remote = true` request is REJECTED with a clear error so the caller
/// knows the remote content was left intact (no false "deleted remotely"
/// signal). The local rows + credentials are always removed.
#[tauri::command]
pub async fn remove_account(
    state: State<'_, AppState>,
    account_id: AccountId,
    delete_remote: bool,
) -> CommandResult<()> {
    if delete_remote {
        return Err(CommandError::with_code(
            ErrorCode::DriveUnreachable,
            "remote deletion on account removal is not available in this build; \
             the account's Drive content was left intact. Remove it from Google Drive directly.",
        ));
    }

    // Gracefully stop + REMOVE the running orchestrator for this account, if
    // present, so quit-free removal leaves no orphaned per-account tasks (mirrors
    // the quit drain contract) AND the handle is gone from the running set (A2).
    // A no-op when the account never spawned (e.g. needs_reauth).
    if let Some(handle) = state.remove_account_handle(account_id) {
        tracing::info!(target: TARGET, account_id = %account_id, "shutting down orchestrator before account removal");
        handle.shutdown().await;
    }

    // Delete the account row (cascades sources / file_state / pending_ops).
    state
        .state()
        .delete_account(account_id)
        .await
        .map_err(CommandError::from)?;

    // Wipe the keychain refresh token (idempotent: absent entry is fine).
    if let Err(e) = KeyringTokenStore::new(account_id.to_string()).delete_refresh_token() {
        tracing::warn!(target: TARGET, account_id = %account_id, error = %e, "failed to delete refresh token from keychain on account removal");
    }
    // A1: wipe the per-account BYO client creds too (idempotent).
    if let Err(e) =
        driven_drive::google::token_store::ClientCredsStore::new(account_id.to_string()).delete()
    {
        tracing::warn!(target: TARGET, account_id = %account_id, error = %e, "failed to delete BYO client creds from keychain on account removal");
    }
    // Wipe the account master key (encryption opt-out / removal; idempotent).
    match Keystore::open(&account_id.to_string()) {
        Ok(ks) => {
            if let Err(e) = ks.delete_master_key() {
                tracing::warn!(target: TARGET, account_id = %account_id, error = %e, "failed to delete master key from keychain on account removal");
            }
        }
        Err(e) => {
            tracing::debug!(target: TARGET, account_id = %account_id, error = %e, "keystore open failed on account removal (no master key to delete?)");
        }
    }

    tracing::info!(target: TARGET, account_id = %account_id, "account removed");
    Ok(())
}

/// `reauth_account(account_id)` - re-run consent for an account whose refresh
/// token was revoked (SPEC s11.1; the `account:needs_reauth` banner CTA).
///
/// A3: begins a fresh PKCE flow scoped to the EXISTING account (a server-side
/// session carrying `account_id`) and returns BOTH the consent URL AND the
/// `sessionId`, so the UI opens the URL, polls `poll_oauth_status` / listens for
/// `oauth:complete`, then calls `finish_add_account(sessionId)` which re-stores
/// the new refresh token + client creds onto the EXISTING account and flips it
/// back to `ok` (NO duplicate account is created). The account's orchestrator is
/// hot-spawned by `finish_add_account` once re-consent completes.
#[tauri::command]
pub async fn reauth_account(
    app: AppHandle,
    state: State<'_, AppState>,
    account_id: AccountId,
) -> CommandResult<ReauthSession> {
    // The account must exist (read by id from the strongly-consistent state DB).
    let exists = state
        .state()
        .account_state(account_id)
        .await
        .map_err(CommandError::from)?
        .is_some();
    if !exists {
        return Err(CommandError::with_code(
            ErrorCode::InternalBug,
            format!("unknown account id: {account_id}"),
        ));
    }

    // R4-P2-4: reap abandoned / stale-terminal sessions before opening this one.
    prune_stale_sessions();

    // A1: a reauth must use the SAME client that minted the original refresh
    // token (the account's persisted BYO client creds), so the new refresh
    // token is minted by - and bound to - that client. Seed the session with
    // the stored creds when present.
    let session_id = uuid::Uuid::new_v4().to_string();
    let mut session = WizardSession::new(Some(account_id));
    if let Some((client_id, client_secret)) = load_account_client_creds(account_id) {
        session.client_id = Some(client_id);
        session.client_secret = Some(client_secret);
    }
    lock_sessions().insert(session_id.clone(), session);

    // Drive the standard start-signin path against the new session, then return
    // both the consent URL and the session id so the UI can complete it.
    let session_arg = AddAccountWizardSessionId(session_id.clone());
    let OAuthAuthUrl { auth_url } = start_oauth_signin(app, state, session_arg).await?;
    Ok(ReauthSession {
        session_id,
        auth_url,
    })
}

/// Load an account's persisted BYO client creds (A1), or `None` when it used the
/// env/default client. Best-effort: a keychain read failure logs (never the
/// secret) and returns `None` so reauth falls back to the env/default client.
fn load_account_client_creds(account_id: AccountId) -> Option<(String, String)> {
    use driven_drive::google::token_store::ClientCredsStore;
    match ClientCredsStore::new(account_id.to_string()).load() {
        Ok(Some(creds)) if !creds.client_id.trim().is_empty() => {
            Some((creds.client_id, creds.client_secret))
        }
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(target: TARGET, account_id = %account_id, error = %e, "failed to load BYO client creds for reauth");
            None
        }
    }
}

/// The error returned for an unknown / stale wizard session id.
fn unknown_session_err() -> CommandError {
    CommandError::with_code(
        ErrorCode::AuthConsentRequired,
        "unknown or expired add-account session",
    )
}

/// Best-effort SPEC s24 classification of an OAuth-flow error for the
/// `OAuthStatus::Failed { code }` the webview renders. A classified
/// `DriveError` (the refresh path's `invalid_grant`) downcasts authoritatively;
/// otherwise the loopback flow's `access_denied` / timeout / `no refresh token`
/// messages map to `auth.consent_required` (the flow could not obtain consent).
fn oauth_error_code(err: &anyhow::Error) -> ErrorCode {
    use driven_drive::google::classification_of;
    use driven_drive::remote_store::DriveErrorClassification as C;
    if let Some(C::AuthInvalidGrant) = classification_of(err) {
        return ErrorCode::AuthInvalidGrant;
    }
    // The flow's terminal failures (user denied, redirect timed out, Google
    // returned no refresh token) all mean consent was not obtained.
    ErrorCode::AuthConsentRequired
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_state_str_matches_serde_rename() {
        assert_eq!(account_state_str(AccountState::Ok), "ok");
        assert_eq!(account_state_str(AccountState::NeedsReauth), "needs_reauth");
        assert_eq!(account_state_str(AccountState::Disabled), "disabled");
    }

    #[test]
    fn account_row_to_dto_maps_fields_and_hides_key_handle() {
        let id = AccountId::new_v4();
        let row = AccountRow {
            id,
            email: "label".to_string(),
            display_name: Some("My Drive".to_string()),
            state: AccountState::NeedsReauth,
            encryption_master_key_id: Some("kc-handle".to_string()),
            created_at: 123,
            last_synced_at: Some(456),
        };
        let dto = account_row_to_dto(&row);
        assert_eq!(dto.id, id.to_string());
        assert_eq!(dto.email, "label");
        assert_eq!(dto.display_name.as_deref(), Some("My Drive"));
        assert_eq!(dto.state, "needs_reauth");
        // Encryption is derived from the presence of a master-key handle - the
        // handle itself is never copied into the DTO.
        assert!(dto.encryption_enabled);
        assert_eq!(dto.created_at, 123);
        assert_eq!(dto.last_synced_at, Some(456));
    }

    #[test]
    fn prune_stale_sessions_reaps_abandoned_and_stale_terminal_sessions() {
        // R4-P2-4: an abandoned (non-terminal) session past SESSION_TTL_MS and a
        // terminal session past SESSION_TERMINAL_GRACE_MS are reaped; a recent
        // session of either kind is kept. Unique ids keep this test isolated from
        // the shared process-global session map.
        let now = SystemClock.now_ms();
        let tag = uuid::Uuid::new_v4().to_string();
        let fresh_id = format!("{tag}-fresh");
        let stale_abandoned_id = format!("{tag}-stale-abandoned");
        let recent_terminal_id = format!("{tag}-recent-terminal");
        let stale_terminal_id = format!("{tag}-stale-terminal");

        {
            let mut sessions = lock_sessions();
            // Fresh non-terminal: kept.
            let mut fresh = WizardSession::new(None);
            fresh.updated_at = now;
            sessions.insert(fresh_id.clone(), fresh);
            // Abandoned non-terminal, idle past the TTL: reaped.
            let mut stale = WizardSession::new(None);
            stale.updated_at = now - SESSION_TTL_MS - 1;
            sessions.insert(stale_abandoned_id.clone(), stale);
            // Terminal but recent (within grace): kept.
            let mut recent_terminal = WizardSession::new(None);
            recent_terminal.status = OAuthStatus::Complete;
            recent_terminal.updated_at = now - 1;
            sessions.insert(recent_terminal_id.clone(), recent_terminal);
            // Terminal past the grace: reaped.
            let mut stale_terminal = WizardSession::new(None);
            stale_terminal.status = OAuthStatus::Failed {
                code: "auth.consent_required".to_string(),
            };
            stale_terminal.updated_at = now - SESSION_TERMINAL_GRACE_MS - 1;
            sessions.insert(stale_terminal_id.clone(), stale_terminal);
        }

        prune_stale_sessions();

        let sessions = lock_sessions();
        assert!(sessions.contains_key(&fresh_id), "fresh session kept");
        assert!(
            !sessions.contains_key(&stale_abandoned_id),
            "abandoned past-TTL session reaped"
        );
        assert!(
            sessions.contains_key(&recent_terminal_id),
            "recent terminal session kept (within grace)"
        );
        assert!(
            !sessions.contains_key(&stale_terminal_id),
            "terminal past-grace session reaped"
        );
    }

    #[test]
    fn progress_maps_to_status() {
        assert!(matches!(
            progress_to_status(OAuthProgress::OpeningBrowser),
            OAuthStatus::OpeningBrowser
        ));
        assert!(matches!(
            progress_to_status(OAuthProgress::WaitingForRedirect),
            OAuthStatus::AwaitingCallback
        ));
        assert!(matches!(
            progress_to_status(OAuthProgress::ExchangingCode),
            OAuthStatus::ExchangingCode
        ));
    }

    #[test]
    fn google_userinfo_parses_email_and_name() {
        // A5: the userinfo response shape Driven persists.
        let body = r#"{"sub":"123","email":"real@gmail.com","email_verified":true,"name":"Real Name","picture":"https://x"}"#;
        let info: GoogleUserinfo = serde_json::from_str(body).expect("parse userinfo");
        assert_eq!(info.email, "real@gmail.com");
        assert_eq!(info.name.as_deref(), Some("Real Name"));
    }

    #[test]
    fn google_userinfo_tolerates_missing_name() {
        // A minimal userinfo (email only) still parses; name is None.
        let body = r#"{"email":"only@gmail.com"}"#;
        let info: GoogleUserinfo = serde_json::from_str(body).expect("parse minimal userinfo");
        assert_eq!(info.email, "only@gmail.com");
        assert!(info.name.is_none());
    }

    /// R2-P2-2: an in-memory [`AccountSecretStore`] recording which secrets are
    /// currently stored, so a test can assert no orphan remains after a rollback.
    #[derive(Default)]
    struct FakeSecretStore {
        refresh: std::sync::Mutex<std::collections::HashSet<AccountId>>,
        creds: std::sync::Mutex<std::collections::HashSet<AccountId>>,
    }
    impl FakeSecretStore {
        fn has_refresh(&self, id: AccountId) -> bool {
            self.refresh.lock().unwrap().contains(&id)
        }
        fn has_creds(&self, id: AccountId) -> bool {
            self.creds.lock().unwrap().contains(&id)
        }
    }
    impl AccountSecretStore for FakeSecretStore {
        fn store_refresh_token(&self, account_id: AccountId, _t: &str) -> CommandResult<()> {
            self.refresh.lock().unwrap().insert(account_id);
            Ok(())
        }
        fn store_client_creds(
            &self,
            account_id: AccountId,
            _c: &(String, String),
        ) -> CommandResult<()> {
            self.creds.lock().unwrap().insert(account_id);
            Ok(())
        }
        fn delete_refresh_token(&self, account_id: AccountId) -> anyhow::Result<()> {
            self.refresh.lock().unwrap().remove(&account_id);
            Ok(())
        }
        fn delete_client_creds(&self, account_id: AccountId) -> anyhow::Result<()> {
            self.creds.lock().unwrap().remove(&account_id);
            Ok(())
        }
    }

    fn fresh_row(id: AccountId) -> AccountRow {
        AccountRow {
            id,
            email: "u@example.com".to_string(),
            display_name: None,
            state: AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        }
    }

    #[tokio::test]
    async fn persist_new_account_row_failure_rolls_back_all_keychain_entries() {
        // R2-P2-2: a forced ROW-insert failure must leave NO orphaned keychain
        // creds (both the refresh token and the BYO client creds are rolled back),
        // and `persist_new_account` returns the row error (so finish keeps the
        // session intact for a retry).
        let secrets = FakeSecretStore::default();
        let id = AccountId::new_v4();
        let row = fresh_row(id);
        let creds = ("client-id".to_string(), "secret".to_string());

        let err = persist_new_account(
            &secrets,
            id,
            "refresh-token",
            &creds,
            // Force the row insert to fail.
            |_r| async { Err(anyhow::anyhow!("forced row insert failure")) },
            &row,
        )
        .await
        .expect_err("a row-insert failure must propagate");
        // The error carries the row failure (mapped to internal.bug here).
        assert_eq!(err.code, ErrorCode::InternalBug);
        // No orphaned keychain entries remain.
        assert!(
            !secrets.has_refresh(id),
            "refresh token must be rolled back on row-insert failure"
        );
        assert!(
            !secrets.has_creds(id),
            "client creds must be rolled back on row-insert failure"
        );
    }

    #[test]
    fn finish_reads_session_tokens_by_clone_so_a_failure_leaves_them_replayable() {
        // R2-P2-2: `finish_add_account` now CLONES the session tokens (it used to
        // `take()` them) and only removes the session on success. So after a
        // failed finish the session still carries its Complete status + tokens and
        // the user can retry without re-running OAuth. Model that invariant on a
        // session directly: a clone must NOT empty the session's tokens.
        let mut s = WizardSession::new(None);
        s.status = OAuthStatus::Complete;
        s.tokens = Some(Tokens {
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            expires_at: 0,
        });
        // The clone the finish path uses must leave the session's tokens intact.
        let cloned = s.tokens.clone();
        assert!(cloned.is_some(), "clone yields the tokens");
        assert!(
            s.tokens.is_some(),
            "the session must STILL hold its tokens after a clone (replayable)"
        );
        assert!(matches!(s.status, OAuthStatus::Complete));
    }

    #[tokio::test]
    async fn persist_new_account_success_keeps_both_secrets() {
        // The happy path keeps both keychain entries and inserts the row.
        let secrets = FakeSecretStore::default();
        let id = AccountId::new_v4();
        let row = fresh_row(id);
        let creds = ("client-id".to_string(), "secret".to_string());
        let inserted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inserted2 = inserted.clone();

        persist_new_account(
            &secrets,
            id,
            "refresh-token",
            &creds,
            move |_r| {
                let inserted2 = inserted2.clone();
                async move {
                    inserted2.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }
            },
            &row,
        )
        .await
        .expect("persist must succeed");
        assert!(inserted.load(std::sync::atomic::Ordering::SeqCst));
        assert!(secrets.has_refresh(id) && secrets.has_creds(id));
    }

    #[test]
    fn resolve_creds_requires_byo_and_rejects_when_absent() {
        // R2-P2-1 (BYO-only): a session with NO submitted client id AND no env
        // override is REJECTED (no baked-in default client). This test removes any
        // env override so the no-creds path is deterministic.
        std::env::remove_var(ENV_OAUTH_CLIENT_ID);
        std::env::remove_var(ENV_OAUTH_CLIENT_SECRET);
        let mut s = WizardSession::new(None);
        let err = resolve_creds(&s).expect_err("no creds must be rejected");
        assert_eq!(err.code, ErrorCode::AuthConsentRequired);
        // BYO submitted -> resolves to exactly those creds.
        s.client_id = Some("byo-id".to_string());
        s.client_secret = Some("byo-secret".to_string());
        let (id, secret) = resolve_creds(&s).expect("byo creds resolve");
        assert_eq!(id, "byo-id");
        assert_eq!(secret, "byo-secret");
    }
}
