//! [`BrokeredVssProvider`] - the app-side [`driven_vss::VssProvider`] that reads
//! locked files through the elevated helper instead of in-process (DESIGN
//! s5.3.1).
//!
//! It fits the EXISTING executor seam: `map_for_volume` streams the locked
//! file's bytes from the helper into a short-lived temp file the un-elevated app
//! owns and returns THAT path, so the executor's `read_path`
//! open/identity/encrypt/upload pipeline is unchanged. Temp copies are deleted
//! at `end_cycle`. Off Windows - or when the helper is not reachable - every
//! `map_for_volume` returns [`SnapshotOutcome::Unavailable`], so the executor
//! degrades to skip-the-locked-file exactly as it does with no elevation today.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use driven_vss::{SnapshotOutcome, VssMode, VssProvider};

use crate::launch::HelperLauncher;

/// The app-side provider that brokers VSS reads through the elevated helper.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct BrokeredVssProvider {
    mode: Mutex<VssMode>,
    /// The named pipe the helper serves on (shared with the launched helper).
    pipe_name: String,
    /// The app's install directory - the helper must be a sibling here, and the
    /// client verifies the server's image lives here.
    helper_dir: PathBuf,
    /// Where streamed temp copies of locked files land (app-owned).
    temp_dir: PathBuf,
    /// LIVENESS: whether the helper is currently believed reachable (a
    /// connection succeeded, or [`Self::probe`] confirmed it). Distinct from the
    /// [`VssProvider::available`] METHOD, which is a CAPABILITY check (can the
    /// helper be brought up on demand). Named `helper_live` so the two are never
    /// confused: the executor reads the capability method; the internal map/
    /// end-cycle gating reads THIS liveness flag so a cycle with no locked file
    /// never eats a connect-timeout reaching an unlaunched helper.
    helper_live: AtomicBool,
    /// The on-demand launch seam (DESIGN s5.3.1). `Some` in production: the FIRST
    /// locked file triggers [`HelperLauncher::ensure_launched`] (at-most-once, one
    /// UAC prompt); a launcher's presence also makes [`Self::available`] report
    /// the CAPABILITY true so the executor routes locked files here. `None` in the
    /// integration tests, which launch the server themselves and drive
    /// reachability via [`Self::probe`].
    launcher: Option<Arc<dyn HelperLauncher>>,
    /// Serialises helper access: locked files are rare, so one connection at a
    /// time keeps the server single-instance and simple.
    guard: Mutex<()>,
    /// Temp copies created this cycle, deleted at [`VssProvider::end_cycle`].
    temp_files: Mutex<Vec<PathBuf>>,
}

