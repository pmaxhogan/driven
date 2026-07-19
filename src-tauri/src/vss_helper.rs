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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    /// Monotonic launch generation. Bumped (under the `state` lock) on every
    /// launch trigger AND on every shutdown/disable. A background launch captures
    /// its generation; when it resolves it applies its result ONLY if the
    /// generation is still current - otherwise it was ABANDONED (a quit/disable
    /// happened while it was Pending) and it REAPS the elevated helper it brought
    /// up instead of leaving an orphaned elevated process holding the pipe.
    generation: AtomicU64,
    /// Count of abandoned launches whose helper the resolver reaped (test
    /// observability; cheap in production).
    reap_count: AtomicUsize,
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
                generation: AtomicU64::new(0),
                reap_count: AtomicUsize::new(0),
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
            // Disabled: stop the broker (best-effort), abandon any in-flight
            // launch (so it reaps the helper it brings up), and reset the state so
            // a later re-enable relaunches cleanly. `shutdown` does all three.
            self.shutdown();
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
        // Capture THIS launch's generation under the state lock, so a concurrent
        // shutdown/disable that bumps the generation is ordered against it.
        let my_gen = {
            let mut st = self.inner.lock_state();
            if *st != LaunchState::NotAttempted {
                return; // already pending / ready / declined / transiently-failed
            }
            *st = LaunchState::Pending;
            self.inner
                .generation
                .fetch_add(1, Ordering::SeqCst)
                .wrapping_add(1)
        };
        let inner = self.inner.clone();
        let handle = std::thread::Builder::new()
            .name("driven-vss-launch".to_string())
            .spawn(move || {
                let outcome = (inner.launch)();
                // Apply the result ONLY if this launch is still the current
                // generation; the check + state write are atomic under the lock so
                // a shutdown that bumped the generation cannot interleave.
                let abandoned_ok = {
                    let mut st = inner.lock_state();
                    if inner.generation.load(Ordering::SeqCst) != my_gen {
                        // Superseded / abandoned (a quit/disable happened while we
                        // were Pending). Do NOT touch state; if we brought a helper
                        // up, reap it below (outside the lock).
                        outcome.is_ok()
                    } else {
                        *st = match &outcome {
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
                        false
                    }
                };
                if abandoned_ok {
                    // The app quit / disabled the helper while this launch was in
                    // flight, then it came up: SHUT IT DOWN so no orphaned elevated
                    // process lingers on the pipe (the always-on elevated attack
                    // surface the DESIGN model must not leave behind).
                    inner.reap_count.fetch_add(1, Ordering::SeqCst);
                    reap_helper(&inner);
                }
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
    /// Best-effort + idempotent.
    ///
    /// Two jobs, both closing the orphaned-elevated-process hole: (1) bump the
    /// launch generation so ANY in-flight launch that resolves AFTER this reaps
    /// the helper it brings up instead of leaving it (the quit/disable-during-
    /// Pending race); (2) if a helper is currently up (`Ready`), shut it down now.
    /// Resets the state to `NotAttempted` so a later re-enable relaunches cleanly.
    pub fn shutdown(&self) {
        // Bump the generation + read/reset the state atomically, so an in-flight
        // resolver either already applied `Ready` (then we reap it below) or sees
        // the new generation and reaps itself.
        let was_ready = {
            let mut st = self.inner.lock_state();
            self.inner.generation.fetch_add(1, Ordering::SeqCst);
            let ready = *st == LaunchState::Ready;
            *st = LaunchState::NotAttempted;
            ready
        };
        if was_ready {
            reap_helper(&self.inner);
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

    /// Test-only: how many abandoned launches the resolver reaped.
    #[cfg(test)]
    fn reap_count(&self) -> usize {
        self.inner.reap_count.load(Ordering::SeqCst)
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

/// Shut down a broker that an ABANDONED launch brought up (best-effort). Called
/// off the state lock. Windows-only: the client is not compiled elsewhere, so
/// this is a no-op there (and the manager is never in play off Windows anyway).
fn reap_helper(inner: &Inner) {
    #[cfg(windows)]
    {
        match driven_vss_helper::HelperClient::connect(&inner.pipe_name, &inner.helper_dir) {
            Ok(mut c) => {
                let _ = c.shutdown();
                tracing::info!(
                    "VSS helper: reaped an abandoned broker (quit/disable landed while it was launching)"
                );
            }
            Err(e) => {
                tracing::debug!(error = %e, "VSS helper: reap connect failed (broker may not have come up)");
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = inner;
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

    #[test]
    fn shutdown_during_pending_reaps_the_helper_that_resolves_after() {
        // The quit/disable-during-Pending race: a launch is in flight (UAC up);
        // shutdown lands; then the launch RESOLVES (the user approved late). The
        // resolver must REAP the just-launched elevated helper instead of leaving
        // it as an orphan holding the pipe - and NOT leave the state Ready.
        let (tx, rx) = mpsc::channel::<()>();
        let rx = Arc::new(Mutex::new(rx));
        let launch: LaunchFn = Box::new(move || {
            let _ = rx.lock().unwrap().recv(); // block until released
            Ok(()) // then "come up"
        });
        let mgr = manager(true, launch);
        mgr.launch_now();
        assert!(mgr.launch_pending(), "launch is in flight");

        // Quit/disable lands while Pending.
        mgr.shutdown();

        // The launch resolves AFTER the shutdown.
        tx.send(()).unwrap();
        mgr.join_launch_thread();

        assert!(
            !mgr.helper_alive(),
            "an abandoned launch must NOT be left Ready"
        );
        assert_eq!(
            mgr.reap_count(),
            1,
            "the resolver must reap the helper it brought up after the shutdown"
        );
    }

    #[test]
    fn shutdown_after_ready_reaps_without_double_counting() {
        // The other ordering: the launch reaches Ready BEFORE shutdown. Shutdown
        // reaps the running helper directly; the resolver already applied Ready and
        // does not also reap (no double reap).
        let (launch, _) = counting_launch(Ok(()));
        let mgr = manager(true, launch);
        mgr.launch_now();
        mgr.join_launch_thread();
        assert!(mgr.helper_alive());
        mgr.shutdown();
        assert!(!mgr.helper_alive(), "shutdown resets the state");
        assert_eq!(
            mgr.reap_count(),
            0,
            "a launch that reached Ready before shutdown is reaped BY shutdown, not the resolver"
        );
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
