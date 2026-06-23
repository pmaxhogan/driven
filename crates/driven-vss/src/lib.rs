//! `driven-vss` - Windows Volume Shadow Copy Service (VSS) reads for
//! exclusively-locked files (ROADMAP M3.5, DESIGN s5.3).
//!
//! The user's core pain point: files held with an exclusive write lock
//! (Outlook PSTs, running database files, hypervisor disk images) cannot be
//! opened for read even with `FILE_SHARE_DELETE`, so a naive backup client
//! skips them forever. Driven falls through to a per-cycle VSS snapshot of the
//! volume and reads the locked file from the read-only shadow copy instead.
//!
//! # Surface split
//!
//! Everything here is OS-independent and compiles on every target:
//! - [`VssMode`] - the persisted `auto` / `always` / `never` setting (SPEC
//!   s22 `windows.vss_mode`).
//! - [`is_elevated`] - true when the process holds the Administrator
//!   elevation that VSS snapshot creation requires (always `false` off
//!   Windows).
//! - [`VssProvider`] - the seam the executor's open path consults. The
//!   production [`RealVssProvider`] lazily creates ONE snapshot per volume per
//!   cycle and reuses it; [`FakeVssProvider`] lets the cross-OS tests exercise
//!   the degrade path deterministically.
//! - [`fallback_decision`] - the pure "open failed + which mode + elevated +
//!   what did the provider return -> open the mapped path or skip-as-locked"
//!   function, table-tested on all OSes (the locked-file behavioural contract
//!   that cannot be produced with a real lock off Windows).
//! - [`OrphanRegistry`] / [`prune_orphans`] - the snapshot-ownership ledger
//!   and the >1h age filter for releasing shadow copies an unclean shutdown
//!   (`kill -9`) left behind, where the RAII [`Drop`] never ran.
//!
//! The real `IVssBackupComponents` COM sequence ([`VssSnapshot`]) lives in the
//! `#[cfg(windows)]` [`windows_vss`] module. Off Windows, [`VssSnapshot`] is a
//! stub whose `create` returns [`VssError::Unavailable`] so the orchestrator
//! degrades exactly as it does for an un-elevated Windows host.
//!
//! # Why a hand-declared COM interface
//!
//! `IVssBackupComponents` and its `CreateVssBackupComponents` factory are NOT
//! present in the `windows` 0.62 bindings - win32metadata never projected
//! them (microsoft/win32metadata#2095, open). The supporting types
//! (`IVssAsync`, `VSS_SNAPSHOT_PROP`, the `VSS_CTX_*` / `VSS_SS_*` constants)
//! DO exist. So [`windows_vss`] hand-declares the `IVssBackupComponents`
//! vtable with its real IID via [`windows::core::interface`] and loads the
//! factory (`CreateVssBackupComponentsInternal`) from `vssapi.dll` at runtime.
//! This is recorded in design/CODEX_NOTES.md.

#![forbid(unsafe_op_in_unsafe_fn)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

mod mode;
mod orphan;
mod provider;

pub use mode::VssMode;
pub use orphan::{prune_orphans, OrphanRegistry, RecordedSnapshot, DEFAULT_ORPHAN_MAX_AGE_MS};
pub use provider::{FakeVssProvider, RealVssProvider, SnapshotRecorder, VssProvider};

// The real COM sequence (Windows) and the no-op stub (everything else) are
// each behind a cfg. Exactly one defines `VssSnapshot`.
#[cfg(windows)]
mod windows_vss;
#[cfg(windows)]
pub use windows_vss::VssSnapshot;

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::VssSnapshot;

