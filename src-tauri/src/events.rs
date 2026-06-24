//! Outbound event emit helpers (Rust -> webview, SPEC s11.7).
//!
//! Thin wrappers over the Tauri v2 [`Emitter`] trait (`app.emit(event,
//! payload)`; the v1 `emit_all` was removed - SPEC s11.7). Each helper pins
//! the canonical SPEC s11.7 event name so call sites cannot typo the channel.
//! Payload DTOs (`GlobalSyncStatus`, `ActivityEntry`) land with the IPC layer
//! in M6/M7; until then the broadcast helpers are generic over any
//! `serde::Serialize` payload so the orchestrator-bridge can emit without the
//! DTO crate existing yet.

use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// `sync:status_changed` - global sync status changed (payload:
/// `GlobalSyncStatus`, SPEC s11.7).
pub const EVENT_SYNC_STATUS_CHANGED: &str = "sync:status_changed";
/// `sync:source_progress` - per-source progress (payload:
/// `{ source_id, progress }`, SPEC s11.7).
///
/// Reserved for M6: the per-source progress DTO + the bridge that emits it land
/// with the M6 IPC layer (the M5 event bridge only forwards `StateChanged`).
#[allow(dead_code)]
pub const EVENT_SYNC_SOURCE_PROGRESS: &str = "sync:source_progress";
/// `activity:new` - a new activity-log entry (payload: `ActivityEntry`,
/// SPEC s11.7).
///
/// M7 (activity dashboard): the orchestrator broadcasts
/// `OrchestratorEvent::ActivityWritten` on every durable activity write and the
/// event bridge re-emits the carried `ActivityEntry` on this channel for the
/// dashboard's live tail.
pub const EVENT_ACTIVITY_NEW: &str = "activity:new";
/// `activity:lagged` - the live-tail broadcast lagged and dropped one or more
/// `activity:new` events (payload: none).
///
/// M7-P1-1: the per-account `OrchestratorEvent` broadcast is bounded (capacity
/// 256). Under a burst (e.g. a per-file error storm) the event bridge's receiver
/// can lag and the dropped events would be PERMANENTLY missing from the live
/// tail, violating DESIGN s8.3's last-1000 tail + ROADMAP M7's <500ms latency.
/// On `RecvError::Lagged` the bridge emits this gap signal; the webview's
/// activity store reconciles by re-querying the durable `activity_log` (page 0,
/// the source of truth) and dedup-merging, so no durable row is lost. The
/// 500ms-typical path stays event-driven via `activity:new`; this fires only on
/// the rare lag.
pub const EVENT_ACTIVITY_LAGGED: &str = "activity:lagged";
/// `account:needs_reauth` - a refresh token was revoked (payload:
/// `{ account_id, email }`, SPEC s11.7).
pub const EVENT_ACCOUNT_NEEDS_REAUTH: &str = "account:needs_reauth";
/// `oauth:complete` - an in-flight add-account / reauth OAuth flow reached a
/// terminal state (payload: `{ session_id, status }`, SPEC s11.7).
///
/// M6: the wizard subscribes so it can advance past the OAuth step without
/// polling. Emitted by the accounts command layer once
/// [`driven_drive::google::oauth::run_pkce_loopback_flow`] resolves; the
/// emit helper lands with that implementer, so the constant is defined-but-
/// uncalled in the M6 scaffold.
#[allow(dead_code)]
pub const EVENT_OAUTH_COMPLETE: &str = "oauth:complete";
/// `updater:available` - a newer release is available (payload: `UpdateInfo`,
/// SPEC s11.7).
///
/// M6: the About tab + in-app banner subscribe. The periodic updater check
/// emits it; the emit helper lands with the settings implementer, so the
/// constant is defined-but-uncalled in the M6 scaffold.
#[allow(dead_code)]
pub const EVENT_UPDATER_AVAILABLE: &str = "updater:available";
/// `updater:downloaded` - the available update finished downloading and is
/// ready to install (payload: `UpdateInfo`, SPEC s11.7).
#[allow(dead_code)]
pub const EVENT_UPDATER_DOWNLOADED: &str = "updater:downloaded";

/// Broadcast `sync:status_changed` with the global status payload (SPEC s11.7).
///
/// Thin wrapper over the v2 [`Emitter::emit`] so the orchestrator-event bridge
/// cannot typo the channel name. The payload is whatever the IPC layer hands
/// in (`GlobalSyncStatus` in production); kept generic so the bridge can emit
/// before the concrete DTO crate exists.
pub fn emit_sync_status_changed<P: Serialize + Clone>(
    app: &AppHandle,
    status: &P,
) -> tauri::Result<()> {
    app.emit(EVENT_SYNC_STATUS_CHANGED, status)
}

/// Broadcast `activity:new` with the new activity entry (SPEC s11.7).
///
/// M7 (activity dashboard): the event bridge calls this on every
/// `OrchestratorEvent::ActivityWritten`, forwarding the carried `ActivityEntry`
/// (already the camelCase wire shape) to the webview's live tail.
pub fn emit_activity_new<P: Serialize + Clone>(app: &AppHandle, entry: &P) -> tauri::Result<()> {
    app.emit(EVENT_ACTIVITY_NEW, entry)
}

/// Broadcast `activity:lagged` - the live-tail dropped events on broadcast lag
/// (M7-P1-1, SPEC s11.7). Carries no payload: it is purely a gap signal telling
/// the webview store to reconcile from the durable `activity_log`. `skipped` is
/// the broadcast's reported drop count, attached as a structured field for
/// diagnostics only (the store does not need it to reconcile).
pub fn emit_activity_lagged(app: &AppHandle, skipped: u64) -> tauri::Result<()> {
    app.emit(
        EVENT_ACTIVITY_LAGGED,
        serde_json::json!({ "skipped": skipped }),
    )
}

/// Broadcast `account:needs_reauth` for `account_id` / `email` (SPEC s11.7).
///
/// Emits the `{ account_id, email }` payload the webview's re-consent banner
/// subscribes to (SPEC s11.7 table). Built as an inline JSON object so the
/// shape matches the spec without a dedicated DTO type.
pub fn emit_account_needs_reauth(
    app: &AppHandle,
    account_id: &str,
    email: &str,
) -> tauri::Result<()> {
    app.emit(
        EVENT_ACCOUNT_NEEDS_REAUTH,
        serde_json::json!({ "account_id": account_id, "email": email }),
    )
}
