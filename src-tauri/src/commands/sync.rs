//! Sync IPC commands (SPEC s11.3): `sync_now`, `pause_sync`, `resume_sync`,
//! `get_sync_status`.
//!
//! Each is a `#[tauri::command]` over `State<AppState>` that drives the
//! per-account [`Orchestrator`](driven_core::orchestrator::Orchestrator)
//! control surface (`trigger` / `set_paused` / `state`). The richer
//! account/source/restore/settings IPC (SPEC s11.1/s11.2/s11.5/s11.6) is M6.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use driven_core::orchestrator::{Orchestrator, TickSource};
use driven_core::types::{AccountId, OrchestratorState, SourceId};

use crate::app_state::AppState;
use crate::commands::{CommandError, CommandResult};

/// The global sync status returned by [`get_sync_status`] (SPEC s11.3 /
/// s11.7 `GlobalSyncStatus`).
///
/// M5 scaffold shape: the aggregate state across accounts. M6 expands this to
/// the full DTO (per-account states, last-sync timestamps, queue depth). Kept
/// minimal-but-real so `get_sync_status` and the `sync:status_changed` event
/// have a concrete payload to compile against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalSyncStatus {
    /// The orchestrator states, one per account (keyed by account id string).
    pub accounts: Vec<AccountSyncStatus>,
}

/// One account's sync state within [`GlobalSyncStatus`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSyncStatus {
    /// The account id (string form of `AccountId`).
    pub account_id: String,
    /// The orchestrator's current state (SPEC s5 machine).
    pub state: OrchestratorState,
}

/// `sync_now(source_id?)` - trigger an out-of-band cycle now (SPEC s11.3).
///
/// `source_id = None` triggers every account's orchestrator; `Some` scopes to
/// the OWNING account (the orchestrator ticks all its enabled sources -
/// per-source scoping is an M6 refinement). The owning account is resolved
/// from the state DB; an unknown source id is a command error rather than a
/// silent no-op (so a stale webview surfaces the problem).
///
/// Each [`Orchestrator::trigger`] coalesces into the run loop's capacity-1
/// trigger channel, so spamming "Sync now" never stacks concurrent cycles.
#[tauri::command]
pub async fn sync_now(
    state: State<'_, AppState>,
    source_id: Option<SourceId>,
) -> CommandResult<()> {
    match source_id {
        None => {
            for (_id, handle) in state.accounts() {
                handle.orchestrator.trigger(TickSource::Manual).await;
            }
            Ok(())
        }
        Some(source_id) => {
            // Resolve the source's owning account from the state DB. Read by
            // id from the strongly-consistent `backup_sources` table, not a
            // search, so a just-added source is visible.
            let sources = state
                .state()
                .list_sources()
                .await
                .map_err(CommandError::from)?;
            let account_id = sources
                .iter()
                .find(|s| s.id == source_id)
                .map(|s| s.account_id)
                .ok_or_else(|| CommandError::new(format!("unknown source id: {source_id}")))?;
            let handle = state.account(account_id).ok_or_else(|| {
                CommandError::new(format!("no running orchestrator for account {account_id}"))
            })?;
            handle.orchestrator.trigger(TickSource::Manual).await;
            Ok(())
        }
    }
}

/// `pause_sync(duration_secs?)` - pause sync (SPEC s11.3).
///
/// `duration_secs = Some` is a timed pause (e.g. the tray "Pause for 30m");
/// `None` is pause-until-manual-resume. Sets the manual-pause signal on every
/// account orchestrator (DESIGN s5.7: manual pause persists across restarts).
///
/// C5-P2-1: a timed pause spawns a CANCELLABLE auto-resume timer. Each
/// pause/resume bumps a per-account pause "generation"; the timer captures the
/// generation at arm time and only auto-resumes if it STILL matches when it
/// wakes. So a later `pause_sync(None)` (indefinite) issued before the old
/// timer fires bumps the generation and CANCELS the stale timer's auto-resume -
/// the indefinite pause is no longer clobbered.
#[tauri::command]
pub async fn pause_sync(
    app: AppHandle,
    state: State<'_, AppState>,
    duration_secs: Option<u64>,
) -> CommandResult<()> {
    // Snapshot (account_id, orchestrator) so the resume timer does not need to
    // borrow `State` (which is not `'static`); bump each account's pause
    // generation so any in-flight timer is superseded.
    let entries: Vec<(AccountId, Arc<dyn Orchestrator>)> = state
        .accounts()
        .map(|(id, handle)| (*id, handle.orchestrator.clone()))
        .collect();

    let mut tokens: Vec<(AccountId, Arc<dyn Orchestrator>, u64)> =
        Vec::with_capacity(entries.len());
    for (id, orch) in entries {
        orch.set_paused(true).await;
        let token = state.bump_pause_generation(id);
        tokens.push((id, orch, token));
    }

    if let Some(secs) = duration_secs {
        // Detached timed-resume: sleep the window, then clear the manual pause
        // ONLY for accounts whose pause generation is unchanged (no newer
        // pause/resume superseded this timer). `tokio::time::sleep` (real
        // wall-clock UI affordance) keeps the task off the IPC path so the
        // command returns immediately.
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            let Some(state) = app.try_state::<AppState>() else {
                return;
            };
            for (id, orch, token) in &tokens {
                if state.pause_generation_matches(*id, *token) {
                    orch.set_paused(false).await;
                } else {
                    tracing::debug!(
                        target: "driven::app::sync",
                        account_id = %id,
                        "timed-resume superseded by a newer pause/resume; not auto-resuming"
                    );
                }
            }
        });
    }

    Ok(())
}

/// `resume_sync()` - clear the manual pause on every account (SPEC s11.3).
///
/// C5-P2-1: bumps each account's pause generation too, so an outstanding timed
/// auto-resume timer for that account is cancelled (the manual resume already
/// did its job; the stale timer must not later re-resume a fresh pause).
#[tauri::command]
pub async fn resume_sync(state: State<'_, AppState>) -> CommandResult<()> {
    for (id, handle) in state.accounts() {
        handle.orchestrator.set_paused(false).await;
        let _ = state.bump_pause_generation(*id);
    }
    Ok(())
}

/// `get_sync_status()` - snapshot the aggregate sync state (SPEC s11.3).
///
/// Reads each account orchestrator's current [`OrchestratorState`] into the
/// [`GlobalSyncStatus`] DTO (one [`AccountSyncStatus`] per account).
#[tauri::command]
pub async fn get_sync_status(state: State<'_, AppState>) -> CommandResult<GlobalSyncStatus> {
    let mut accounts = Vec::new();
    for (id, handle) in state.accounts() {
        accounts.push(AccountSyncStatus {
            account_id: id.to_string(),
            state: handle.orchestrator.state().await,
        });
    }
    Ok(GlobalSyncStatus { accounts })
}
