//! Sync IPC commands (SPEC s11.3): `sync_now`, `pause_sync`, `resume_sync`,
//! `get_sync_status`.
//!
//! Each is a `#[tauri::command]` over `State<AppState>` that drives the
//! per-account [`Orchestrator`](driven_core::orchestrator::Orchestrator)
//! control surface (`trigger` / `set_paused` / `state`). The richer
//! account/source/restore/settings IPC (SPEC s11.1/s11.2/s11.5/s11.6) is M6.

use serde::{Deserialize, Serialize};
use tauri::State;

use driven_core::types::{OrchestratorState, SourceId};

use crate::app_state::AppState;
use crate::commands::CommandResult;

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
/// the owning account (the orchestrator ticks all its enabled sources -
/// per-source scoping is an M6 refinement).
///
/// TODO(M5): resolve the target orchestrator(s) from `state` and call
/// `orchestrator.trigger(TickSource::Manual).await` on each.
#[tauri::command]
pub async fn sync_now(
    state: State<'_, AppState>,
    source_id: Option<SourceId>,
) -> CommandResult<()> {
    let _ = (state, source_id);
    todo!("M5: trigger(TickSource::Manual) on the target account orchestrator(s)")
}

/// `pause_sync(duration_secs?)` - pause sync (SPEC s11.3).
///
/// `duration_secs = Some` is a timed pause (e.g. the tray "Pause for 30m");
/// `None` is pause-until-manual-resume. Sets the manual-pause signal on every
/// account orchestrator (DESIGN s5.7: manual pause persists across restarts).
///
/// TODO(M5): call `orchestrator.set_paused(true).await` on each account; for a
/// timed pause, schedule a resume after `duration_secs` (clock-driven).
#[tauri::command]
pub async fn pause_sync(
    state: State<'_, AppState>,
    duration_secs: Option<u64>,
) -> CommandResult<()> {
    let _ = (state, duration_secs);
    todo!("M5: set_paused(true) on every account orchestrator; arm timed resume if duration_secs")
}

/// `resume_sync()` - clear the manual pause on every account (SPEC s11.3).
///
/// TODO(M5): call `orchestrator.set_paused(false).await` on each account.
#[tauri::command]
pub async fn resume_sync(state: State<'_, AppState>) -> CommandResult<()> {
    let _ = state;
    todo!("M5: set_paused(false) on every account orchestrator")
}

/// `get_sync_status()` - snapshot the aggregate sync state (SPEC s11.3).
///
/// TODO(M5): for each account, read `orchestrator.state().await` and assemble
/// a [`GlobalSyncStatus`].
#[tauri::command]
pub async fn get_sync_status(state: State<'_, AppState>) -> CommandResult<GlobalSyncStatus> {
    let _ = state;
    todo!("M5: collect orchestrator.state() per account into GlobalSyncStatus")
}
