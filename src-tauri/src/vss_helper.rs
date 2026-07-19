//! [`VssHelperManager`]: the app-side lifecycle owner for the least-privilege
//! VSS helper (DESIGN s5.3.1, issue #25).
//!
//! VSS snapshot creation needs Administrator rights. Rather than elevate the
//! whole backup app, Driven elevates ONLY the shadow-copy operation: a small
//! privileged broker (`driven-vss-helper.exe`, bundled as a sidecar next to the
//! app) creates the snapshot and streams the locked file's bytes back over a
//! secured named pipe. This manager owns the ONE broker per app session:
//!
//! - it generates the unguessable pipe name ONCE at construction;
//! - it launches the broker ELEVATED (`ShellExecute runas` -> one UAC prompt)
//!   ON DEMAND - the FIRST time a locked file needs it - via
//!   [`HelperLauncher::ensure_launched`], and MEMOISES the outcome so a second
//!   account's provider (or a later locked file) does not raise a second UAC
//!   prompt, and a user who DECLINED is never re-prompted this session;
//! - it fixes the helper's allow-list of snapshot-able roots at launch (the
//!   union of the configured source roots the app passes on the command line),
//!   so the untrusted app can only ever ask the broker for files the user
//!   already told Driven to back up;
//! - it shuts the broker down on app quit (best-effort `Shutdown` over the
//!   pipe), so no elevated process outlives the session.
//!
//! It is built ONCE in [`crate::assembly::build_and_spawn`] (only on Windows,
//! only when the app is NOT already elevated and the `windows.vss_helper`
//! setting is on) and the SAME `Arc` is handed to every account's
//! [`driven_vss_helper::BrokeredVssProvider`] as its `Arc<dyn HelperLauncher>`,
//! which is what guarantees a single launch / single UAC prompt / single pipe
//! across all accounts.
//!
//! The launch call itself is injected ([`VssHelperManager::with_launch_fn`]) so
//! the memoisation logic is unit-tested cross-OS without a real `runas`/UAC
//! prompt; production uses [`driven_vss_helper::launch::launch_elevated`].

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use driven_vss_helper::launch::{generate_pipe_name, helper_args, launch_elevated, HelperLauncher};

/// The bundled sidecar file name (tauri installs the externalBin next to the
/// app executable with the target-triple suffix stripped).
const HELPER_EXE_NAME: &str = "driven-vss-helper.exe";

/// The injected elevated-launch call. Real production value is
/// [`driven_vss_helper::launch::launch_elevated`]; tests inject a fake so the
/// at-most-once memoisation is exercised without a real UAC prompt.
type LaunchFn = Box<dyn Fn(&Path, &[String]) -> Result<(), String> + Send + Sync>;

/// Whether the broker has been launched this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchState {
    /// No launch attempted yet - the first locked file will attempt one.
    NotAttempted,
    /// The broker was launched (the process was STARTED); its liveness beyond
    /// that is proven by a provider connect.
    Launched,
    /// A launch was attempted and FAILED (UAC declined / exe missing); we do NOT
    /// re-attempt this session (no repeat UAC prompts).
    Failed,
}

/// The app-side owner of the least-privilege VSS helper broker (DESIGN s5.3.1).
pub struct VssHelperManager {
    /// The unguessable pipe the broker serves on (generated once per session).
    pipe_name: String,
    /// Absolute path to the bundled `driven-vss-helper.exe` sidecar.
    helper_exe: PathBuf,
    /// The install directory (the sidecar's parent) - the provider's client
    /// verifies the broker's server image lives here.
    helper_dir: PathBuf,
    /// App-owned scratch dir where the provider streams locked-file temp copies.
    temp_dir: PathBuf,
    /// The allow-list of snapshot-able roots fixed at launch (union of the
    /// configured source roots), passed to the broker on its command line.
    allowed_roots: Vec<PathBuf>,
    /// The injected elevated-launch call (real `runas` in production).
    launch: LaunchFn,
    /// At-most-once launch memo.
    state: Mutex<LaunchState>,
}