/// Why a VSS read could not be performed (DESIGN s5.3).
///
/// [`VssError::Unavailable`] is the GRACEFUL-DEGRADE signal: the caller skips
/// the locked file and surfaces `local.vss_unavailable` /
/// `local.file_locked`, exactly as it does today. The other variants are
/// genuine COM faults during snapshot creation, also non-fatal - they degrade
/// to the same skip + report.
#[derive(Debug, thiserror::Error)]
pub enum VssError {
    /// VSS is not available on this host/process: not Windows, not elevated,
    /// or `vss_mode = never`. The caller degrades to skip-the-locked-file.
    #[error("VSS unavailable: {0}")]
    Unavailable(&'static str),

    /// `CreateVssBackupComponents` (loaded from `vssapi.dll`) is missing or
    /// returned a failure HRESULT - e.g. `E_ACCESSDENIED` when the process
    /// lacks backup privileges despite the elevation check.
    #[error("VSS backup-components init failed: {0}")]
    Init(String),

    /// A step in the snapshot COM sequence (`SetContext`,
    /// `GatherWriterMetadata`, `StartSnapshotSet`, `AddToSnapshotSet`,
    /// `PrepareForBackup`, `DoSnapshotSet`, `GetSnapshotProperties`) failed.
    /// The field is named `detail` (not `source`) so thiserror does not treat
    /// the `String` as a `std::error::Error` source.
    #[error("VSS snapshot step '{step}' failed: {detail}")]
    Com {
        /// The COM step that failed (for diagnostics).
        step: &'static str,
        /// The underlying error string (HRESULT-derived).
        detail: String,
    },

    /// The volume letter passed to [`VssSnapshot::create`] could not be
    /// normalised to a `X:\` volume mount the snapshot can target.
    #[error("VSS: invalid volume '{0}'")]
    InvalidVolume(String),

    /// A live path could not be mapped under a snapshot root (it did not lie
    /// on the snapshotted volume).
    #[error("VSS: path '{0}' is not on the snapshot's volume")]
    PathNotOnVolume(String),
}

/// What the open path should do after consulting VSS for a locked file
/// (the output of [`fallback_decision`]).
///
/// This is the single behavioural contract the cross-OS table test pins: it
/// has no I/O, so it is exercised identically on Linux / macOS / Windows even
/// though a real `ERROR_SHARING_VIOLATION` + real snapshot only exist on an
/// elevated Windows host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackDecision {
    /// Open the live path directly - either the first open succeeded, or
    /// `vss_mode != always` and the file was not locked.
    OpenLive,
    /// Re-open the file at this snapshot-mapped path (under
    /// `\\?\GLOBALROOT\Device\...`).
    OpenSnapshot(PathBuf),
    /// VSS could not help (unavailable, or the snapshot/mapping failed):
    /// skip the file and surface it as locked.
    SkipLocked,
}

/// The result of a first, live open attempt that the open path feeds into
/// [`fallback_decision`]. Decouples the pure decision from the OS open call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAttempt {
    /// The live open succeeded - no VSS needed (unless `always`).
    Ok,
    /// The live open hit `ERROR_SHARING_VIOLATION` / a lock: the file is
    /// exclusively held and VSS is the only way to read it.
    Locked,
}

/// The provider's verdict for a volume, fed into [`fallback_decision`].
///
/// Separated from the decision so the provider's lazy per-volume snapshot
/// machinery (which DOES touch COM on Windows) stays out of the pure function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOutcome {
    /// VSS produced a usable snapshot and mapped the file to this path.
    Mapped(PathBuf),
    /// VSS is unavailable or snapshot creation/mapping failed.
    Unavailable,
}

/// Decide what the executor's open path does for one file, given the live
/// open result, the configured [`VssMode`], whether the process is elevated,
/// and (only when VSS would be consulted) the provider's [`SnapshotOutcome`].
///
/// This is the load-bearing degrade contract (ROADMAP M3.5 acceptance):
/// - `Never` mode never uses VSS: a locked file is always skipped.
/// - Not elevated: VSS is never available, so a locked file is skipped.
/// - `Auto`: live first; only a `Locked` file consults the snapshot.
/// - `Always`: route reads through the snapshot even when the live open would
///   have worked (SPEC s22 "paranoid"), but a successful live open with no
///   snapshot still falls back to the live bytes rather than failing.
///
/// `snapshot` is the provider's outcome for the file's volume; pass
/// [`SnapshotOutcome::Unavailable`] when VSS was not consulted (it is then
/// only read on the paths that actually need it, so a stray value is inert).
pub fn fallback_decision(
    open: OpenAttempt,
    mode: VssMode,
    elevated: bool,
    snapshot: SnapshotOutcome,
) -> FallbackDecision {
    match mode {
        // never: VSS is off entirely. A lock is an unconditional skip; an
        // unlocked file reads live.
        VssMode::Never => match open {
            OpenAttempt::Ok => FallbackDecision::OpenLive,
            OpenAttempt::Locked => FallbackDecision::SkipLocked,
        },

        // auto: live-first. Only a locked file consults VSS, and only when
        // elevation makes it available.
        VssMode::Auto => match open {
            OpenAttempt::Ok => FallbackDecision::OpenLive,
            OpenAttempt::Locked => {
                if !elevated {
                    return FallbackDecision::SkipLocked;
                }
                match snapshot {
                    SnapshotOutcome::Mapped(p) => FallbackDecision::OpenSnapshot(p),
                    SnapshotOutcome::Unavailable => FallbackDecision::SkipLocked,
                }
            }
        },

        // always: route EVERY read through the snapshot (paranoid), locked or
        // not. Without elevation a snapshot is impossible; an unlocked file
        // then still reads live (we never fail a readable file just because
        // paranoid mode could not snapshot), while a locked file is skipped.
        VssMode::Always => {
            if !elevated {
                return match open {
                    OpenAttempt::Ok => FallbackDecision::OpenLive,
                    OpenAttempt::Locked => FallbackDecision::SkipLocked,
                };
            }
            match snapshot {
                SnapshotOutcome::Mapped(p) => FallbackDecision::OpenSnapshot(p),
                // Snapshot creation failed: a readable file still reads live;
                // a locked one is skipped.
                SnapshotOutcome::Unavailable => match open {
                    OpenAttempt::Ok => FallbackDecision::OpenLive,
                    OpenAttempt::Locked => FallbackDecision::SkipLocked,
                },
            }
        }
    }
}