impl BrokeredVssProvider {
    /// Build a brokered provider. `helper_live` starts `false`; in production
    /// attach a launcher via [`Self::with_launcher`] (the first locked file then
    /// launches the helper on demand), or - in the integration tests - call
    /// [`Self::probe`] after launching the server yourself. Off Windows the
    /// provider is permanently unavailable.
    pub fn new(
        mode: VssMode,
        pipe_name: impl Into<String>,
        helper_dir: impl Into<PathBuf>,
        temp_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            mode: Mutex::new(mode),
            pipe_name: pipe_name.into(),
            helper_dir: helper_dir.into(),
            temp_dir: temp_dir.into(),
            helper_live: AtomicBool::new(false),
            launcher: None,
            guard: Mutex::new(()),
            temp_files: Mutex::new(Vec::new()),
        }
    }

    /// Attach the on-demand launch seam (DESIGN s5.3.1). With a launcher the
    /// provider reports the CAPABILITY available (so the executor consults it for
    /// locked files) and launches the elevated helper lazily, at-most-once, on
    /// the first locked file. The app hands the SAME `Arc<dyn HelperLauncher>` to
    /// every account's provider so there is ONE launch / one UAC prompt / one
    /// helper process for the whole session.
    #[must_use]
    pub fn with_launcher(mut self, launcher: Arc<dyn HelperLauncher>) -> Self {
        self.launcher = Some(launcher);
        self
    }

    fn current_mode(&self) -> VssMode {
        match self.mode.lock() {
            Ok(g) => *g,
            Err(p) => *p.into_inner(),
        }
    }

    /// The pipe name the helper serves on (so the launcher passes the SAME one).
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    /// Try to reach the helper (connect + handshake, then disconnect) and update
    /// the [`Self::helper_live`] liveness flag. Returns the new liveness. Off
    /// Windows: always `false`.
    pub fn probe(&self) -> bool {
        #[cfg(windows)]
        {
            let _g = self.guard.lock();
            match crate::client::HelperClient::connect(&self.pipe_name, &self.helper_dir) {
                Ok(_client) => {
                    self.helper_live.store(true, Ordering::SeqCst);
                    true
                }
                Err(e) => {
                    tracing::debug!(error = %e, "VSS helper: probe failed; unavailable");
                    self.helper_live.store(false, Ordering::SeqCst);
                    false
                }
            }
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// Ask the helper to shut down (release everything + exit). Best-effort;
    /// called at app shutdown. Off Windows: no-op.
    pub fn shutdown_helper(&self) {
        #[cfg(windows)]
        {
            let _g = self.guard.lock();
            if let Ok(mut c) =
                crate::client::HelperClient::connect(&self.pipe_name, &self.helper_dir)
            {
                let _ = c.shutdown();
            }
        }
    }

    #[cfg(windows)]
    fn map_via_helper(&self, live_path: &std::path::Path) -> SnapshotOutcome {
        use std::io::Write;

        let Some(volume) = crate::validate::drive_of(live_path) else {
            tracing::warn!(path = %live_path.display(), "VSS helper: no drive letter; degrading");
            return SnapshotOutcome::Unavailable;
        };

        let _g = self.guard.lock();
        let mut client =
            match crate::client::HelperClient::connect(&self.pipe_name, &self.helper_dir) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "VSS helper: connect failed; degrading to skip");
                    self.helper_live.store(false, Ordering::SeqCst);
                    return SnapshotOutcome::Unavailable;
                }
            };
        // A successful connect confirms the helper is live (the launch path only
        // knows the process was STARTED; this is the first proof it is serving).
        self.helper_live.store(true, Ordering::SeqCst);

        let size = match client.open_locked(&volume, &live_path.to_string_lossy()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, path = %live_path.display(), "VSS helper: open failed; degrading to skip");
                return SnapshotOutcome::Unavailable;
            }
        };

        // Stream the bytes into an app-owned temp file the executor can read.
        if let Err(e) = std::fs::create_dir_all(&self.temp_dir) {
            tracing::warn!(error = %e, "VSS helper: temp dir create failed; degrading");
            return SnapshotOutcome::Unavailable;
        }
        let temp = self
            .temp_dir
            .join(format!("driven-vss-{}.tmp", uuid::Uuid::new_v4().simple()));
        let mut out = match std::fs::File::create(&temp) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "VSS helper: temp file create failed; degrading");
                return SnapshotOutcome::Unavailable;
            }
        };
        let copied = match std::io::copy(&mut client, &mut out) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, path = %live_path.display(), "VSS helper: stream failed; degrading");
                let _ = std::fs::remove_file(&temp);
                return SnapshotOutcome::Unavailable;
            }
        };
        let _ = out.flush();
        let _ = client.close_file();
        drop(client);

        if copied != size {
            tracing::warn!(
                copied,
                expected = size,
                path = %live_path.display(),
                "VSS helper: streamed byte count did not match reported size; degrading"
            );
            let _ = std::fs::remove_file(&temp);
            return SnapshotOutcome::Unavailable;
        }

        self.temp_files
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(temp.clone());
        SnapshotOutcome::Mapped(temp)
    }
}

