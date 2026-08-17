//! Sync IPC commands (SPEC s11.3): `sync_now`, `pause_sync`, `resume_sync`,
//! `get_pause_state`, `get_sync_status`.
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
use driven_core::state::StateRepo;
use driven_core::time::{Clock, SystemClock};
use driven_core::types::{AccountId, OrchestratorState, SourceId};

use crate::app_state::AppState;
use crate::commands::{CommandError, CommandResult};

/// Settings key holding the DURABLE manual-pause state.
///
/// DESIGN s5.7 says the manual pause "persists across restarts"; before this it
/// lived only in an in-memory watch cell plus a detached timer, so a restart
/// silently resumed backups the user had paused. The value is a serialized
/// [`PauseState`]; the key is absent (or `null`) when sync is not manually
/// paused.
const PAUSE_STATE_KEY: &str = "manual_pause";

/// The active manual pause (SPEC s11.3 `get_pause_state`).
///
/// Serializes as `{"kind":"timed","until_ms":1234}` / `{"kind":"indefinite"}` so
/// the webview can render both the countdown ("Backups paused - 27m left") and
/// the open-ended banner ("Backups paused indefinitely") off one payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PauseState {
    /// A timed pause (the tray's "Pause for 30 minutes"): auto-resumes at
    /// `until_ms` (unix ms).
    Timed {
        /// Wall-clock unix ms at which the pause auto-resumes.
        until_ms: i64,
    },
    /// Paused until the user explicitly resumes. No timer is armed.
    Indefinite,
}

impl PauseState {
    /// Has this pause already elapsed at `now_ms`? Always `false` for an
    /// indefinite pause.
    #[must_use]
    fn is_expired(self, now_ms: i64) -> bool {
        match self {
            Self::Timed { until_ms } => until_ms <= now_ms,
            Self::Indefinite => false,
        }
    }

    /// How long is left before the auto-resume fires? `None` for an indefinite
    /// pause (no timer). Saturates at zero rather than going negative.
    #[must_use]
    fn remaining(self, now_ms: i64) -> Option<Duration> {
        match self {
            Self::Timed { until_ms } => Some(Duration::from_millis(
                u64::try_from(until_ms.saturating_sub(now_ms)).unwrap_or(0),
            )),
            Self::Indefinite => None,
        }
    }
}

/// Read the persisted [`PauseState`], treating an absent key, a `null`, and an
/// unparseable value alike as "not paused".
///
/// A decode failure is logged and swallowed rather than propagated: a corrupt
/// pause record must never wedge boot or the status IPC - the safe direction is
/// "not paused", which the user can always re-establish with one click.
async fn read_pause_state(state: &dyn StateRepo) -> Option<PauseState> {
    let raw = match state.get_setting(PAUSE_STATE_KEY).await {
        Ok(raw) => raw?,
        Err(err) => {
            tracing::warn!(target: "driven::app::sync", %err, "failed to read the persisted pause state; treating as not paused");
            return None;
        }
    };
    if raw.is_null() {
        return None;
    }
    match serde_json::from_value::<PauseState>(raw.clone()) {
        Ok(pause) => Some(pause),
        Err(err) => {
            tracing::warn!(target: "driven::app::sync", %err, %raw, "unparseable persisted pause state; treating as not paused");
            None
        }
    }
}

/// Persist (`Some`) or clear (`None`) the durable pause state. Best-effort: a
/// write failure is logged, never propagated - the in-memory pause has already
/// taken effect and failing the IPC call would tell the user the pause did not
/// happen when it did.
async fn write_pause_state(state: &dyn StateRepo, pause: Option<PauseState>) {
    let value = match pause {
        Some(pause) => serde_json::to_value(pause).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    };
    if let Err(err) = state.set_setting(PAUSE_STATE_KEY, &value).await {
        tracing::warn!(target: "driven::app::sync", %err, "failed to persist the pause state; it will not survive a restart");
    }
}