/// Returns `true` when the current process holds the Administrator elevation
/// that VSS snapshot creation requires.
///
/// Windows: `OpenProcessToken(GetCurrentProcess, TOKEN_QUERY)` +
/// `GetTokenInformation(TokenElevation)`. Any failure conservatively reads as
/// NOT elevated (so we degrade rather than attempt a snapshot that will fail
/// with `E_ACCESSDENIED`). Off Windows there is no VSS, so this is always
/// `false`.
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        windows_vss::is_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// A snapshot the COM sequence created and exposed, identified for the orphan
/// ledger (so a later run can release it if our process died before [`Drop`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotHandle {
    /// The shadow-copy GUID (`VSS_SNAPSHOT_PROP::m_SnapshotId`), stored as the
    /// canonical `{...}` braced string so it round-trips back into a `GUID`.
    pub snapshot_id: String,
    /// The `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN` device root the
    /// snapshot is exposed at; locked-file paths map under this.
    pub device_root: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapped(p: &str) -> SnapshotOutcome {
        SnapshotOutcome::Mapped(PathBuf::from(p))
    }

    #[test]
    fn never_mode_skips_locked_reads_unlocked_live_regardless_of_elevation() {
        for elevated in [true, false] {
            assert_eq!(
                fallback_decision(
                    OpenAttempt::Ok,
                    VssMode::Never,
                    elevated,
                    SnapshotOutcome::Unavailable
                ),
                FallbackDecision::OpenLive
            );
            assert_eq!(
                fallback_decision(
                    OpenAttempt::Locked,
                    VssMode::Never,
                    elevated,
                    mapped(r"\\?\GLOBALROOT\x")
                ),
                FallbackDecision::SkipLocked,
                "never mode never uses VSS even if a snapshot was somehow mapped"
            );
        }
    }

    #[test]
    fn auto_mode_unlocked_reads_live() {
        assert_eq!(
            fallback_decision(
                OpenAttempt::Ok,
                VssMode::Auto,
                true,
                SnapshotOutcome::Unavailable
            ),
            FallbackDecision::OpenLive
        );
    }

    #[test]
    fn auto_mode_locked_unelevated_skips() {
        assert_eq!(
            fallback_decision(
                OpenAttempt::Locked,
                VssMode::Auto,
                false,
                mapped(r"\\?\GLOBALROOT\x")
            ),
            FallbackDecision::SkipLocked,
            "no elevation -> no VSS -> skip"
        );
    }

    #[test]
    fn auto_mode_locked_elevated_with_snapshot_opens_snapshot() {
        assert_eq!(
            fallback_decision(
                OpenAttempt::Locked,
                VssMode::Auto,
                true,
                mapped(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\f.pst")
            ),
            FallbackDecision::OpenSnapshot(PathBuf::from(
                r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\f.pst"
            ))
        );
    }

    #[test]
    fn auto_mode_locked_elevated_snapshot_failed_skips() {
        assert_eq!(
            fallback_decision(
                OpenAttempt::Locked,
                VssMode::Auto,
                true,
                SnapshotOutcome::Unavailable
            ),
            FallbackDecision::SkipLocked,
            "elevated but snapshot creation failed -> degrade to skip"
        );
    }

    #[test]
    fn always_mode_routes_unlocked_reads_through_snapshot() {
        assert_eq!(
            fallback_decision(
                OpenAttempt::Ok,
                VssMode::Always,
                true,
                mapped(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\doc.txt")
            ),
            FallbackDecision::OpenSnapshot(PathBuf::from(
                r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\doc.txt"
            )),
            "always = paranoid: even a readable file is read from the snapshot"
        );
    }

    #[test]
    fn always_mode_unelevated_falls_back_to_live_for_readable_files() {
        assert_eq!(
            fallback_decision(
                OpenAttempt::Ok,
                VssMode::Always,
                false,
                SnapshotOutcome::Unavailable
            ),
            FallbackDecision::OpenLive,
            "paranoid mode must never fail a perfectly readable file for lack of elevation"
        );
        assert_eq!(
            fallback_decision(
                OpenAttempt::Locked,
                VssMode::Always,
                false,
                SnapshotOutcome::Unavailable
            ),
            FallbackDecision::SkipLocked
        );
    }

    #[test]
    fn always_mode_snapshot_failed_falls_back_to_live_for_readable_files() {
        assert_eq!(
            fallback_decision(
                OpenAttempt::Ok,
                VssMode::Always,
                true,
                SnapshotOutcome::Unavailable
            ),
            FallbackDecision::OpenLive
        );
        assert_eq!(
            fallback_decision(
                OpenAttempt::Locked,
                VssMode::Always,
                true,
                SnapshotOutcome::Unavailable
            ),
            FallbackDecision::SkipLocked
        );
    }

    #[test]
    fn off_windows_is_never_elevated() {
        #[cfg(not(windows))]
        assert!(!is_elevated());
    }
}