impl VssHelperManager {
    /// Build a manager for the bundled sidecar at `helper_exe`, streaming temp
    /// copies under `temp_dir`, allowing the broker to snapshot files under
    /// `allowed_roots`. Uses the real elevated `runas` launch.
    #[must_use]
    pub fn new(
        helper_exe: impl Into<PathBuf>,
        temp_dir: impl Into<PathBuf>,
        allowed_roots: Vec<PathBuf>,
    ) -> Self {
        Self::with_launch_fn(
            helper_exe,
            temp_dir,
            allowed_roots,
            Box::new(launch_elevated),
        )
    }

    /// Test seam: build a manager with an INJECTED launch call so the
    /// at-most-once memoisation + degrade logic is unit-tested without a real
    /// UAC prompt.
    #[must_use]
    pub fn with_launch_fn(
        helper_exe: impl Into<PathBuf>,
        temp_dir: impl Into<PathBuf>,
        allowed_roots: Vec<PathBuf>,
        launch: LaunchFn,
    ) -> Self {
        let helper_exe = helper_exe.into();
        let helper_dir = helper_exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            pipe_name: generate_pipe_name(),
            helper_exe,
            helper_dir,
            temp_dir: temp_dir.into(),
            allowed_roots,
            launch,
            state: Mutex::new(LaunchState::NotAttempted),
        }
    }

    /// Resolve the bundled sidecar path for the CURRENT app executable: the
    /// `driven-vss-helper.exe` installed next to `driven-app.exe`. `None` if the
    /// current-exe path cannot be resolved.
    #[must_use]
    pub fn bundled_helper_exe() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        Some(dir.join(HELPER_EXE_NAME))
    }

    /// The pipe the broker serves on (the provider connects here; the launcher
    /// passes the SAME name to the broker).
    #[must_use]
    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    /// The install directory (the provider's client checks the server image
    /// lives here).
    #[must_use]
    pub fn helper_dir(&self) -> &Path {
        &self.helper_dir
    }

    /// The app-owned temp dir where the provider streams locked-file copies.
    #[must_use]
    pub fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, LaunchState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// STATUS: the broker has been launched this session (its process started).
    #[must_use]
    pub fn helper_launched(&self) -> bool {
        *self.lock_state() == LaunchState::Launched
    }

    /// STATUS: the broker can be brought up on demand - the bundled sidecar
    /// exists on disk AND a prior launch did not already fail. Distinct from
    /// [`Self::helper_launched`]: a not-yet-attempted launchable helper means
    /// locked-file backup is available (it launches on the first locked file),
    /// so the Settings banner is NOT degraded.
    #[must_use]
    pub fn helper_launchable(&self) -> bool {
        *self.lock_state() != LaunchState::Failed && self.helper_exe.exists()
    }

    /// Shut the broker down (release everything + exit) at app quit. Best-effort
    /// and idempotent: a no-op if the broker was never launched. Off Windows the
    /// client is not compiled, so this is a no-op there too.
    pub fn shutdown(&self) {
        // The whole body is Windows-only (the pipe client is not compiled
        // elsewhere), so the launched-guard lives INSIDE the cfg block - keeping
        // it outside would leave a needless bare `return;` as the function's last
        // statement on non-Windows.
        #[cfg(windows)]
        {
            if *self.lock_state() != LaunchState::Launched {
                return;
            }
            match driven_vss_helper::HelperClient::connect(&self.pipe_name, &self.helper_dir) {
                Ok(mut c) => {
                    let _ = c.shutdown();
                    tracing::info!("VSS helper: shutdown requested on app quit");
                }
                Err(e) => {
                    tracing::debug!(error = %e, "VSS helper: shutdown connect failed (broker may have already exited)");
                }
            }
        }
    }
}