impl VssProvider for BrokeredVssProvider {
    fn map_for_volume(&self, live_path: &std::path::Path) -> SnapshotOutcome {
        if self.current_mode() == VssMode::Never {
            return SnapshotOutcome::Unavailable;
        }
        #[cfg(windows)]
        {
            // DESIGN s5.3.1: consult the launcher for the broker's readiness. The
            // lazy first-locked-file launch is TRIGGERED here (non-blocking) for
            // the boot-already-on case; the eager enable-toggle path launched
            // ahead of time. Without a launcher (integration tests) require a prior
            // `probe`.
            match &self.launcher {
                Some(launcher) => {
                    use crate::launch::LaunchStatus;
                    match launcher.launch_status() {
                        // Broker up: read the locked file through it.
                        LaunchStatus::Ready => self.map_via_helper(live_path),
                        // Launch in progress (awaiting UAC / pipe coming up): retry
                        // next cycle rather than reporting a permanent lock. Never
                        // blocks here.
                        LaunchStatus::Pending => SnapshotOutcome::Pending,
                        // Declined or disabled: degrade to the historical skip.
                        LaunchStatus::Declined | LaunchStatus::Disabled => {
                            SnapshotOutcome::Unavailable
                        }
                    }
                }
                None => {
                    if self.helper_live.load(Ordering::SeqCst) {
                        self.map_via_helper(live_path)
                    } else {
                        SnapshotOutcome::Unavailable
                    }
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = live_path;
            SnapshotOutcome::Unavailable
        }
    }

    fn mode(&self) -> VssMode {
        self.current_mode()
    }

    fn set_mode(&self, mode: VssMode) {
        match self.mode.lock() {
            Ok(mut g) => *g = mode,
            Err(p) => *p.into_inner() = mode,
        }
    }

    fn available(&self) -> bool {
        // CAPABILITY (not liveness): can VSS help for a locked file this run? The
        // executor reads this as its `elevated` input to `fallback_decision`, so
        // it must be `true` whenever the broker is up OR can still be brought up,
        // and `false` once disabled / declined (so the provider then behaves like
        // the un-elevated skip). With a launcher this is the launcher's capability;
        // without one (integration tests) it is observed liveness.
        if !cfg!(windows) || self.current_mode() == VssMode::Never {
            return false;
        }
        match &self.launcher {
            Some(launcher) => launcher.is_available(),
            None => self.helper_live.load(Ordering::SeqCst),
        }
    }

    fn end_cycle(&self) {
        // Delete this cycle's temp copies.
        let files: Vec<PathBuf> = {
            let mut guard = self.temp_files.lock().unwrap_or_else(|p| p.into_inner());
            guard.drain(..).collect()
        };
        for f in &files {
            let _ = std::fs::remove_file(f);
        }
        // Tell the helper to release its per-cycle snapshots (best-effort). Gated
        // on LIVENESS (`helper_live`), not the capability method: a cycle that
        // never touched a locked file never launched/reached the helper, so we
        // must not connect (and eat the connect-timeout) just to release nothing.
        #[cfg(windows)]
        {
            if self.helper_live.load(Ordering::SeqCst) {
                let _g = self.guard.lock();
                if let Ok(mut c) =
                    crate::client::HelperClient::connect(&self.pipe_name, &self.helper_dir)
                {
                    let _ = c.end_cycle();
                }
            }
        }
    }

    // recorded_snapshots(): the helper owns the real shadow copies under a
    // VSS_CTX_BACKUP (auto-release) context bounded by the helper process
    // lifetime, so the app keeps no cross-process orphan ledger - the default
    // empty impl is correct here.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::LaunchStatus;

    #[test]
    fn never_mode_and_unprobed_are_unavailable_and_degrade() {
        let p = BrokeredVssProvider::new(
            VssMode::Auto,
            r"\\.\pipe\x",
            r"C:\app",
            std::env::temp_dir(),
        );
        // Not probed => unavailable => degrade regardless of OS.
        assert!(!p.available());
        assert_eq!(
            p.map_for_volume(std::path::Path::new(r"C:\Users\me\Documents\f.pst")),
            SnapshotOutcome::Unavailable
        );

        let n = BrokeredVssProvider::new(
            VssMode::Never,
            r"\\.\pipe\x",
            r"C:\app",
            std::env::temp_dir(),
        );
        assert!(!n.available(), "never mode is never available");
    }

    #[test]
    fn set_mode_never_disables() {
        let p = BrokeredVssProvider::new(
            VssMode::Auto,
            r"\\.\pipe\x",
            r"C:\app",
            std::env::temp_dir(),
        );
        p.set_mode(VssMode::Never);
        assert_eq!(p.mode(), VssMode::Never);
        assert!(!p.available());
    }

    #[test]
    fn end_cycle_is_safe_with_no_temp_files() {
        let p = BrokeredVssProvider::new(
            VssMode::Auto,
            r"\\.\pipe\x",
            r"C:\app",
            std::env::temp_dir(),
        );
        p.end_cycle(); // must not panic
    }

    #[test]
    fn end_cycle_deletes_recorded_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", dir.path());
        // Simulate a streamed temp copy.
        let temp = dir.path().join("driven-vss-fake.tmp");
        std::fs::write(&temp, b"stale").unwrap();
        p.temp_files.lock().unwrap().push(temp.clone());
        assert!(temp.exists());
        p.end_cycle();
        assert!(!temp.exists(), "end_cycle must delete temp copies");
    }

    /// A deterministic [`HelperLauncher`] for the launch-seam tests: returns a
    /// configured [`LaunchStatus`] + capability, and counts `launch_status` calls.
    struct FakeLauncher {
        status: LaunchStatus,
        available: bool,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl FakeLauncher {
        fn new(status: LaunchStatus, available: bool) -> Arc<Self> {
            Arc::new(Self {
                status,
                available,
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    impl HelperLauncher for FakeLauncher {
        fn launch_status(&self) -> LaunchStatus {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.status
        }
        fn is_available(&self) -> bool {
            self.available
        }
    }

    /// With a launcher attached, `available()` delegates to the launcher's
    /// CAPABILITY (VSS can be brought up on demand) - on Windows, true when the
    /// launcher reports available even before any launch. `never` mode and
    /// non-Windows are still unavailable regardless.
    #[test]
    fn launcher_available_delegates_to_capability() {
        let launcher = FakeLauncher::new(LaunchStatus::Pending, true);
        let p = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(launcher);
        // Capability: the launcher says available, so true on Windows (VSS is a
        // Windows capability), false elsewhere.
        assert_eq!(p.available(), cfg!(windows));

        // A launcher reporting NOT available (disabled/declined) is unavailable.
        let d = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(FakeLauncher::new(LaunchStatus::Declined, false));
        assert!(
            !d.available(),
            "a declined/disabled launcher is not available"
        );

        // `never` mode is never available regardless of the launcher.
        let n = BrokeredVssProvider::new(VssMode::Never, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(FakeLauncher::new(LaunchStatus::Ready, true));
        assert!(!n.available(), "never mode is never available");
    }

    /// A launcher reporting `Pending` maps to [`SnapshotOutcome::Pending`] (skip +
    /// retry next cycle), while `Declined`/`Disabled` map to `Unavailable` (the
    /// historical skip). Both are non-blocking.
    #[test]
    fn launch_status_maps_to_snapshot_outcome() {
        let path = std::path::Path::new(r"C:\Users\me\Documents\f.pst");

        // Pending -> Pending (only meaningful on Windows; off Windows the map
        // short-circuits to Unavailable before consulting the launcher).
        let pending = FakeLauncher::new(LaunchStatus::Pending, true);
        let pp = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(pending.clone());
        let expect_pending = if cfg!(windows) {
            SnapshotOutcome::Pending
        } else {
            SnapshotOutcome::Unavailable
        };
        assert_eq!(pp.map_for_volume(path), expect_pending);

        // Declined -> Unavailable.
        let declined = FakeLauncher::new(LaunchStatus::Declined, false);
        let dp = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(declined);
        assert_eq!(dp.map_for_volume(path), SnapshotOutcome::Unavailable);

        // On Windows the launcher IS consulted on the locked path; off Windows the
        // map short-circuits before it.
        assert!(pending.calls() <= 1);
    }

    /// End-to-end executor DECISION for a locked file while the helper is PENDING.
    /// Mirrors the executor's `open_effective`: read `elevated = provider.available()`
    /// and `provider.map_for_volume(..)`, feed both into the REAL
    /// [`driven_vss::fallback_decision`]. A pending launch yields `SkipRetryLater`
    /// (retry next cycle) on Windows - NOT a permanent lock.
    #[test]
    fn executor_decision_for_locked_file_while_pending_retries_later() {
        use driven_vss::{fallback_decision, FallbackDecision, OpenAttempt};

        let p = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(FakeLauncher::new(LaunchStatus::Pending, true));
        let elevated = p.available();
        let snapshot = p.map_for_volume(std::path::Path::new(r"C:\Users\me\Outlook.pst"));
        let decision = fallback_decision(OpenAttempt::Locked, VssMode::Auto, elevated, snapshot);
        if cfg!(windows) {
            assert_eq!(
                decision,
                FallbackDecision::SkipRetryLater,
                "a pending helper must retry next cycle, not report a permanent lock"
            );
        } else {
            // Off Windows the provider is not available; a locked file skips.
            assert_eq!(decision, FallbackDecision::SkipLocked);
        }
    }

    /// A DECLINED helper degrades a locked file to the graceful skip (SkipLocked).
    #[test]
    fn executor_decision_for_locked_file_via_declined_helper_is_skip() {
        use driven_vss::{fallback_decision, FallbackDecision, OpenAttempt};

        let p = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(FakeLauncher::new(LaunchStatus::Declined, false));
        let elevated = p.available();
        let snapshot = p.map_for_volume(std::path::Path::new(r"C:\Users\me\Outlook.pst"));
        assert_eq!(snapshot, SnapshotOutcome::Unavailable);
        assert_eq!(
            fallback_decision(OpenAttempt::Locked, VssMode::Auto, elevated, snapshot),
            FallbackDecision::SkipLocked,
            "a declined helper degrades a locked file to a skip"
        );
    }

    /// The positive executor DECISION: a `Mapped` outcome + capability opens the
    /// snapshot copy (cross-OS via the real `fallback_decision`).
    #[test]
    fn executor_decision_for_locked_file_with_mapped_snapshot_opens_snapshot() {
        use driven_vss::{fallback_decision, FallbackDecision, OpenAttempt};

        let p = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(FakeLauncher::new(LaunchStatus::Ready, true));
        let elevated = p.available();
        let mapped = SnapshotOutcome::Mapped(std::path::PathBuf::from(
            r"C:\Users\me\driven-vss-tmp\Outlook.pst",
        ));
        let decision = fallback_decision(OpenAttempt::Locked, VssMode::Auto, elevated, mapped);
        if cfg!(windows) {
            assert!(matches!(decision, FallbackDecision::OpenSnapshot(_)));
        } else {
            assert_eq!(decision, FallbackDecision::SkipLocked);
        }
    }
}
