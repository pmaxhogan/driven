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
            // Launch-on-demand (DESIGN s5.3.1): with a launcher, the FIRST locked
            // file brings the elevated helper up (at-most-once, one UAC prompt).
            // A declined/failed launch degrades this file to skip; the launcher
            // memoises the failure so the next locked file does not re-prompt.
            // Without a launcher (integration tests), require a prior `probe`.
            match &self.launcher {
                Some(launcher) => {
                    if !launcher.ensure_launched() {
                        self.helper_live.store(false, Ordering::SeqCst);
                        return SnapshotOutcome::Unavailable;
                    }
                }
                None => {
                    if !self.helper_live.load(Ordering::SeqCst) {
                        return SnapshotOutcome::Unavailable;
                    }
                }
            }
            self.map_via_helper(live_path)
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
        // CAPABILITY (not liveness): can VSS help for a locked file this run?
        // The executor reads this as its `elevated` input to `fallback_decision`,
        // so it must be `true` whenever the helper can be brought up on demand -
        // BEFORE the lazy launch actually happens - or a locked file would be
        // skipped before the launch is ever attempted. With a launcher attached,
        // the helper is launchable, so capability is true (Windows, mode != never).
        // Without a launcher (integration tests) fall back to observed liveness.
        if !cfg!(windows) || self.current_mode() == VssMode::Never {
            return false;
        }
        self.launcher.is_some() || self.helper_live.load(Ordering::SeqCst)
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
    /// configured verdict and counts calls (so a test can assert `ensure_launched`
    /// is memoised at the MANAGER, not re-fired per file).
    struct FakeLauncher {
        verdict: bool,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl FakeLauncher {
        fn new(verdict: bool) -> Arc<Self> {
            Arc::new(Self {
                verdict,
                calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    impl HelperLauncher for FakeLauncher {
        fn ensure_launched(&self) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.verdict
        }
    }

    /// With a launcher attached, `available()` reports the CAPABILITY (VSS can be
    /// brought up on demand) BEFORE any launch - on Windows it is true even with
    /// `helper_live` still false, so the executor routes a locked file here rather
    /// than pre-skipping it. `never` mode and non-Windows are still unavailable.
    #[test]
    fn launcher_makes_available_report_capability() {
        let launcher = FakeLauncher::new(true);
        let p = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(launcher);
        // Liveness has NOT been established yet.
        assert!(!p.helper_live.load(Ordering::SeqCst));
        // Capability: true on Windows (launchable), false elsewhere.
        assert_eq!(p.available(), cfg!(windows));

        // `never` mode is never available regardless of the launcher.
        let n = BrokeredVssProvider::new(VssMode::Never, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(FakeLauncher::new(true));
        assert!(!n.available(), "never mode is never available");
    }

    /// A launcher whose `ensure_launched` returns `false` (UAC declined / helper
    /// exe missing) degrades every `map_for_volume` to `Unavailable` - the
    /// skip-the-locked-file path - and never establishes liveness.
    #[test]
    fn declined_launch_degrades_map() {
        let launcher = FakeLauncher::new(false);
        let p = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(launcher.clone());
        assert_eq!(
            p.map_for_volume(std::path::Path::new(r"C:\Users\me\Documents\f.pst")),
            SnapshotOutcome::Unavailable
        );
        assert!(
            !p.helper_live.load(Ordering::SeqCst),
            "a declined launch must not mark the helper live"
        );
        // On Windows the launcher is consulted on the locked path; off Windows the
        // map short-circuits before it (no real pipe), so only assert it was NOT
        // over-called.
        assert!(launcher.calls() <= 1);
    }

    /// End-to-end executor DECISION for a locked file backed by a declined helper.
    /// This mirrors the executor's `open_effective` locked-file path (driven-core
    /// executor.rs): it reads `elevated = provider.available()` and
    /// `provider.map_for_volume(..)`, then feeds BOTH into the REAL
    /// [`driven_vss::fallback_decision`]. With a launcher attached the capability
    /// is reported available (so the executor consults the provider instead of
    /// pre-skipping), but a DECLINED launch degrades the map to `Unavailable`, so
    /// the decision is `SkipLocked` - the graceful skip the executor emits. This
    /// pins the executor -> brokered-provider -> skip wiring without inverting the
    /// crate layering (driven-vss-helper already tests against `driven-vss`).
    #[test]
    fn executor_decision_for_locked_file_via_declined_helper_is_skip() {
        use driven_vss::{fallback_decision, FallbackDecision, OpenAttempt};

        let launcher = FakeLauncher::new(false);
        let p = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(launcher);

        // The two inputs the executor captures for a locked file, in order.
        let elevated = p.available();
        let snapshot = p.map_for_volume(std::path::Path::new(r"C:\Users\me\Outlook.pst"));
        assert_eq!(
            snapshot,
            SnapshotOutcome::Unavailable,
            "a declined helper cannot map the locked file"
        );
        assert_eq!(
            fallback_decision(OpenAttempt::Locked, VssMode::Auto, elevated, snapshot),
            FallbackDecision::SkipLocked,
            "a locked file through a declined helper must degrade to a skip"
        );
    }

    /// The positive executor DECISION: when the map SUCCEEDS (a `Mapped` outcome),
    /// the executor opens the snapshot copy. Proven here with a fake mapped
    /// outcome fed through the real `fallback_decision`, so the
    /// capability-`available()` -> `OpenSnapshot` wiring is pinned cross-OS (the
    /// real byte stream is the elevation-gated integration test).
    #[test]
    fn executor_decision_for_locked_file_with_mapped_snapshot_opens_snapshot() {
        use driven_vss::{fallback_decision, FallbackDecision, OpenAttempt};

        // A provider that reports capability available (launcher attached).
        let p = BrokeredVssProvider::new(VssMode::Auto, r"\\.\pipe\x", r"C:\app", "temp")
            .with_launcher(FakeLauncher::new(true));
        let elevated = p.available();
        // Simulate the provider's successful map (the real map streams via the
        // helper on Windows; here we assert the DECISION the executor makes given
        // a Mapped outcome + the capability flag).
        let mapped = SnapshotOutcome::Mapped(std::path::PathBuf::from(
            r"C:\Users\me\driven-vss-tmp\Outlook.pst",
        ));
        // On Windows the capability is true, so a Mapped snapshot opens the copy.
        // Off Windows the capability is false (no VSS), so the same inputs skip -
        // exactly the platform contract.
        let decision = fallback_decision(OpenAttempt::Locked, VssMode::Auto, elevated, mapped);
        if cfg!(windows) {
            assert!(matches!(decision, FallbackDecision::OpenSnapshot(_)));
        } else {
            assert_eq!(decision, FallbackDecision::SkipLocked);
        }
    }
}
