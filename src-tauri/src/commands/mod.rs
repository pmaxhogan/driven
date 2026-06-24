//! Tauri IPC command surface (SPEC s11).
//!
//! M6 lands the accounts / sources / settings surface (SPEC s11.1 / s11.2 /
//! s11.6) alongside the M5 sync commands (SPEC s11.3); the restore + activity
//! surface (SPEC s11.4 / s11.5) is M7/M8. The shared [`CommandError`] + the
//! [`validate_writable_dest`] path-safety helper live here; the per-area
//! commands live in [`accounts`], [`sources`], [`settings`], and [`sync`], and
//! the shared DTOs in [`dtos`].

pub mod accounts;
pub mod dtos;
pub mod settings;
pub mod sources;
pub mod sync;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The serializable error every IPC command returns on failure.
///
/// Tauri requires a command's `Err` type to be `Serialize`; `anyhow::Error`
/// is not, so the command bodies map their internal errors into this stable
/// shape (M6 will enrich it with the SPEC s24 `ErrorCode`). A bare wrapper for
/// now so the M5 sync commands compile + surface a message to the webview.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandError {
    /// Human-readable error message (already redacted of secrets).
    pub message: String,
}

impl CommandError {
    /// Build a command error from any displayable error.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CommandError {}

impl From<anyhow::Error> for CommandError {
    fn from(e: anyhow::Error) -> Self {
        Self::new(e.to_string())
    }
}

/// The result alias every IPC command returns (SPEC s11).
pub type CommandResult<T> = Result<T, CommandError>;

/// A token proving a path was produced by a `tauri-plugin-dialog` dialog
/// (SPEC s11.6.1), NOT injected as a raw string by the (untrusted) webview.
///
/// The webview never fabricates one of these: the dialog wrappers in
/// `ui/src/ipc/*.ts` round-trip the dialog's returned path together with its
/// token, and the backend confines every write to a dialog-derived path. M6
/// scaffold defines the type so the signature of [`validate_writable_dest`] is
/// frozen for the implementers; its concrete contents (e.g. an HMAC over the
/// path + a per-session nonce) are filled in by the settings/sources
/// implementer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DialogToken(pub String);

/// Validate a webview-supplied destination path before ANY filesystem write
/// (SPEC s11.6.1). The single choke point every path-bearing write command
/// (`restore_files`, `export_diagnostic_bundle`, `add_source`) routes through.
///
/// Enforces, in order (SPEC s11.6.1 1-4):
/// 1. canonicalize via `dunce::canonicalize` (rejecting a non-existent parent);
/// 2. confine to the allowed root proven by `dialog_token` (the round-tripped
///    dialog selection) - reject any path the webview shaped itself;
/// 3. reject any `..` surviving canonicalisation (path-traversal defense);
/// 4. reject a symlink at the leaf (no write through a symlink).
///
/// Returns the canonical, confined [`PathBuf`] the caller then writes to
/// atomically (SPEC s11.6.1 step 5). M6 SCAFFOLD: `todo!()` body - the
/// settings/sources implementer wires in `dunce` + the dialog-token check; the
/// SIGNATURE is the frozen contract. Tests land in
/// `src-tauri/tests/ipc_path_validation.rs` (SPEC s11.6.1).
#[allow(dead_code)]
pub fn validate_writable_dest(path: &Path, dialog_token: &DialogToken) -> CommandResult<PathBuf> {
    let _ = (path, dialog_token);
    todo!("M6 s11.6.1: canonicalize + confine to dialog-token root + reject traversal/symlink-leaf")
}
