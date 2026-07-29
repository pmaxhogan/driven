//! [`ApfsBrokeredProvider`] - the macOS implementation of the
//! [`driven_vss::VssProvider`] seam (DESIGN s5.3.2).
//!
//! Lifecycle mirrors the Windows contract exactly (the CYCLE owns snapshots;
//! `end_cycle` releases on every exit path). Per cycle:
//! - first locked file: create ONE APFS local snapshot (unprivileged,
//!   covers every TM-set volume at once), record it for the orphan ledger;
//! - per volume: broker-mount that snapshot once, cache the mountpoint;
//! - per file: firmlink-aware map under the mountpoint ([`crate::paths`]);
//! - `end_cycle`: broker-unmount everything and delete the snapshot
//!   (deterministic cleanup; APFS auto-thinning is the crash backstop).
//!
//! Off macOS every operation reports unavailable, exactly like the VSS stub.

// Off macOS the provider is an always-unavailable shell, so these are only
// reachable from the macOS paths or from the cross-OS tests.
#[cfg(any(target_os = "macos", test))]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use driven_vss::{RecordedSnapshot, SnapshotOutcome, SnapshotRecorder, VssMode, VssProvider};

#[cfg(any(target_os = "macos", test))]
use crate::launch::HelperLaunchStatus;
use crate::launch::HelperLauncher;

/// Per-cycle state (all torn down by `end_cycle`).
#[derive(Default)]
struct CycleState {
    /// The one snapshot this cycle created (its `tmutil` date stamp).
    snapshot_date: Option<String>,
    /// When that snapshot was created, Unix epoch ms. MUST be the real time:
    /// the orphan ledger prunes on age (`OrphanRegistry::orphans_older_than`),
    /// so a placeholder 0 would date every snapshot to 1970 and mark it
    /// instantly sweepable.
    snapshot_created_ms: i64,
    /// Volume mount point -> broker mountpoint for that snapshot. Only the
    /// macOS path mounts anything, so off macOS this would be dead weight.
    #[cfg(target_os = "macos")]
    mounts: std::collections::HashMap<PathBuf, PathBuf>,
    /// The connected broker client (kept for the cycle once established).
    #[cfg(target_os = "macos")]
    client: Option<crate::client::HelperClient>,
}

/// The macOS snapshot provider. Cross-OS compiling; genuinely functional only
/// on macOS with a launched broker.
pub struct ApfsBrokeredProvider {
    mode: Mutex<VssMode>,
    launcher: Arc<dyn HelperLauncher>,
    recorder: Mutex<Option<SnapshotRecorder>>,
    cycle: Mutex<CycleState>,
}

impl ApfsBrokeredProvider {
    /// A provider driving the broker behind `launcher`, in `mode`.
    pub fn new(launcher: Arc<dyn HelperLauncher>, mode: VssMode) -> Self {
        Self {
            mode: Mutex::new(mode),
            launcher,
            recorder: Mutex::new(None),
            cycle: Mutex::new(CycleState::default()),
        }
    }

    /// The snapshot id recorded in the orphan ledger for an APFS snapshot:
    /// namespaced so Windows-GUID cleanup code never confuses the two.
    pub fn ledger_id(date: &str) -> String {
        format!("apfs:{date}")
    }

    #[cfg(target_os = "macos")]
    fn map_locked(&self, live_path: &std::path::Path) -> SnapshotOutcome {
        use crate::{client::HelperClient, paths, snapshot};

        let mut cycle = self.cycle.lock().expect("apfs cycle lock");

        // Broker connection (once per cycle; a connect failure right after a
        // successful launch is the broker still binding - transient).
        if cycle.client.is_none() {
            match HelperClient::connect(&self.launcher.socket_path()) {
                Ok(c) => cycle.client = Some(c),
                Err(crate::client::ClientError::NotRoot) => {
                    // A squatter answered. Never speak, never retry blindly.
                    tracing::warn!(
                        "apfs broker socket peer was not root; disabling for this cycle"
                    );
                    return SnapshotOutcome::Unavailable;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "apfs broker not connectable yet");
                    return SnapshotOutcome::Pending;
                }
            }
        }

        // One unprivileged snapshot per cycle.
        if cycle.snapshot_date.is_none() {
            match snapshot::create_local_snapshot() {
                Ok(date) => {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    if let Some(rec) = self.recorder.lock().expect("recorder lock").as_ref() {
                        rec(&Self::ledger_id(&date), now_ms);
                    }
                    cycle.snapshot_date = Some(date);
                    cycle.snapshot_created_ms = now_ms;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "tmutil localsnapshot failed");
                    return SnapshotOutcome::Unavailable;
                }
            }
        }
        let date = cycle.snapshot_date.clone().expect("just ensured");
        let name = snapshot::snapshot_name_for_date(&date);

        // Volume resolution + one broker mount per volume per cycle.
        let volume = match paths::resolve_volume_mount(live_path) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, "statfs failed for locked file");
                return SnapshotOutcome::Unavailable;
            }
        };
        if !cycle.mounts.contains_key(&volume) {
            let client = cycle.client.as_mut().expect("client ensured above");
            match client.mount_snapshot(&volume.to_string_lossy(), &name) {
                Ok(mp) => {
                    cycle.mounts.insert(volume.clone(), PathBuf::from(mp));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "broker mount failed");
                    return SnapshotOutcome::Unavailable;
                }
            }
        }
        let mountpoint = cycle.mounts.get(&volume).expect("just ensured").clone();
        drop(cycle);

        match paths::snapshot_path_for(live_path, &volume, &mountpoint) {
            Some(mapped) => SnapshotOutcome::Mapped(mapped),
            None => SnapshotOutcome::Unavailable,
        }
    }
}

