//! [`VssHelperManager`]: the app-side lifecycle owner for the least-privilege
//! VSS helper (DESIGN s5.3.1, issue #25).
//!
//! VSS snapshot creation needs Administrator rights. Rather than elevate the
//! whole backup app, Driven elevates ONLY the shadow-copy operation: a small
//! privileged broker (`driven-vss-helper.exe`, bundled as a sidecar next to the
//! app) creates the snapshot and streams the locked file's bytes back over a
//! secured named pipe. This manager owns the ONE broker per app session.
//!
//! # Attended vs unattended launch (the UAC-race fix)
//!
//! The elevation prompt needs a HUMAN at the keyboard. So:
//! - **enable-toggle = EAGER.** When the user flips `windows.vss_helper` ON in
//!   Settings, [`Self::launch_now`] fires the elevated launch immediately - the
//!   user is right there and approves the one UAC prompt. The launch runs on a
//!   background thread so the settings IPC returns at once; the UI polls
//!   `get_vss_helper_status` and shows Pending -> Ready/Declined.
//! - **boot = LAZY.** When the setting is ALREADY on at startup we do NOT prompt
//!   at silent boot (rude); the FIRST locked file triggers the launch via
//!   [`HelperLauncher::launch_status`]. Because a launch-in-progress reports
//!   [`LaunchStatus::Pending`] (the file is skipped + retried, never blocked and
//!   never mislabelled "locked") and a timeout is NOT memoised, the boot case
//!   self-heals whenever the user approves the deferred prompt.
//!
//! # Memoisation (only real declines stick)
//!
//! `ShellExecuteExW(runas)` returns `ERROR_CANCELLED` for BOTH an explicit
//! decline and an ignored/timed-out prompt - indistinguishable and both "did not
//! approve", so [`LaunchState::Declined`] is memoised for the session (no
//! re-prompt). A launch that was APPROVED but whose pipe never came up within the
//! attended window is a TRANSIENT failure ([`LaunchState::FailedTransient`]) - not
//! memoised, retried on the next enable-toggle or app start. An off->on re-toggle
//! is fresh present-user intent and clears a prior decline.
//!
//! The launch call is injected ([`VssHelperManager::with_launch_fn`]) so the
//! whole state machine is unit-tested cross-OS without a real `runas`/UAC prompt.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use driven_vss_helper::launch::{generate_pipe_name, helper_args, launch_elevated};
use driven_vss_helper::{HelperLauncher, LaunchError, LaunchStatus};

/// The bundled sidecar file name (tauri installs the externalBin next to the
/// app executable with the target-triple suffix stripped).
const HELPER_EXE_NAME: &str = "driven-vss-helper.exe";

/// The ATTENDED window: how long the launch operation waits, after the user
/// approves elevation, for the broker's pipe to come up before declaring a
/// transient failure. Generous because it spans a human approving the UAC prompt
/// PLUS the broker starting; only ever applies to the one-shot launch, never to
/// steady-state reconnects (those keep the client's tight budget).
const ATTENDED_WINDOW: Duration = Duration::from_secs(90);

/// The injected "bring the broker to Ready" operation (launch elevated, then
/// confirm the pipe is serving). Production uses [`production_launch`]; tests
/// inject the outcome directly so the state machine is exercised without a real
/// UAC prompt. Returns `Ok(())` once the broker is up, [`LaunchError::Declined`]
/// when the user did not approve, [`LaunchError::Failed`] on any other / timeout.
type LaunchFn = Box<dyn Fn() -> Result<(), LaunchError> + Send + Sync>;

/// The launch state machine (DESIGN s5.3.1). See the module docs for the
/// attended/decline/transient semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchState {
    /// No launch attempted yet - the eager toggle or the first locked file starts
    /// one.
    NotAttempted,
    /// A launch is in progress on the background thread (awaiting UAC approval /
    /// pipe coming up). The provider reports this as `Pending` (skip + retry).
    Pending,
    /// The broker is up and serving (its pipe accepted a handshake).
    Ready,
    /// The user declined / ignored the UAC prompt. Memoised for the session; a
    /// fresh off->on toggle clears it.
    Declined,
    /// The launch was approved but the pipe never came up in the attended window,
    /// or the launch failed for a non-decline reason. Transient - retried on the
    /// next enable-toggle or app start (NOT memoised as a decline).
    FailedTransient,
}

