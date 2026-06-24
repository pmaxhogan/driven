//! Backend-owned native file dialogs (C1, SPEC s11.6.1).
//!
//! SPEC s11.6.1 requires every path-bearing IPC write command to use a
//! DIALOG-DERIVED path the webview cannot forge. The frontend `tauri-plugin-dialog`
//! cannot prove to the backend that a path it sends came from a real dialog, so
//! the BACKEND owns the dialog instead: it opens the native folder / save-file
//! picker, mints a one-shot token bound to the path the USER chose
//! ([`AppState::mint_dialog_token`]), and returns `{ path, token }`. The webview
//! threads the token (+ path) into the matching write command, which validates
//! the token maps to exactly that path ([`AppState::take_dialog_token`]) - so a
//! tampered / injected path is rejected.
//!
//! The dialog plugin's `pick_folder` / `save_file` are CALLBACK-based; we bridge
//! the callback to the async command via a `oneshot` channel so the command can
//! `await` the user's choice without blocking the event loop.

use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

use driven_core::types::ErrorCode;

use crate::app_state::AppState;
use crate::commands::dtos::PickedPath;
use crate::commands::{CommandError, CommandResult};

/// Tracing target for the dialog command layer.
const TARGET: &str = "driven::app::dialogs";

/// The diagnostic-bundle default save filename (C2): the save dialog suggests
/// this so the user lands on a concrete `.zip` file path, not a directory.
const DEFAULT_BUNDLE_NAME: &str = "driven-diagnostics.zip";

/// `pick_folder_dialog()` - open the native folder picker (C1, SPEC s11.6.1).
///
/// Returns the chosen folder path PLUS a one-shot backend-minted token bound to
/// it. The add-source wizard passes both to `add_source` so the backend can
/// prove `local_path` came from this dialog (not an injected webview string).
/// Returns `auth.consent_required`-shaped... no: a user cancel is surfaced as a
/// `local.io_error` "cancelled" so the frontend can distinguish it from a real
/// failure and simply not advance.
#[tauri::command]
pub async fn pick_folder_dialog(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<PickedPath> {
    let path = pick_folder(&app).await?;
    let token = state.mint_dialog_token(path.clone());
    tracing::debug!(target: TARGET, "folder dialog picked + token minted");
    Ok(PickedPath {
        path: path.to_string_lossy().to_string(),
        token,
    })
}

/// `pick_save_zip_dialog()` - open the native SAVE-FILE picker for the
/// diagnostic-bundle `.zip` (C1 + C2, SPEC s11.6.1).
///
/// Returns the chosen `.zip` file path (a concrete FILE, not a directory - C2)
/// PLUS a one-shot backend-minted token bound to it. The About tab passes both
/// to `export_diagnostic_bundle`, which writes the ZIP atomically AT that path.
#[tauri::command]
pub async fn pick_save_zip_dialog(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<PickedPath> {
    let path = save_zip(&app).await?;
    let token = state.mint_dialog_token(path.clone());
    tracing::debug!(target: TARGET, "save-zip dialog picked + token minted");
    Ok(PickedPath {
        path: path.to_string_lossy().to_string(),
        token,
    })
}

/// Open the native folder picker, awaiting the user's choice via a oneshot the
/// dialog callback fulfils. A cancel maps to a `local.io_error` "cancelled".
async fn pick_folder(app: &AppHandle) -> CommandResult<std::path::PathBuf> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<std::path::PathBuf>>();
    app.dialog().file().pick_folder(move |chosen| {
        let _ = tx.send(chosen.and_then(file_path_to_pathbuf));
    });
    match rx.await {
        Ok(Some(path)) => Ok(path),
        Ok(None) => Err(cancelled()),
        Err(_) => Err(CommandError::with_code(
            ErrorCode::LocalIoError,
            "folder dialog closed unexpectedly",
        )),
    }
}

/// Open the native save-file picker (suggested name + `.zip` filter), awaiting
/// the user's choice. A cancel maps to a `local.io_error` "cancelled".
async fn save_zip(app: &AppHandle) -> CommandResult<std::path::PathBuf> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<std::path::PathBuf>>();
    app.dialog()
        .file()
        .set_file_name(DEFAULT_BUNDLE_NAME)
        .add_filter("ZIP archive", &["zip"])
        .save_file(move |chosen| {
            let _ = tx.send(chosen.and_then(file_path_to_pathbuf));
        });
    match rx.await {
        Ok(Some(path)) => Ok(path),
        Ok(None) => Err(cancelled()),
        Err(_) => Err(CommandError::with_code(
            ErrorCode::LocalIoError,
            "save dialog closed unexpectedly",
        )),
    }
}

/// Map the dialog plugin's `FilePath` to a real [`std::path::PathBuf`]; a
/// non-filesystem (e.g. content-URI) choice yields `None` so the caller treats
/// it as a cancel rather than fabricating a path.
fn file_path_to_pathbuf(fp: tauri_plugin_dialog::FilePath) -> Option<std::path::PathBuf> {
    fp.into_path().ok()
}

/// The shared "user cancelled the dialog" error (a benign no-advance signal).
fn cancelled() -> CommandError {
    CommandError::with_code(ErrorCode::LocalIoError, "dialog cancelled")
}
