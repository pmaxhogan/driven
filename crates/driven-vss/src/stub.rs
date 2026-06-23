//! Non-Windows [`VssSnapshot`] stub.
//!
//! macOS / Linux have no VSS equivalent that fits a backup client's model
//! (DESIGN s5.3), so every operation returns [`VssError::Unavailable`] and the
//! orchestrator degrades to skip-the-locked-file exactly as an un-elevated
//! Windows host does. Keeping the same type shape lets `driven-core` code
//! against [`VssSnapshot`] without per-OS `cfg` at the call site.

use std::path::{Path, PathBuf};

use crate::{SnapshotHandle, VssError};

/// No-op shadow-copy handle for non-Windows targets. Construction always
/// fails with [`VssError::Unavailable`]; the other methods exist only so the
/// type surface matches the Windows build.
#[derive(Debug)]
pub struct VssSnapshot {
    // Never constructed off Windows (create always errors), but the field
    // keeps the type non-trivial and documents intent.
    _private: (),
}

impl VssSnapshot {
    /// Always [`VssError::Unavailable`] off Windows - there is no VSS.
    pub fn create(_volume_letter: &str) -> Result<Self, VssError> {
        Err(VssError::Unavailable("VSS is Windows-only"))
    }

    /// Unreachable off Windows (no instance is ever constructed); present for
    /// type-surface parity with the Windows build.
    pub fn root_path(&self) -> PathBuf {
        PathBuf::new()
    }

    /// Unreachable off Windows; type-surface parity.
    pub fn handle(&self) -> SnapshotHandle {
        SnapshotHandle {
            snapshot_id: String::new(),
            device_root: String::new(),
        }
    }

    /// Unreachable off Windows; type-surface parity.
    pub fn map_path(&self, _live_path: &Path) -> Result<PathBuf, VssError> {
        Err(VssError::Unavailable("VSS is Windows-only"))
    }

    /// Releasing a recorded orphan by GUID is a no-op off Windows.
    pub fn delete_by_id(_snapshot_id: &str) -> Result<(), VssError> {
        Err(VssError::Unavailable("VSS is Windows-only"))
    }
}
