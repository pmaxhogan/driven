//! Settings & misc IPC commands (SPEC s11.6).
//!
//! The Rules + About settings tabs (DESIGN s8.2) drive these. Each is a
//! `#[tauri::command]` over `State<AppState>`.
//!
//! M6 SCAFFOLD: command bodies are `todo!()` - the settings implementer fills
//! them in. The signatures + [`crate::commands::dtos`] shapes are frozen.
//!
//! IPC path safety (SPEC s11.6.1): `export_diagnostic_bundle` takes a
//! destination `PathBuf` from the (untrusted) webview and MUST validate it via
//! [`crate::commands::validate_writable_dest`] (dialog-derived, confined, no
//! traversal, no symlink-at-leaf, atomic write) before writing the ZIP.

use std::path::PathBuf;

use tauri::State;

use crate::app_state::AppState;
use crate::commands::dtos::{ReleaseDto, SettingsDto, SettingsPatch, UpdateInfo};
use crate::commands::CommandResult;

/// `get_settings()` - the full settings snapshot (SPEC s11.6).
///
/// Reads the SPEC s22 KV groups (`global`, `telemetry`, `updater`, `ui`, and
/// `windows` on Windows) from the `settings` table into one [`SettingsDto`].
#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> CommandResult<SettingsDto> {
    let _ = state;
    todo!("M6 settings: read the SPEC s22 KV groups into SettingsDto")
}

/// `update_settings(patch)` - apply a settings patch (SPEC s11.6).
///
/// Merges the present [`SettingsPatch`] groups/fields into the stored settings,
/// applies side effects (autostart registration, log-level change, locale
/// change re-renders the tray + frontend), and returns the updated
/// [`SettingsDto`].
#[tauri::command]
pub async fn update_settings(
    state: State<'_, AppState>,
    patch: SettingsPatch,
) -> CommandResult<SettingsDto> {
    let _ = (state, patch);
    todo!("M6 settings: merge SettingsPatch, apply side effects, return SettingsDto")
}

/// `export_diagnostic_bundle(dest)` - write a redacted diagnostic ZIP
/// (SPEC s11.6, s18).
///
/// SPEC s11.6.1: `dest` MUST be dialog-derived; validate via
/// [`crate::commands::validate_writable_dest`] and write atomically. Returns the
/// final ZIP path.
#[tauri::command]
pub async fn export_diagnostic_bundle(
    state: State<'_, AppState>,
    dest: PathBuf,
) -> CommandResult<PathBuf> {
    let _ = (state, dest);
    todo!("M6 settings: validate dest (s11.6.1), build the redacted bundle, atomic-write the ZIP")
}

/// `check_for_updates()` - check the updater endpoint for a newer release
/// (SPEC s11.6, s15).
///
/// Queries the active channel's updater manifest; returns `Some(UpdateInfo)`
/// when a newer version is available, `None` when up to date.
#[tauri::command]
pub async fn check_for_updates(state: State<'_, AppState>) -> CommandResult<Option<UpdateInfo>> {
    let _ = state;
    todo!("M6 settings: query the channel updater manifest, return Option<UpdateInfo>")
}

/// `list_releases(page)` - list published releases for the About tab's
/// release-notes viewer (SPEC s11.6).
///
/// Fetches a page of releases from the GitHub releases API for the active
/// channel and maps each to a [`ReleaseDto`].
#[tauri::command]
pub async fn list_releases(
    state: State<'_, AppState>,
    page: u32,
) -> CommandResult<Vec<ReleaseDto>> {
    let _ = (state, page);
    todo!("M6 settings: fetch a page of releases, map to ReleaseDto")
}
