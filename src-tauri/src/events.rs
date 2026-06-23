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
use tauri::AppHandle;

/// `sync:status_changed` - global sync status changed (payload:
/// `GlobalSyncStatus`, SPEC s11.7).
pub const EVENT_SYNC_STATUS_CHANGED: &str = "sync:status_changed";
/// `sync:source_progress` - per-source progress (payload:
/// `{ source_id, progress }`, SPEC s11.7).
pub const EVENT_SYNC_SOURCE_PROGRESS: &str = "sync:source_progress";
/// `activity:new` - a new activity-log entry (payload: `ActivityEntry`,
/// SPEC s11.7).
pub const EVENT_ACTIVITY_NEW: &str = "activity:new";
/// `account:needs_reauth` - a refresh token was revoked (payload:
/// `{ account_id, email }`, SPEC s11.7).
pub const EVENT_ACCOUNT_NEEDS_REAUTH: &str = "account:needs_reauth";

/// Broadcast `sync:status_changed` with the global status payload (SPEC s11.7).
///
/// TODO(M5): `app.emit(EVENT_SYNC_STATUS_CHANGED, &status)` and map the error.
pub fn emit_sync_status_changed<P: Serialize + Clone>(
    app: &AppHandle,
    status: &P,
) -> tauri::Result<()> {
    let _ = (app, status);
    todo!("M5: app.emit(EVENT_SYNC_STATUS_CHANGED, status)")
}

/// Broadcast `activity:new` with the new activity entry (SPEC s11.7).
///
/// TODO(M5): `app.emit(EVENT_ACTIVITY_NEW, &entry)`.
pub fn emit_activity_new<P: Serialize + Clone>(app: &AppHandle, entry: &P) -> tauri::Result<()> {
    let _ = (app, entry);
    todo!("M5: app.emit(EVENT_ACTIVITY_NEW, entry)")
}

/// Broadcast `account:needs_reauth` for `account_id` / `email` (SPEC s11.7).
///
/// TODO(M5): emit the `{ account_id, email }` payload via
/// `app.emit(EVENT_ACCOUNT_NEEDS_REAUTH, serde_json::json!(...))`.
pub fn emit_account_needs_reauth(
    app: &AppHandle,
    account_id: &str,
    email: &str,
) -> tauri::Result<()> {
    let _ = (app, account_id, email);
    todo!("M5: app.emit(EVENT_ACCOUNT_NEEDS_REAUTH, account_id + email payload)")
}