/// Shared launch state + everything the background launch thread needs. Held in
/// an `Arc` so the thread (`'static`) can own a clone alongside the manager.
struct Inner {
    /// The unguessable pipe the broker serves on (generated once per session).
    pipe_name: String,
    /// Absolute path to the bundled `driven-vss-helper.exe` sidecar.
    helper_exe: PathBuf,
    /// The install directory (the sidecar's parent) - the provider's client
    /// verifies the broker's server image lives here.
    helper_dir: PathBuf,
    /// App-owned scratch dir where the provider streams locked-file temp copies.
    temp_dir: PathBuf,
    /// The injected "bring the broker to Ready" operation.
    launch: LaunchFn,
    /// The launch state machine.
    state: Mutex<LaunchState>,
    /// Whether the `windows.vss_helper` setting is on (gates all launching +
    /// capability). Updated live by [`VssHelperManager::set_enabled`].
    enabled: AtomicBool,
}

impl Inner {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, LaunchState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The app-side owner of the least-privilege VSS helper broker (DESIGN s5.3.1).
pub struct VssHelperManager {
    inner: Arc<Inner>,
    /// The most recent launch thread, kept so shutdown can best-effort reap it
    /// (a thread mid-UAC cannot be cancelled; the process exit reaps it).
    launch_thread: Mutex<Option<JoinHandle<()>>>,
}

impl VssHelperManager {
    /// Build a manager for the bundled sidecar at `helper_exe`, streaming temp
    /// copies under `temp_dir`, allowing the broker to snapshot files under
    /// `allowed_roots`, with the `windows.vss_helper` setting initially `enabled`.
    /// Uses the real elevated `runas` launch + pipe probe.
    #[must_use]
    pub fn new(
        helper_exe: impl Into<PathBuf>,
        temp_dir: impl Into<PathBuf>,
        allowed_roots: Vec<PathBuf>,
        enabled: bool,
    ) -> Self {
        let helper_exe = helper_exe.into();
        let helper_dir = helper_exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let pipe_name = generate_pipe_name();
        // The production launch: elevate then wait (attended window) for the pipe.
        let launch: LaunchFn = {
            let pipe_name = pipe_name.clone();
            let helper_exe = helper_exe.clone();
            let helper_dir = helper_dir.clone();
            Box::new(move || {
                production_launch(&pipe_name, &helper_exe, &helper_dir, &allowed_roots)
            })
        };
        Self::from_parts(
            pipe_name,
            helper_exe,
            helper_dir,
            temp_dir.into(),
            launch,
            enabled,
        )
    }

    /// Test seam: build a manager with an INJECTED launch operation so the state
    /// machine is unit-tested without a real UAC prompt. `allowed_roots` is not
    /// needed here (the injected launch decides the outcome).
    #[must_use]
    pub fn with_launch_fn(
        helper_exe: impl Into<PathBuf>,
        temp_dir: impl Into<PathBuf>,
        enabled: bool,
        launch: LaunchFn,
    ) -> Self {
        let helper_exe = helper_exe.into();
        let helper_dir = helper_exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::from_parts(
            generate_pipe_name(),
            helper_exe,
            helper_dir,
            temp_dir.into(),
            launch,
            enabled,
        )
    }

    fn from_parts(
        pipe_name: String,
        helper_exe: PathBuf,
        helper_dir: PathBuf,
        temp_dir: PathBuf,
        launch: LaunchFn,
        enabled: bool,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                pipe_name,
                helper_exe,
                helper_dir,
                temp_dir,
                launch,
                state: Mutex::new(LaunchState::NotAttempted),
                enabled: AtomicBool::new(enabled),
            }),
            launch_thread: Mutex::new(None),
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

    /// The pipe the broker serves on (the provider connects here).
    #[must_use]
    pub fn pipe_name(&self) -> &str {
        &self.inner.pipe_name
    }

    /// The install directory (the provider's client checks the server image here).
    #[must_use]
    pub fn helper_dir(&self) -> &Path {
        &self.inner.helper_dir
    }

    /// The app-owned temp dir where the provider streams locked-file copies.
    #[must_use]
    pub fn temp_dir(&self) -> &Path {
        &self.inner.temp_dir
    }

    fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::SeqCst)
    }

    /// Apply a change to the `windows.vss_helper` setting (called from the
    /// settings IPC on a real toggle). On enable (off->on) this clears any prior
    /// decline/transient state so the eager [`Self::launch_now`] the caller then
    /// issues re-attempts with a fresh UAC prompt; on disable it shuts the broker
    /// down and resets the state so a later re-enable relaunches cleanly.
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::SeqCst);
        if enabled {
            // Fresh present-user intent: clear a memoised decline / transient fail.
            let mut st = self.inner.lock_state();
            if matches!(*st, LaunchState::Declined | LaunchState::FailedTransient) {
                *st = LaunchState::NotAttempted;
            }
        } else {
            // Disabled: stop the broker (best-effort) and reset so re-enable
            // relaunches cleanly.
            self.shutdown();
            *self.inner.lock_state() = LaunchState::NotAttempted;
        }
    }

    /// EAGER launch (the enable-toggle path): if enabled and no launch is in
    /// flight / done, start the elevated launch NOW on a background thread so the
    /// attended UAC prompt appears while the user is at the Settings screen.
    /// Non-blocking + idempotent (a launch already Pending/Ready/Declined is left
    /// as-is).
    pub fn launch_now(&self) {
        self.trigger_launch();
    }

    /// Start the background launch if the state permits it (NotAttempted +
    /// enabled). Sets `Pending` under the state lock so concurrent triggers spawn
    /// exactly one thread. Shared by the eager (`launch_now`) and lazy
    /// (`launch_status`) paths.
    fn trigger_launch(&self) {
        if !self.is_enabled() {
            return;
        }
        {
            let mut st = self.inner.lock_state();
            if *st != LaunchState::NotAttempted {
                return; // already pending / ready / declined / transiently-failed
            }
            *st = LaunchState::Pending;
        }
        let inner = self.inner.clone();
        let handle = std::thread::Builder::new()
            .name("driven-vss-launch".to_string())
            .spawn(move || {
                let outcome = (inner.launch)();
                let next = match outcome {
                    Ok(()) => {
                        tracing::info!("VSS helper: elevated broker launched + serving");
                        LaunchState::Ready
                    }
                    Err(LaunchError::Declined) => {
                        tracing::warn!(
                            "VSS helper: elevation declined/ignored; locked-file backup stays degraded this session (no re-prompt)"
                        );
                        LaunchState::Declined
                    }
                    Err(LaunchError::Failed(detail)) => {
                        tracing::warn!(
                            error = %detail,
                            "VSS helper: launch did not come up (transient); will retry on the next enable/start"
                        );
                        LaunchState::FailedTransient
                    }
                };
                *inner.lock_state() = next;
            });
        match handle {
            Ok(h) => {
                // Best-effort reap: replace any prior (finished) handle.
                *self.launch_thread.lock().unwrap_or_else(|p| p.into_inner()) = Some(h);
            }
            Err(e) => {
                tracing::warn!(error = %e, "VSS helper: could not spawn launch thread; degrading");
                *self.inner.lock_state() = LaunchState::FailedTransient;
            }
        }
    }

    // --- status accessors (for get_vss_helper_status) -----------------------

    /// STATUS: the broker is up and serving this session.
    #[must_use]
    pub fn helper_alive(&self) -> bool {
        *self.inner.lock_state() == LaunchState::Ready
    }

    /// STATUS: a launch is in progress (awaiting elevation approval / pipe coming
    /// up) - the UI shows a "waiting for approval" hint.
    #[must_use]
    pub fn launch_pending(&self) -> bool {
        *self.inner.lock_state() == LaunchState::Pending
    }

    /// STATUS: the user declined elevation this session (memoised).
    #[must_use]
    pub fn launch_declined(&self) -> bool {
        *self.inner.lock_state() == LaunchState::Declined
    }

    /// STATUS: locked-file backup can (still) happen - the setting is on, the
    /// sidecar exists, and the broker is up / coming up / not-yet-tried (NOT
    /// declined or transiently failed). Drives the "not degraded" banner state.
    #[must_use]
    pub fn helper_launchable(&self) -> bool {
        if !self.is_enabled() || !self.inner.helper_exe.exists() {
            return false;
        }
        matches!(
            *self.inner.lock_state(),
            LaunchState::NotAttempted | LaunchState::Pending | LaunchState::Ready
        )
    }

    /// Shut the broker down (release everything + exit) at app quit / on disable.
    /// Best-effort + idempotent: a no-op unless the broker is up. Off Windows the
    /// client is not compiled, so this is a no-op there too.
    pub fn shutdown(&self) {
        #[cfg(windows)]
        {
            if *self.inner.lock_state() != LaunchState::Ready {
                return;
            }
            match driven_vss_helper::HelperClient::connect(
                &self.inner.pipe_name,
                &self.inner.helper_dir,
            ) {
                Ok(mut c) => {
                    let _ = c.shutdown();
                    tracing::info!("VSS helper: shutdown requested");
                }
                Err(e) => {
                    tracing::debug!(error = %e, "VSS helper: shutdown connect failed (broker may have already exited)");
                }
            }
        }
    }

    /// Test-only: block until the in-flight launch thread finishes, so a test can
    /// assert the terminal state deterministically.
    #[cfg(test)]
    fn join_launch_thread(&self) {
        let handle = self
            .launch_thread
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

impl HelperLauncher for VssHelperManager {
    fn launch_status(&self) -> LaunchStatus {
        if !self.is_enabled() {
            return LaunchStatus::Disabled;
        }
        // Read the state; a NotAttempted state lazily triggers a launch (the
        // boot-already-on first-locked-file path) and then reports Pending.
        let state = *self.inner.lock_state();
        match state {
            LaunchState::Ready => LaunchStatus::Ready,
            LaunchState::Pending => LaunchStatus::Pending,
            LaunchState::Declined => LaunchStatus::Declined,
            // A transient failure is not auto-retried on the next locked file
            // (only on enable-toggle / restart); report Disabled so the executor
            // skips rather than spinning.
            LaunchState::FailedTransient => LaunchStatus::Disabled,
            LaunchState::NotAttempted => {
                self.trigger_launch();
                LaunchStatus::Pending
            }
        }
    }

    fn is_available(&self) -> bool {
        if !self.is_enabled() {
            return false;
        }
        // Capable when the broker is up, coming up, or not-yet-tried (launchable);
        // NOT capable once declined / transiently failed (then the executor skips).
        matches!(
            *self.inner.lock_state(),
            LaunchState::NotAttempted | LaunchState::Pending | LaunchState::Ready
        )
    }
}

/// The production "bring the broker to Ready" operation: launch elevated (which,
/// with `SEE_MASK_NOASYNC`, blocks until the user approves/declines), then - once
/// approved - probe the pipe until the broker is serving, up to [`ATTENDED_WINDOW`].
fn production_launch(
    pipe_name: &str,
    helper_exe: &Path,
    helper_dir: &Path,
    allowed_roots: &[PathBuf],
) -> Result<(), LaunchError> {
    let args = helper_args(pipe_name, allowed_roots);
    launch_elevated(helper_exe, &args)?;
    // Approved: the elevated process is starting. Confirm its pipe comes up.
    #[cfg(windows)]
    {
        let deadline = std::time::Instant::now() + ATTENDED_WINDOW;
        loop {
            match driven_vss_helper::HelperClient::connect(pipe_name, helper_dir) {
                Ok(_client) => return Ok(()),
                Err(e) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(LaunchError::Failed(format!(
                            "helper pipe did not come up within the attended window: {e}"
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }
    // Off Windows `launch_elevated` already returned Err above, so this is
    // unreachable in practice; keep it total for the type.
    #[cfg(not(windows))]
    {
        let _ = (helper_dir, ATTENDED_WINDOW);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Build an injected launch fn that returns `verdict` and counts calls.
    fn counting_launch(
        verdict: Result<(), LaunchError>,
    ) -> (LaunchFn, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_c = calls.clone();
        let f: LaunchFn = Box::new(move || {
            calls_c.fetch_add(1, Ordering::SeqCst);
            verdict.clone()
        });
        (f, calls)
    }

    fn manager(enabled: bool, launch: LaunchFn) -> VssHelperManager {
        VssHelperManager::with_launch_fn(
            std::env::temp_dir().join("Driven").join(HELPER_EXE_NAME),
            std::env::temp_dir(),
            enabled,
            launch,
        )
    }

    #[test]
    fn eager_launch_on_enable_reaches_ready() {
        let (launch, calls) = counting_launch(Ok(()));
        let mgr = manager(true, launch);
        assert!(!mgr.helper_alive());
        mgr.launch_now();
        mgr.join_launch_thread();
        assert!(mgr.helper_alive(), "an approved launch reaches Ready");
        assert!(!mgr.launch_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Idempotent: a second launch_now does not re-launch a Ready broker.
        mgr.launch_now();
        mgr.join_launch_thread();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn declined_launch_is_memoised_and_not_retried() {
        let (launch, calls) = counting_launch(Err(LaunchError::Declined));
        let mgr = manager(true, launch);
        mgr.launch_now();
        mgr.join_launch_thread();
        assert!(mgr.launch_declined());
        assert!(!mgr.helper_launchable(), "declined -> not launchable");
        // A second lazy/eager attempt does NOT re-prompt.
        assert_eq!(mgr.launch_status(), LaunchStatus::Declined);
        mgr.launch_now();
        mgr.join_launch_thread();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a decline is not re-attempted"
        );
    }

    #[test]
    fn transient_failure_is_not_memoised_and_retries_on_reenable() {
        let (launch, calls) = counting_launch(Err(LaunchError::Failed("pipe timeout".into())));
        let mgr = manager(true, launch);
        mgr.launch_now();
        mgr.join_launch_thread();
        assert!(!mgr.helper_alive());
        assert!(!mgr.launch_declined(), "a timeout is NOT a decline");
        // Re-enable (off->on) clears the transient state and re-attempts.
        mgr.set_enabled(false);
        mgr.set_enabled(true);
        mgr.launch_now();
        mgr.join_launch_thread();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a transient failure retries on the next enable"
        );
    }

    #[test]
    fn reenable_after_decline_reprompts() {
        let (launch, calls) = counting_launch(Err(LaunchError::Declined));
        let mgr = manager(true, launch);
        mgr.launch_now();
        mgr.join_launch_thread();
        assert!(mgr.launch_declined());
        // A fresh off->on toggle is present-user intent: it clears the decline and
        // re-attempts (the memoisation governs only the automatic/lazy path).
        mgr.set_enabled(false);
        mgr.set_enabled(true);
        mgr.launch_now();
        mgr.join_launch_thread();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn disabled_manager_never_launches_and_is_unavailable() {
        let (launch, calls) = counting_launch(Ok(()));
        let mgr = manager(false, launch);
        assert_eq!(mgr.launch_status(), LaunchStatus::Disabled);
        assert!(!mgr.is_available());
        mgr.launch_now();
        mgr.join_launch_thread();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a disabled manager never launches"
        );
    }

    #[test]
    fn lazy_launch_status_triggers_a_launch_and_reports_pending() {
        // A launch that blocks until the test releases it, so `Pending` is
        // observable before it resolves.
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Arc::new(Mutex::new(rx));
        let launch: LaunchFn = Box::new(move || {
            // Block until released, then succeed.
            let _ = rx.lock().unwrap().recv();
            Ok(())
        });
        let mgr = manager(true, launch);
        // First lazy consult (boot-already-on, first locked file) triggers the
        // launch and reports Pending.
        assert_eq!(mgr.launch_status(), LaunchStatus::Pending);
        assert!(mgr.launch_pending());
        assert!(
            mgr.is_available(),
            "pending is still capable (executor consults it)"
        );
        // Release the launch -> Ready.
        tx.send(()).unwrap();
        mgr.join_launch_thread();
        assert!(mgr.helper_alive());
        assert_eq!(mgr.launch_status(), LaunchStatus::Ready);
    }

    #[test]
    fn shutdown_is_a_noop_when_not_ready() {
        let (launch, _) = counting_launch(Ok(()));
        let mgr = manager(true, launch);
        mgr.shutdown(); // NotAttempted -> no-op, must not panic/connect
    }

    /// Exercise the PRODUCTION constructor + launch path (`new` -> `production_launch`
    /// -> `launch_elevated`) without a real UAC prompt. Gated to non-Windows: there
    /// `launch_elevated` reports "Windows only" immediately (no `runas`, no prompt),
    /// so the production path resolves to a transient failure - NOT alive, NOT a
    /// decline - deterministically. (The real elevated `runas` + pipe probe is the
    /// post-merge Windows smoke.) The coverage job runs on ubuntu, so this covers
    /// the otherwise-untested production constructor lines.
    #[cfg(not(windows))]
    #[test]
    fn production_launch_off_windows_is_transient_not_a_decline() {
        let mgr = VssHelperManager::new(
            std::env::temp_dir().join("Driven").join(HELPER_EXE_NAME),
            std::env::temp_dir(),
            vec![std::env::temp_dir()],
            true,
        );
        mgr.launch_now();
        mgr.join_launch_thread();
        assert!(
            !mgr.helper_alive(),
            "the helper is not supported off Windows"
        );
        assert!(
            !mgr.launch_declined(),
            "an unsupported-platform failure is transient, not a decline"
        );
    }
}
