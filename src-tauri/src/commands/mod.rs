//! Tauri IPC command surface (SPEC s11).
//!
//! M5 scopes the IPC layer to the SPEC s11.3 SYNC commands + status; the full
//! account/source/restore/settings surface (SPEC s11.1/s11.2/s11.5/s11.6) is
//! M6/M7. The shared [`CommandError`] and the sync-status DTO live here; the
//! sync commands themselves are in [`sync`].

pub mod sync;

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