/// Emit `sync:pause_changed` so the webview's paused banner appears/disappears
/// live. Best-effort - a webview that missed the event still reconciles via
/// `get_pause_state` on its next mount.
fn emit_pause_changed(app: &AppHandle, pause: Option<PauseState>) {
    if let Err(err) = crate::events::emit_pause_changed(app, pause) {
        tracing::debug!(target: "driven::app::sync", %err, "emit sync:pause_changed failed");
    }
}

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

/// `sync_now(source_id?, bypass_gates?)` - trigger an out-of-band cycle now
/// (SPEC s11.3).
///
/// `source_id = None` triggers every account's orchestrator; `Some` scopes to
/// the OWNING account (the orchestrator ticks all its enabled sources -
/// per-source scoping is an M6 refinement). The owning account is resolved
/// from the state DB; an unknown source id is a command error rather than a
/// silent no-op (so a stale webview surfaces the problem).
///
/// `bypass_gates = Some(true)` (spec 2026-08-01, unified pause/status banner:
/// the banner's "Sync now" while gated) arms
/// [`Orchestrator::bypass_gates_once`] on every TARGETED account BEFORE the
/// manual trigger, so the very next cycle skips a Metered/Battery/Schedule
/// gate that would otherwise re-refuse it. It never overrides the manual
/// pause, the network family, or the Drive circuit breaker (see
/// `bypass_gates_once`'s doc). `None`/`Some(false)` is the unchanged
/// behaviour.
///
/// Issue #303: each trigger lands on the account's visible PENDING-WORK QUEUE,
/// coalescing per (kind, source) - so spamming "Sync now" never stacks
/// concurrent cycles, while a manual click no longer vanishes into an unrelated
/// pending watcher tick the way the old capacity-1 channel let it.
#[tauri::command]
pub async fn sync_now(
    state: State<'_, AppState>,
    source_id: Option<SourceId>,
    bypass_gates: Option<bool>,
) -> CommandResult<()> {
    sync_now_impl(state.inner(), source_id, bypass_gates).await
}

/// The testable core of [`sync_now`], split out so it is unit-testable
/// against a real `AppState` (the `#[tauri::command]` itself needs a Tauri
/// `State`, which cannot be constructed outside a running app - mirrors
/// `restore::build_restore_plans`).
async fn sync_now_impl(
    state: &AppState,
    source_id: Option<SourceId>,
    bypass_gates: Option<bool>,
) -> CommandResult<()> {
    match source_id {
        None => {
            for (_id, handle) in state.accounts() {
                if bypass_gates == Some(true) {
                    handle.orchestrator.bypass_gates_once();
                }
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
            if bypass_gates == Some(true) {
                handle.orchestrator.bypass_gates_once();
            }
            // Issue #303: attribute the request to its source so the work queue
            // can name it ("Back up now - Music") and coalesce repeat clicks per
            // source rather than folding unrelated requests together.
            handle
                .orchestrator
                .trigger_source(TickSource::Manual, Some(source_id))
                .await;
            Ok(())
        }
    }
}

/// Set the manual pause on every account orchestrator and, for a timed pause,
/// arm the cancellable auto-resume timer. Shared by the `pause_sync` IPC and the
/// boot-time restore of a persisted pause, so both paths arm exactly the same
/// machinery (an indefinite pause arms no timer at all).
///
/// C5-P2-1: each pause/resume bumps a per-account pause "generation"; the timer
/// captures the generation at arm time and only auto-resumes if it STILL matches
/// when it wakes. So a later `pause_sync(None)` (indefinite) issued before the
/// old timer fires bumps the generation and CANCELS the stale timer's
/// auto-resume - the indefinite pause is no longer clobbered.
///
/// When the timer DOES auto-resume, it also clears the durable pause record and
/// emits `sync:pause_changed`, so the banner disappears on its own without the
/// user touching anything.
async fn apply_pause(app: &AppHandle, state: &AppState, pause: PauseState) {
    // Snapshot (account_id, orchestrator) so the resume timer does not need to
    // borrow `State` (which is not `'static`); bump each account's pause
    // generation so any in-flight timer is superseded.
    let entries: Vec<(AccountId, Arc<dyn Orchestrator>)> = state
        .accounts()
        .into_iter()
        .map(|(id, handle)| (id, handle.orchestrator.clone()))
        .collect();

    let mut tokens: Vec<(AccountId, Arc<dyn Orchestrator>, u64)> =
        Vec::with_capacity(entries.len());
    for (id, orch) in entries {
        orch.set_paused(true).await;
        let token = state.bump_pause_generation(id);
        tokens.push((id, orch, token));
    }

    let Some(delay) = pause.remaining(SystemClock.now_ms()) else {
        // Indefinite: no timer, so nothing auto-resumes it.
        return;
    };

    // Detached timed-resume: sleep the remaining window, then clear the manual
    // pause ONLY for accounts whose pause generation is unchanged (no newer
    // pause/resume superseded this timer). `tokio::time::sleep` (real
    // wall-clock UI affordance) keeps the task off the IPC path so the command
    // returns immediately.
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(delay).await;
        let Some(state) = app.try_state::<AppState>() else {
            return;
        };
        let mut resumed_any = false;
        for (id, orch, token) in &tokens {
            if state.pause_generation_matches(*id, *token) {
                orch.set_paused(false).await;
                resumed_any = true;
            } else {
                tracing::debug!(
                    target: "driven::app::sync",
                    account_id = %id,
                    "timed-resume superseded by a newer pause/resume; not auto-resuming"
                );
            }
        }
        // Only clear the durable record when this timer actually did the
        // resuming. If every account was superseded, a NEWER pause owns the
        // record and clearing it here would erase a pause the user just set.
        // A no-account install (quiesced boot) has nothing to supersede it, so
        // the expired record is cleared there too.
        if resumed_any || tokens.is_empty() {
            write_pause_state(state.state().as_ref(), None).await;
            emit_pause_changed(&app, None);
        }
    });
}