impl VssProvider for ApfsBrokeredProvider {
    fn map_for_volume(&self, live_path: &std::path::Path) -> SnapshotOutcome {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = live_path;
            SnapshotOutcome::Unavailable
        }
        #[cfg(target_os = "macos")]
        {
            match self.launcher.launch_status() {
                HelperLaunchStatus::Ready => self.map_locked(live_path),
                HelperLaunchStatus::Pending => SnapshotOutcome::Pending,
                HelperLaunchStatus::Declined | HelperLaunchStatus::Disabled => {
                    SnapshotOutcome::Unavailable
                }
            }
        }
    }

    fn mode(&self) -> VssMode {
        *self.mode.lock().expect("apfs mode lock")
    }

    fn set_mode(&self, mode: VssMode) {
        *self.mode.lock().expect("apfs mode lock") = mode;
    }

    fn available(&self) -> bool {
        cfg!(target_os = "macos") && self.launcher.is_available()
    }

    fn set_recorder(&self, recorder: SnapshotRecorder) {
        *self.recorder.lock().expect("recorder lock") = Some(recorder);
    }

    fn end_cycle(&self) {
        let mut cycle = self.cycle.lock().expect("apfs cycle lock");
        #[cfg(target_os = "macos")]
        {
            let date_to_delete = cycle.snapshot_date.clone();
            // Unmount FIRST (privileged, via the broker), then delete the
            // snapshot ourselves - deletion needs no privilege, so it never
            // goes near the root process.
            if let Some(client) = cycle.client.as_mut() {
                if let Err(e) = client.unmount_all() {
                    tracing::warn!(error = %e, "broker unmount_all failed at end of cycle");
                }
            }
            if let Some(date) = date_to_delete.as_deref() {
                if let Err(e) = crate::snapshot::delete_local_snapshot(date) {
                    // Non-fatal: APFS auto-thinning is the backstop, and the
                    // ledger keeps the date for a later deterministic sweep.
                    tracing::debug!(error = %e, "snapshot delete failed");
                }
            }
        }
        *cycle = CycleState::default();
    }

    fn recorded_snapshots(&self) -> Vec<RecordedSnapshot> {
        let cycle = self.cycle.lock().expect("apfs cycle lock");
        cycle
            .snapshot_date
            .as_deref()
            .map(|date| {
                vec![RecordedSnapshot {
                    snapshot_id: Self::ledger_id(date),
                    created_at_ms: cycle.snapshot_created_ms,
                }]
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    struct FixedLauncher(HelperLaunchStatus, bool);
    impl HelperLauncher for FixedLauncher {
        fn launch_status(&self) -> HelperLaunchStatus {
            self.0
        }
        fn is_available(&self) -> bool {
            self.1
        }
        fn socket_path(&self) -> PathBuf {
            PathBuf::from("/nonexistent/driven-apfs-test.sock")
        }
    }

    #[test]
    fn declined_and_disabled_report_unavailable() {
        for st in [HelperLaunchStatus::Declined, HelperLaunchStatus::Disabled] {
            let p = ApfsBrokeredProvider::new(Arc::new(FixedLauncher(st, false)), VssMode::Auto);
            assert_eq!(
                p.map_for_volume(Path::new("/Users/x/f")),
                SnapshotOutcome::Unavailable
            );
            assert!(!p.available());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pending_launch_reports_pending() {
        let p = ApfsBrokeredProvider::new(
            Arc::new(FixedLauncher(HelperLaunchStatus::Pending, true)),
            VssMode::Auto,
        );
        assert_eq!(
            p.map_for_volume(Path::new("/Users/x/f")),
            SnapshotOutcome::Pending
        );
        assert!(p.available());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ready_with_no_broker_socket_is_pending_not_a_hang() {
        // Ready launcher but nothing listening: the connect fails fast and
        // the file skips transiently.
        let p = ApfsBrokeredProvider::new(
            Arc::new(FixedLauncher(HelperLaunchStatus::Ready, true)),
            VssMode::Auto,
        );
        assert_eq!(
            p.map_for_volume(Path::new("/Users/x/f")),
            SnapshotOutcome::Pending
        );
    }

    #[test]
    fn mode_round_trips_and_end_cycle_resets() {
        let p = ApfsBrokeredProvider::new(
            Arc::new(FixedLauncher(HelperLaunchStatus::Disabled, false)),
            VssMode::Auto,
        );
        assert_eq!(p.mode(), VssMode::Auto);
        p.set_mode(VssMode::Always);
        assert_eq!(p.mode(), VssMode::Always);
        p.end_cycle();
        assert!(p.recorded_snapshots().is_empty());
    }

    #[test]
    fn ledger_id_is_namespaced() {
        assert_eq!(
            ApfsBrokeredProvider::ledger_id("2026-07-29-154532"),
            "apfs:2026-07-29-154532"
        );
    }
}