impl HelperLauncher for VssHelperManager {
    fn ensure_launched(&self) -> bool {
        let mut st = self.lock_state();
        match *st {
            LaunchState::Launched => true,
            LaunchState::Failed => false,
            LaunchState::NotAttempted => {
                let args = helper_args(&self.pipe_name, &self.allowed_roots);
                match (self.launch)(&self.helper_exe, &args) {
                    Ok(()) => {
                        tracing::info!(
                            pipe = %self.pipe_name,
                            roots = self.allowed_roots.len(),
                            "VSS helper: launched elevated broker on demand (least-privilege locked-file backup)"
                        );
                        *st = LaunchState::Launched;
                        true
                    }
                    Err(e) => {
                        // Degrade gracefully (DESIGN s5.3.1): a declined UAC prompt
                        // or a missing sidecar leaves locked files skipped exactly
                        // as with no elevation - never a crash / wedged sync. We do
                        // NOT re-attempt this session, so the user is not re-prompted
                        // on every subsequent locked file.
                        tracing::warn!(
                            error = %e,
                            "VSS helper: elevated launch failed; locked-file backup degrades to skip (will not re-prompt this session)"
                        );
                        *st = LaunchState::Failed;
                        false
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A launch counter shared with the injected launch fn so a test can assert
    /// `ensure_launched` launches AT MOST ONCE even across many calls.
    fn counting_launch(verdict: Result<(), String>) -> (LaunchFn, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = calls.clone();
        let f: LaunchFn = Box::new(move |_exe, _args| {
            calls_c.fetch_add(1, Ordering::SeqCst);
            verdict.clone()
        });
        (f, calls)
    }

    #[test]
    fn ensure_launched_launches_at_most_once_on_success() {
        let (launch, calls) = counting_launch(Ok(()));
        let mgr = VssHelperManager::with_launch_fn(
            r"C:\app\driven-vss-helper.exe",
            std::env::temp_dir(),
            vec![PathBuf::from(r"C:\Users\me\Documents")],
            launch,
        );
        assert!(!mgr.helper_launched(), "no launch attempted yet");
        assert!(mgr.ensure_launched(), "first launch succeeds");
        assert!(mgr.ensure_launched(), "memoised: still true");
        assert!(mgr.ensure_launched());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "launched exactly once");
        assert!(mgr.helper_launched());
    }

    #[test]
    fn declined_launch_is_not_retried_and_degrades() {
        let (launch, calls) =
            counting_launch(Err("elevation was declined at the UAC prompt".into()));
        let mgr = VssHelperManager::with_launch_fn(
            r"C:\app\driven-vss-helper.exe",
            std::env::temp_dir(),
            vec![PathBuf::from(r"C:\Users\me\Documents")],
            launch,
        );
        assert!(!mgr.ensure_launched(), "declined launch degrades");
        assert!(!mgr.ensure_launched(), "still degraded");
        assert!(!mgr.ensure_launched());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a declined UAC prompt is NOT re-attempted (no repeat prompts)"
        );
        assert!(!mgr.helper_launched());
    }

    #[test]
    fn pipe_name_is_unguessable_and_dir_is_the_exe_parent() {
        // Build the exe path from a real dir join so `.parent()` resolves on
        // every OS (a literal `C:\...` string has no path separators on Linux and
        // would yield an empty parent).
        let dir = std::env::temp_dir().join("Driven");
        let exe = dir.join("driven-vss-helper.exe");
        let (launch, _) = counting_launch(Ok(()));
        let mgr = VssHelperManager::with_launch_fn(&exe, std::env::temp_dir(), Vec::new(), launch);
        assert!(mgr.pipe_name().starts_with(r"\\.\pipe\driven-vss-"));
        assert_eq!(mgr.helper_dir(), dir);
    }

    #[test]
    fn launchable_is_false_when_the_sidecar_is_missing() {
        let (launch, _) = counting_launch(Ok(()));
        let mgr = VssHelperManager::with_launch_fn(
            // A path that does not exist on any test host.
            std::env::temp_dir().join("definitely-not-here-driven-vss-helper.exe"),
            std::env::temp_dir(),
            Vec::new(),
            launch,
        );
        assert!(
            !mgr.helper_launchable(),
            "a missing sidecar is not launchable"
        );
    }

    #[test]
    fn shutdown_is_a_noop_when_never_launched() {
        let (launch, _) = counting_launch(Ok(()));
        let mgr = VssHelperManager::with_launch_fn(
            r"C:\app\driven-vss-helper.exe",
            std::env::temp_dir(),
            Vec::new(),
            launch,
        );
        // Must not panic / connect when nothing was launched.
        mgr.shutdown();
    }
}