/// `pause_sync(duration_secs?)` - pause sync (SPEC s11.3).
///
/// `duration_secs = Some` is a timed pause (the tray "Pause for 30 minutes");
/// `None` is an INDEFINITE pause, held until the user resumes (the tray "Pause
/// until I resume" and the paused banner's Resume button).
///
/// The pause is applied in memory first (so the effect is immediate), then
/// persisted so it survives a restart per DESIGN s5.7, then broadcast on
/// `sync:pause_changed` so the banner appears without a refresh.
#[tauri::command]
pub async fn pause_sync(
    app: AppHandle,
    state: State<'_, AppState>,
    duration_secs: Option<u64>,
) -> CommandResult<()> {
    let pause = match duration_secs {
        Some(secs) => PauseState::Timed {
            until_ms: SystemClock
                .now_ms()
                .saturating_add(i64::try_from(secs.saturating_mul(1_000)).unwrap_or(i64::MAX)),
        },
        None => PauseState::Indefinite,
    };
    apply_pause(&app, &state, pause).await;
    write_pause_state(state.state().as_ref(), Some(pause)).await;
    emit_pause_changed(&app, Some(pause));
    Ok(())
}

/// `resume_sync()` - clear the manual pause on every account (SPEC s11.3).
///
/// C5-P2-1: bumps each account's pause generation too, so an outstanding timed
/// auto-resume timer for that account is cancelled (the manual resume already
/// did its job; the stale timer must not later re-resume a fresh pause). Also
/// clears the durable record and broadcasts `sync:pause_changed` so the banner
/// disappears immediately.
#[tauri::command]
pub async fn resume_sync(app: AppHandle, state: State<'_, AppState>) -> CommandResult<()> {
    for (id, handle) in state.accounts() {
        handle.orchestrator.set_paused(false).await;
        let _ = state.bump_pause_generation(id);
    }
    write_pause_state(state.state().as_ref(), None).await;
    emit_pause_changed(&app, None);
    Ok(())
}

