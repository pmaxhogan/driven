//! Source IPC commands (SPEC s11.2).
//!
//! The Sources settings tab + the add-source wizard (DESIGN s8.2 / s8.5 step 3)
//! drive these. Each is a `#[tauri::command]` over `State<AppState>`.
//!
//! M6 SCAFFOLD: command bodies are `todo!()` - the sources implementer fills
//! them in. The signatures + [`crate::commands::dtos`] shapes are frozen.
//!
//! IPC path safety (SPEC s11.6.1): `add_source` and `preview_exclusions` take a
//! local `PathBuf` from the (untrusted) webview and MUST validate it via
//! [`crate::commands::validate_writable_dest`]-style confinement before any
//! filesystem walk - the implementer wires that in. `pick_drive_folder` lists
//! Drive (remote) folders; `preview_exclusions` walks the local tree via
//! [`driven_core::scanner`] + [`driven_core::exclude`].

use tauri::State;

use driven_core::types::{AccountId, SourceId};

use crate::app_state::AppState;
use crate::commands::dtos::{
    AddSourceRequest, DriveFolderListing, ExclusionPreview, ExclusionPreviewRequest, SourceDto,
    SourcePatch,
};
use crate::commands::CommandResult;

/// `list_sources()` - every configured backup source (SPEC s11.2).
///
/// Reads `backup_sources` from the state DB and maps each
/// [`driven_core::state::SourceRow`] to a [`SourceDto`] (the wrapped per-source
/// key is never exposed to the webview).
#[tauri::command]
pub async fn list_sources(state: State<'_, AppState>) -> CommandResult<Vec<SourceDto>> {
    let _ = state;
    todo!("M6 sources: map StateRepo::list_sources() rows to SourceDto")
}

/// `add_source(req)` - create a new backup source (SPEC s11.2).
///
/// SPEC s11.6.1: `req.local_path` must be dialog-derived; validate before any
/// filesystem access. Persists the `backup_sources` row (wrapping a fresh
/// per-source key when `encryption_enabled`) and returns the new [`SourceDto`].
#[tauri::command]
pub async fn add_source(
    state: State<'_, AppState>,
    req: AddSourceRequest,
) -> CommandResult<SourceDto> {
    let _ = (state, req);
    todo!("M6 sources: validate local_path (s11.6.1), persist source row, return SourceDto")
}

/// `update_source(source_id, patch)` - patch an existing source (SPEC s11.2).
///
/// Applies the present [`SourcePatch`] fields, reconfigures the owning
/// account's orchestrator (so toggling `enabled` / changing globs takes effect
/// without a restart), and returns the updated [`SourceDto`].
#[tauri::command]
pub async fn update_source(
    state: State<'_, AppState>,
    source_id: SourceId,
    patch: SourcePatch,
) -> CommandResult<SourceDto> {
    let _ = (state, source_id, patch);
    todo!("M6 sources: apply SourcePatch, reconfigure orchestrator, return SourceDto")
}

/// `remove_source(source_id, delete_remote)` - remove a source (SPEC s11.2).
///
/// Deletes the `backup_sources` row (cascading its `file_state`) and
/// reconfigures the owning orchestrator. When `delete_remote` is set, also
/// trashes the source's backed-up Drive content.
#[tauri::command]
pub async fn remove_source(
    state: State<'_, AppState>,
    source_id: SourceId,
    delete_remote: bool,
) -> CommandResult<()> {
    let _ = (state, source_id, delete_remote);
    todo!("M6 sources: delete source row, reconfigure orchestrator, optional remote trash")
}

/// `pick_drive_folder(account_id, start_folder_id?)` - list a Drive folder's
/// children for the destination picker (SPEC s11.2).
///
/// Lists `start_folder_id`'s child folders (My Drive root when `None`) via the
/// account's [`driven_drive::RemoteStore`] and returns a [`DriveFolderListing`]
/// with breadcrumb context. Drive-side listing, NOT a local path.
#[tauri::command]
pub async fn pick_drive_folder(
    state: State<'_, AppState>,
    account_id: AccountId,
    start_folder_id: Option<String>,
) -> CommandResult<DriveFolderListing> {
    let _ = (state, account_id, start_folder_id);
    todo!("M6 sources: list_folder via the account's RemoteStore, return DriveFolderListing")
}

/// `preview_exclusions(req)` - preview which files the candidate rules would
/// include vs exclude (SPEC s11.2; DESIGN s8.5 step 3 exclusion preview).
///
/// SPEC s11.6.1: `req.local_path` must be dialog-derived; validate before the
/// walk. Runs [`driven_core::scanner`] + [`driven_core::exclude`] over a bounded
/// sample and returns an [`ExclusionPreview`].
#[tauri::command]
pub async fn preview_exclusions(
    state: State<'_, AppState>,
    req: ExclusionPreviewRequest,
) -> CommandResult<ExclusionPreview> {
    let _ = (state, req);
    todo!("M6 sources: validate local_path (s11.6.1), walk via scanner+exclude, return ExclusionPreview")
}
