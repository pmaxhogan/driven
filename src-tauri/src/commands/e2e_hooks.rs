//! Env-gated e2e hooks (agent QA harness).
//!
//! SPEC s11.6.1 makes every path-bearing write command require a
//! backend-minted dialog token from a REAL native dialog - which is exactly
//! right in production and exactly what a headless WebDriver harness cannot
//! operate (WebDriver drives the webview; a native GTK/Cocoa/Win32 dialog is
//! outside it). The harness still must exercise the PRODUCTION `add_source` /
//! `restore_files` paths, token validation included.
//!
//! So this module provides ONE hook: `e2e_pick_folder(path)` - the headless
//! twin of [`super::dialogs::pick_folder_dialog`]. It validates that `path`
//! is an existing directory and mints the same single-use dialog token the
//! native picker would have minted for the same user choice. Everything
//! downstream (canonicalisation, overlap rejection, confined-dest restore
//! writes) runs unchanged.
//!
//! ## Gating (load-bearing)
//!
//! The hook is registered unconditionally but REFUSES to run unless the
//! process was started with `DRIVEN_E2E_HOOKS=1`. The env var is read at
//! every call (not cached) so there is no way to flip it on after boot via
//! IPC. A production install never sets it, so the hook is inert there; the
//! e2e container sets it at launch. When enabled the boot log carries a
//! prominent warning (see the check in `lib.rs`), so a diagnostic bundle from
//! a hook-enabled session is self-describing.

use tauri::State;

use driven_core::types::ErrorCode;

use crate::app_state::AppState;
use crate::commands::dtos::PickedPath;
use crate::commands::{CommandError, CommandResult};

/// Tracing target for the e2e hook layer.
const TARGET: &str = "driven::app::e2e_hooks";

/// Env var gating the e2e hooks. Value must be exactly `1`.
pub const ENV_E2E_HOOKS: &str = "DRIVEN_E2E_HOOKS";

/// `true` iff the process runs with the e2e hooks enabled.
pub fn hooks_enabled() -> bool {
    std::env::var(ENV_E2E_HOOKS)
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// `e2e_pick_folder(path)` - mint a dialog token for `path` WITHOUT a native
/// dialog (headless harness twin of `pick_folder_dialog`; see module docs).
///
/// Validates that `path` names an existing directory, then mints the same
/// one-shot token the real picker mints. Refused (`internal.bug`) unless
/// `DRIVEN_E2E_HOOKS=1` was set at process start.
#[tauri::command]
pub async fn e2e_pick_folder(
    state: State<'_, AppState>,
    path: String,
) -> CommandResult<PickedPath> {
    if !hooks_enabled() {
        // Deliberately unspecific: in a production build without the env gate
        // this command is a dead end, and the message should not advertise
        // what enabling it would do.
        return Err(CommandError::with_code(
            ErrorCode::InternalBug,
            "e2e hooks are not enabled in this session",
        ));
    }
    let dir = std::path::PathBuf::from(&path);
    if !dir.is_dir() {
        return Err(CommandError::with_code(
            ErrorCode::LocalIoError,
            format!("e2e_pick_folder: not an existing directory: {path}"),
        ));
    }
    let token = state.mint_dialog_token(dir.clone());
    tracing::warn!(
        target: TARGET,
        path = %dir.display(),
        "e2e hook minted a dialog token (DRIVEN_E2E_HOOKS=1 session)"
    );
    Ok(PickedPath {
        path: dir.to_string_lossy().into_owned(),
        token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_default_off() {
        // The suite never sets the var globally; a hooks-off process must
        // report disabled. (The enabled path is covered by the e2e suite
        // itself, which boots the app WITH the env var - an in-process test
        // that mutates process-global env would race the other tests here.)
        if std::env::var(ENV_E2E_HOOKS).is_err() {
            assert!(!hooks_enabled());
        }
    }
}