/// `get_pause_state()` - the active manual pause, or `null` when sync is not
/// manually paused (SPEC s11.3).
///
/// The webview hydrates its banner from this on mount, so a pause set before the
/// window opened (or in a previous run) still shows. An already-elapsed timed
/// pause reads as "not paused" rather than as a stale countdown.
#[tauri::command]
pub async fn get_pause_state(state: State<'_, AppState>) -> CommandResult<Option<PauseState>> {
    let pause = read_pause_state(state.state().as_ref()).await;
    Ok(pause.filter(|p| !p.is_expired(SystemClock.now_ms())))
}

/// Re-apply a manual pause persisted by a previous run (DESIGN s5.7 "manual
/// pause persists across restarts"), called once from the boot path after the
/// orchestrators are managed.
///
/// An indefinite pause is re-applied as-is. A timed pause is re-applied only for
/// the time it has LEFT (so "pause 30m" then a restart 25 minutes later resumes
/// 5 minutes later, not 30); one that already elapsed while the app was closed
/// is cleared instead of being re-armed. Best-effort throughout: a boot must
/// never fail because of the pause record.
pub async fn restore_persisted_pause(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let Some(pause) = read_pause_state(state.state().as_ref()).await else {
        return;
    };
    if pause.is_expired(SystemClock.now_ms()) {
        tracing::info!(target: "driven::app::sync", ?pause, "persisted timed pause already elapsed; clearing it and starting unpaused");
        write_pause_state(state.state().as_ref(), None).await;
        return;
    }
    tracing::info!(target: "driven::app::sync", ?pause, "re-applying the manual pause persisted by a previous run");
    apply_pause(app, &state, pause).await;
    emit_pause_changed(app, Some(pause));
}

