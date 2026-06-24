//! Account IPC commands (SPEC s11.1).
//!
//! The add-account wizard (DESIGN s8.5) + the Accounts settings tab (DESIGN
//! s8.2) drive these. Each is a `#[tauri::command]` over `State<AppState>`.
//!
//! M6 SCAFFOLD: command bodies are `todo!()` - the accounts implementer fills
//! them in. The signatures, the [`crate::commands::dtos`] shapes, and the
//! registration in `lib.rs`'s `invoke_handler` are the frozen contract. The
//! OAuth flow these wrap is [`driven_drive::google::oauth::run_pkce_loopback_flow`]
//! (the wizard drives the loopback + emits `oauth:complete` per SPEC s11.7).

use tauri::State;

use driven_core::types::AccountId;

use crate::app_state::AppState;
use crate::commands::dtos::{
    AccountDto, AddAccountWizardSessionId, OAuthAuthUrl, OAuthStatus, SessionId,
};
use crate::commands::CommandResult;

/// `list_accounts()` - every configured Google account (SPEC s11.1).
///
/// Reads `accounts` from the strongly-consistent state DB and maps each
/// [`driven_core::state::AccountRow`] to an [`AccountDto`] (the keychain
/// master-key handle is never exposed to the webview).
#[tauri::command]
pub async fn list_accounts(state: State<'_, AppState>) -> CommandResult<Vec<AccountDto>> {
    let _ = state;
    todo!("M6 accounts: map StateRepo::list_accounts() rows to AccountDto")
}

/// `begin_add_account_wizard()` - open a new add-account session (SPEC s11.1).
///
/// Allocates a server-side wizard session (holding the in-flight OAuth state)
/// and returns its opaque [`AddAccountWizardSessionId`]; the wizard threads
/// this id through the OAuth-step commands below.
#[tauri::command]
pub async fn begin_add_account_wizard(
    state: State<'_, AppState>,
) -> CommandResult<AddAccountWizardSessionId> {
    let _ = state;
    todo!("M6 accounts: allocate a wizard session + return its id")
}

/// `submit_oauth_credentials(session, client_id, client_secret)` - record the
/// user's BYO OAuth client credentials for the session (SPEC s11.1, DESIGN
/// s8.5 step 2 BYO-credentials walkthrough).
#[tauri::command]
pub async fn submit_oauth_credentials(
    state: State<'_, AppState>,
    session: SessionId,
    client_id: String,
    client_secret: String,
) -> CommandResult<()> {
    let _ = (state, session, client_id, client_secret);
    todo!("M6 accounts: store the BYO client_id/secret on the wizard session")
}

/// `start_oauth_signin(session)` - begin the PKCE loopback flow and return the
/// Google consent URL to open (SPEC s11.1).
///
/// Spawns [`driven_drive::google::oauth::run_pkce_loopback_flow`] driving the
/// loopback listener; progress is observable via `poll_oauth_status` and the
/// terminal `oauth:complete` event (SPEC s11.7).
#[tauri::command]
pub async fn start_oauth_signin(
    state: State<'_, AppState>,
    session: SessionId,
) -> CommandResult<OAuthAuthUrl> {
    let _ = (state, session);
    todo!("M6 accounts: run_pkce_loopback_flow + return the consent auth_url")
}

/// `poll_oauth_status(session)` - poll the in-flight OAuth flow (SPEC s11.1).
///
/// The browser hits the loopback callback; the Rust side resolves the in-flight
/// session and this surfaces its current [`OAuthStatus`] (mirroring
/// [`driven_drive::google::oauth::OAuthProgress`] plus terminal states).
#[tauri::command]
pub async fn poll_oauth_status(
    state: State<'_, AppState>,
    session: SessionId,
) -> CommandResult<OAuthStatus> {
    let _ = (state, session);
    todo!("M6 accounts: surface the wizard session's OAuthProgress as OAuthStatus")
}

/// `finish_add_account(session, display_name?)` - persist the account once the
/// OAuth flow completed (SPEC s11.1).
///
/// Stores the refresh token in the keychain, writes the `accounts` row, spawns
/// the account's orchestrator, and returns the new [`AccountDto`].
#[tauri::command]
pub async fn finish_add_account(
    state: State<'_, AppState>,
    session: SessionId,
    display_name: Option<String>,
) -> CommandResult<AccountDto> {
    let _ = (state, session, display_name);
    todo!("M6 accounts: persist account + keychain token, spawn orchestrator, return AccountDto")
}

/// `remove_account(account_id, delete_remote)` - remove an account (SPEC s11.1).
///
/// Shuts down the account's orchestrator, deletes its `accounts` row (cascading
/// sources + file_state), and wipes the keychain token. When `delete_remote` is
/// set, also trashes the account's backed-up Drive content.
#[tauri::command]
pub async fn remove_account(
    state: State<'_, AppState>,
    account_id: AccountId,
    delete_remote: bool,
) -> CommandResult<()> {
    let _ = (state, account_id, delete_remote);
    todo!("M6 accounts: shutdown orchestrator, delete account row + keychain token, optional remote trash")
}

/// `reauth_account(account_id)` - re-run consent for an account whose refresh
/// token was revoked (SPEC s11.1; the `account:needs_reauth` banner CTA).
///
/// Begins a fresh PKCE flow scoped to the existing account and returns the
/// consent URL; on success the account state returns to `ok`.
#[tauri::command]
pub async fn reauth_account(
    state: State<'_, AppState>,
    account_id: AccountId,
) -> CommandResult<OAuthAuthUrl> {
    let _ = (state, account_id);
    todo!("M6 accounts: begin a re-consent PKCE flow for the existing account")
}