/// `io_throughput_series()` - the trailing window of live disk/network
/// throughput samples (2026-08-14 follow-up), for the Activity dashboard's
/// split graphs' initial load; live updates ride the `sync:io_throughput`
/// event. Always available (the quiesced boot path serves an all-zero hub).
#[tauri::command]
pub async fn io_throughput_series(
    state: State<'_, AppState>,
) -> CommandResult<crate::iostat_hub::IoThroughputSeriesDto> {
    Ok(state.iostat_hub().series())
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

/// `get_work_queue()` - snapshot every account's pending-work queue (issue
/// #303).
///
/// One [`QueueSnapshot`](driven_core::queue::QueueSnapshot) per account that
/// runs a queue, in account order. This is the HYDRATION path: the live updates
/// ride `queue:changed`, and this fills the panel for a webview that attached
/// after the last change (or was never open).
#[tauri::command]
pub async fn get_work_queue(
    state: State<'_, AppState>,
) -> CommandResult<Vec<driven_core::queue::QueueSnapshot>> {
    Ok(state
        .accounts()
        .into_iter()
        .filter_map(|(_id, handle)| handle.orchestrator.work_queue())
        .collect())
}

/// `cancel_work_item(account_id, item_id)` - cancel ONE queued item (issue
/// #303).
///
/// A PENDING item is dropped. The RUNNING item is stopped GRACEFULLY: no new
/// ops are dispatched, the ops already in flight finish and durably commit, and
/// the remainder is re-planned by a later scan. Nothing is ever torn mid-file
/// and nothing already uploaded is removed.
///
/// Returns `true` when an item was found and acted on; `false` for a stale
/// click on an item that has since finished (never an error - a queue the user
/// is looking at is always a little behind the one that is running).
#[tauri::command]
pub async fn cancel_work_item(
    state: State<'_, AppState>,
    account_id: AccountId,
    item_id: driven_core::queue::WorkItemId,
) -> CommandResult<bool> {
    let handle = state.account(account_id).ok_or_else(|| {
        CommandError::new(format!("no running orchestrator for account {account_id}"))
    })?;
    let outcome = handle.orchestrator.cancel_work_item(item_id).await;
    Ok(!matches!(
        outcome,
        driven_core::queue::CancelOutcome::NotFound
    ))
}

/// `clear_work_queue(account_id)` - cancel everything pending and gracefully
/// stop what is running (issue #303).
///
/// `account_id = null` clears EVERY account, which is what the panel's
/// "Clear all" does (it shows all accounts' work merged into one list). The
/// running item is asked no more than a single X on it would ask: stop
/// dispatching, finish what is in flight.
///
/// Returns how many pending items were dropped across the accounts touched.
#[tauri::command]
pub async fn clear_work_queue(
    state: State<'_, AppState>,
    account_id: Option<AccountId>,
) -> CommandResult<u32> {
    let mut cleared = 0u32;
    for (id, handle) in state.accounts() {
        if account_id.is_some_and(|wanted| wanted != id) {
            continue;
        }
        let outcome = handle.orchestrator.clear_work_queue().await;
        cleared =
            cleared.saturating_add(u32::try_from(outcome.cancelled_pending).unwrap_or(u32::MAX));
    }
    Ok(cleared)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire shape the webview's `PauseState` TypeScript union mirrors
    /// (ui/src/ipc/types.ts). Internally tagged on a snake_case `kind`, with
    /// `until_ms` on the timed variant only. A rename on either side silently
    /// breaks the banner, so pin both directions.
    #[test]
    fn pause_state_serializes_kind_tagged_snake_case() {
        assert_eq!(
            serde_json::to_value(PauseState::Indefinite).unwrap(),
            serde_json::json!({ "kind": "indefinite" })
        );
        assert_eq!(
            serde_json::to_value(PauseState::Timed {
                until_ms: 1_784_933_394_028
            })
            .unwrap(),
            serde_json::json!({ "kind": "timed", "until_ms": 1_784_933_394_028_i64 })
        );
        let round_tripped: PauseState =
            serde_json::from_value(serde_json::json!({ "kind": "timed", "until_ms": 7 })).unwrap();
        assert_eq!(round_tripped, PauseState::Timed { until_ms: 7 });
        assert_eq!(
            serde_json::from_value::<PauseState>(serde_json::json!({ "kind": "indefinite" }))
                .unwrap(),
            PauseState::Indefinite
        );
    }

    /// A timed pause expires AT its deadline (half-open, so the boundary tick
    /// resumes rather than showing "0m left" forever); an indefinite one never
    /// expires, however far the clock runs.
    #[test]
    fn timed_pause_expires_at_its_deadline_indefinite_never_does() {
        let pause = PauseState::Timed { until_ms: 1_000 };
        assert!(!pause.is_expired(999));
        assert!(pause.is_expired(1_000));
        assert!(pause.is_expired(1_001));
        assert!(!PauseState::Indefinite.is_expired(i64::MAX));
    }

    /// The auto-resume delay is what is LEFT, so a restart part-way through a
    /// 30-minute pause re-arms the remainder, not a fresh 30 minutes. An elapsed
    /// deadline saturates at zero (never a negative/underflowed duration), and an
    /// indefinite pause arms no timer at all.
    #[test]
    fn remaining_is_the_leftover_window_saturating_at_zero() {
        let pause = PauseState::Timed { until_ms: 60_000 };
        assert_eq!(pause.remaining(0), Some(Duration::from_secs(60)));
        assert_eq!(pause.remaining(30_000), Some(Duration::from_secs(30)));
        assert_eq!(pause.remaining(60_000), Some(Duration::ZERO));
        assert_eq!(pause.remaining(999_999), Some(Duration::ZERO));
        assert_eq!(PauseState::Indefinite.remaining(0), None);
    }

    /// A throwaway repo on a RAII temp dir; keep the returned `TempDir` alive
    /// for the repo's lifetime (drop deletes the directory).
    async fn temp_repo() -> (
        tempfile::TempDir,
        driven_core::state::sqlite::SqliteStateRepo,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let repo = driven_core::state::sqlite::SqliteStateRepo::open(&dir.path().join("state.db"))
            .await
            .expect("open repo");
        (dir, repo)
    }

    /// DESIGN s5.7: the manual pause must survive a restart. Round-trip both
    /// variants through the real settings table, and prove `None` clears it (so a
    /// resume does not leave a stale record a later boot would re-apply).
    #[tokio::test]
    async fn pause_state_round_trips_through_the_settings_table_and_clears() {
        let (_dir, repo) = temp_repo().await;
        assert_eq!(
            read_pause_state(&repo).await,
            None,
            "absent key = not paused"
        );

        write_pause_state(&repo, Some(PauseState::Indefinite)).await;
        assert_eq!(read_pause_state(&repo).await, Some(PauseState::Indefinite));

        let timed = PauseState::Timed {
            until_ms: 1_784_933_394_028,
        };
        write_pause_state(&repo, Some(timed)).await;
        assert_eq!(read_pause_state(&repo).await, Some(timed));

        write_pause_state(&repo, None).await;
        assert_eq!(
            read_pause_state(&repo).await,
            None,
            "a resume must clear the record, not leave one a later boot re-applies"
        );
    }

    /// A corrupt or foreign pause record must read as "not paused" rather than
    /// propagating an error - it is on the boot path, and the safe direction is
    /// backups running, which the user can always re-pause with one click.
    #[tokio::test]
    async fn an_unparseable_pause_record_reads_as_not_paused() {
        let (_dir, repo) = temp_repo().await;
        repo.set_setting(PAUSE_STATE_KEY, &serde_json::json!({ "kind": "sideways" }))
            .await
            .unwrap();
        assert_eq!(read_pause_state(&repo).await, None);

        repo.set_setting(PAUSE_STATE_KEY, &serde_json::json!("nonsense"))
            .await
            .unwrap();
        assert_eq!(read_pause_state(&repo).await, None);
    }

    /// spec 2026-08-01 (unified pause/status banner): `sync_now` with
    /// `bypass_gates: Some(true)` must arm the one-shot gate bypass on EVERY
    /// targeted account's orchestrator BEFORE the manual trigger, so the very
    /// next cycle actually sees it armed (arming AFTER the trigger would race
    /// the run loop and could miss the bypassed cycle entirely).
    #[tokio::test]
    async fn sync_now_bypasses_gates_once_before_triggering_when_requested() {
        use crate::app_state::tests::{build_fake_account_handle, FakeOrchestrator};
        use crate::app_state::RemoteMode;
        use std::collections::HashMap;

        let (_dir, repo) = temp_repo().await;
        let account_id = AccountId::new_v4();
        let fake = Arc::new(FakeOrchestrator::new());
        let orchestrator: Arc<dyn Orchestrator> = fake.clone();
        let mut accounts = HashMap::new();
        accounts.insert(account_id, build_fake_account_handle(orchestrator));
        let app_state = AppState::new(
            Arc::new(repo),
            accounts,
            RemoteMode::Fake,
            Arc::new(std::sync::Mutex::new(HashMap::new())),
        );

        sync_now_impl(&app_state, None, Some(true))
            .await
            .expect("sync_now with bypass_gates succeeds");

        assert_eq!(
            fake.calls(),
            vec!["bypass_gates_once", "trigger"],
            "bypass_gates_once must be armed before the manual trigger"
        );
    }

    /// The unchanged behaviour: without `bypass_gates: Some(true)`, `sync_now`
    /// never arms the one-shot bypass - only `trigger` fires.
    #[tokio::test]
    async fn sync_now_without_bypass_flag_never_arms_the_bypass() {
        use crate::app_state::tests::{build_fake_account_handle, FakeOrchestrator};
        use crate::app_state::RemoteMode;
        use std::collections::HashMap;

        let (_dir, repo) = temp_repo().await;
        let account_id = AccountId::new_v4();
        let fake = Arc::new(FakeOrchestrator::new());
        let orchestrator: Arc<dyn Orchestrator> = fake.clone();
        let mut accounts = HashMap::new();
        accounts.insert(account_id, build_fake_account_handle(orchestrator));
        let app_state = AppState::new(
            Arc::new(repo),
            accounts,
            RemoteMode::Fake,
            Arc::new(std::sync::Mutex::new(HashMap::new())),
        );

        sync_now_impl(&app_state, None, None)
            .await
            .expect("sync_now succeeds");

        assert_eq!(
            fake.calls(),
            vec!["trigger"],
            "no bypass_gates flag means no bypass_gates_once call"
        );
    }
}
