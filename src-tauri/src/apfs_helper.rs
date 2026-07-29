//! App-side owner of the macOS APFS snapshot broker (DESIGN s5.3.2).
//!
//! The macOS sibling of [`crate::vss_helper::VssHelperManager`] (DESIGN
//! s5.3.1). Same contract, deliberately thinner: the launch state machine
//! (at-most-once osascript consent, background thread, memoised decline)
//! already lives in [`driven_apfs::OsascriptLauncher`], so this manager only
//! adds the two things the app layer owns:
//!
//! - the `macos.apfs_snapshot` SETTING gate (a disabled manager reports
//!   [`HelperLaunchStatus::Disabled`], so every account's provider behaves
//!   exactly like the historical skip until the user opts in), and
//! - session identity: ONE socket path, ONE broker, ONE admin prompt shared by
//!   every account's [`driven_apfs::ApfsBrokeredProvider`].
//!
//! The manager is built at boot REGARDLESS of the setting (mirroring Windows)
//! so flipping the toggle on takes effect without an app restart.
//!
//! Scope note: an APFS snapshot reads around a BUSY/locked file. It does NOT
//! read around a TCC (privacy) denial - a snapshot mount preserves the
//! original's ownership and is itself TCC-gated, and the `-o noowners` bypass
//! was CVE-2020-9771, patched years ago and now an EDR-flagged signature. A
//! `local.permission_denied` file needs Full Disk Access and nothing here
//! helps it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use driven_apfs::{HelperLaunchStatus, HelperLauncher, OsascriptLauncher};

/// The bundled sidecar file name. Tauri installs an `externalBin` next to the
/// app executable with the target-triple suffix stripped; on macOS that is
/// `Driven.app/Contents/MacOS/`, the same directory as the app binary, so the
/// Windows `current_exe().parent()` resolution works here unchanged.
const HELPER_EXE_NAME: &str = "driven-apfs-helper";

/// Parent of the per-user directory (created `0700`) holding this session's
/// broker socket.
///
/// Deliberately `/tmp` and NOT [`std::env::temp_dir`]. A unix-domain socket path
/// must fit in `sockaddr_un.sun_path`, which is 104 bytes on Darwin, and macOS
/// points `TMPDIR` at a per-user `/var/folders/<xx>/<20-odd chars>/T/` path. That
/// leaves a real socket path of ~110 bytes, so `UnixListener::bind` fails with
/// "path must be shorter than SUN_LEN" and the broker exits before it ever
/// serves - on EVERY Mac, not just unusual ones. Measured on macOS 26.6:
/// `$TMPDIR/driven-apfs/driven-apfs-<32 hex>.sock` = 110 bytes; the `/tmp` form
/// below = 70. See `socket_path_fits_in_sockaddr_un` for the regression guard.
///
/// `/tmp` is world-writable but sticky, and the per-uid subdirectory is created
/// `0700`, so another user can neither read the socket nor replace the
/// directory. If they pre-create it, it is owned by THEM, and the broker's own
/// "socket parent must be owned by the peer uid with mode 0700" check refuses to
/// serve - the failure is closed, not silent.
const RUNTIME_DIR_PARENT: &str = "/tmp";

/// The app-side owner of the macOS APFS snapshot broker (DESIGN s5.3.2).
pub struct ApfsHelperManager {
    /// The bundled `driven-apfs-helper` sidecar.
    helper_exe: PathBuf,
    /// This session's broker socket (fixed for the session, per the
    /// [`HelperLauncher::socket_path`] contract).
    socket: PathBuf,
    /// The broker's launch argv (socket + allow-list + peer uid + app pid).
    args: Vec<String>,
    /// Whether the `macos.apfs_snapshot` setting is on. Gates ALL launching and
    /// capability. Updated live by [`ApfsHelperManager::set_enabled`].
    enabled: AtomicBool,
    /// The current launch state machine. REPLACED with a fresh launcher on a
    /// re-enable, which is exactly how [`OsascriptLauncher`] documents clearing
    /// a memoised decline or transient failure ("an enable-toggle constructs a
    /// fresh launcher and retries").
    launcher: Mutex<Arc<OsascriptLauncher>>,
    /// Whether anything has DELIBERATELY triggered a launch on the current
    /// launcher.
    ///
    /// Load-bearing: [`OsascriptLauncher::launch_status`] is NOT a pure read -
    /// its `NotAttempted -> InFlight` transition is exactly what spawns the
    /// consent prompt. So the status accessors below must not call it before a
    /// real trigger, or merely opening the Settings tab (which polls
    /// `get_apfs_helper_status`) would pop an administrator prompt the user
    /// never asked for. Set by the two genuine trigger paths - the provider's
    /// [`HelperLauncher::launch_status`] on a locked file, and the enable-toggle
    /// [`Self::launch_now`] - and cleared whenever the launcher is replaced.
    launch_attempted: AtomicBool,
    /// Set once the broker's socket has actually answered, after which the
    /// liveness probe in [`Self::confirm_ready`] is skipped for the session.
    broker_confirmed: AtomicBool,
    /// When the launcher FIRST claimed `Ready` without a live socket behind it,
    /// so the grace window in [`Self::confirm_ready`] can expire.
    ready_since: Mutex<Option<std::time::Instant>>,
}

/// How long a launcher-reported `Ready` may go unbacked by a live socket before
/// the broker is declared dead. Generous: it only has to cover the gap between
/// `osascript` returning and the broker binding its socket, and erring long
/// costs a few extra transient skips while erring short would call a healthy
/// broker dead.
const BROKER_READY_GRACE: std::time::Duration = std::time::Duration::from_secs(15);

/// Whether a unix-domain socket at `path` has a LISTENER behind it right now.
///
/// Existence alone is not enough: a broker that died leaves its socket file on
/// disk, and connecting to that stale file fails with `ECONNREFUSED`. The
/// connection is dropped immediately without speaking the protocol - this is a
/// liveness probe, not a handshake, and [`crate::apfs_helper`] never treats it
/// as authentication (the client's own root-peer check does that).
#[cfg(unix)]
fn socket_is_live(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

/// Off unix there are no unix-domain sockets, and the launcher can never report
/// `Ready` there anyway (the osascript stub always fails).
#[cfg(not(unix))]
fn socket_is_live(_path: &Path) -> bool {
    false
}

impl ApfsHelperManager {
    /// Build a manager for the bundled sidecar at `helper_exe`, letting the
    /// broker mount snapshots of `allowed_volumes`, with the
    /// `macos.apfs_snapshot` setting initially `enabled`.
    ///
    /// `runtime_dir` is the (app-owned, `0700`) directory the session socket is
    /// created under; the random socket name makes the path unguessable so a
    /// local attacker cannot pre-create it.
    #[must_use]
    pub fn new(
        helper_exe: impl Into<PathBuf>,
        runtime_dir: &Path,
        allowed_volumes: Vec<PathBuf>,
        enabled: bool,
    ) -> Self {
        let helper_exe = helper_exe.into();
        let socket = driven_apfs::launch::generate_socket_path(runtime_dir);
        let args = driven_apfs::launch::helper_args(
            &socket,
            &allowed_volumes,
            current_uid(),
            std::process::id(),
        );
        let launcher = Arc::new(OsascriptLauncher::new(
            helper_exe.clone(),
            socket.clone(),
            args.clone(),
        ));
        Self {
            helper_exe,
            socket,
            args,
            enabled: AtomicBool::new(enabled),
            launcher: Mutex::new(launcher),
            launch_attempted: AtomicBool::new(false),
            broker_confirmed: AtomicBool::new(false),
            ready_since: Mutex::new(None),
        }
    }

    /// Resolve the bundled sidecar path for the CURRENT app executable: the
    /// `driven-apfs-helper` installed next to the app binary inside
    /// `Contents/MacOS/`. `None` if the current-exe path cannot be resolved.
    #[must_use]
    pub fn bundled_helper_exe() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        Some(dir.join(HELPER_EXE_NAME))
    }

    /// The per-user runtime directory this session's socket lives in, created
    /// `0700` so only the app's own uid can reach the socket. `None` if it
    /// cannot be created.
    #[must_use]
    pub fn runtime_dir() -> Option<PathBuf> {
        // Per-uid so two users on one Mac never share a directory.
        let dir = Path::new(RUNTIME_DIR_PARENT).join(format!("driven-apfs-{}", current_uid()));
        std::fs::create_dir_all(&dir).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Best-effort tighten: an existing dir may predate this session.
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        Some(dir)
    }

    fn current_launcher(&self) -> Arc<OsascriptLauncher> {
        Arc::clone(&self.launcher.lock().unwrap_or_else(|p| p.into_inner()))
    }

    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Replace the launch state machine with a fresh one, clearing any memoised
    /// decline / transient failure so the next genuine trigger re-prompts.
    fn reset_launcher(&self) {
        let fresh = Arc::new(OsascriptLauncher::new(
            self.helper_exe.clone(),
            self.socket.clone(),
            self.args.clone(),
        ));
        *self.launcher.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
        // A fresh launcher has not been triggered, so status reads must go back
        // to answering "not yet tried" WITHOUT touching it.
        self.launch_attempted.store(false, Ordering::SeqCst);
        // The next launch gets a fresh broker, so re-prove its liveness rather
        // than trusting a previous session's confirmation.
        self.broker_confirmed.store(false, Ordering::SeqCst);
        *self.ready_since.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// The launcher's status, but ONLY once a launch has genuinely been
    /// triggered. `None` means "not yet tried" and is what every status
    /// accessor must report before a trigger, because merely asking the
    /// launcher would start the consent prompt (see
    /// [`Self::launch_attempted`]). Also `None` when the setting is off.
    fn observed_status(&self) -> Option<HelperLaunchStatus> {
        if !self.is_enabled() || !self.launch_attempted.load(Ordering::SeqCst) {
            return None;
        }
        Some(self.confirm_ready(self.current_launcher().launch_status()))
    }

    /// Downgrade a launcher-reported `Ready` that no live broker backs.
    ///
    /// `osascript` exits 0 as soon as the consent prompt resolves and the shell
    /// BACKGROUNDS the broker (`... &`), so the launcher records `Ready` from
    /// the spawn alone. If the broker then dies immediately - a refused
    /// pre-flight check, a bad sidecar, a socket path that will not bind - the
    /// user sees an administrator prompt followed by a healthy status while
    /// nothing ever mounts. That silent shape is exactly what the `sun_path`
    /// overflow produced, so `Ready` is only reported once the socket actually
    /// answers.
    ///
    /// Timing matters: the broker needs a moment to bind after the spawn, so a
    /// not-yet-live socket is `Pending` (a transient skip, retried next cycle)
    /// until [`BROKER_READY_GRACE`] elapses, and only then `Disabled` - which
    /// makes `helper_launchable` false and surfaces as "degraded" in the UI
    /// rather than as an eternal `Pending` nobody can diagnose.
    ///
    /// Probing stops permanently at the first success, so a healthy session
    /// costs at most a few connects rather than one per status poll.
    fn confirm_ready(&self, status: HelperLaunchStatus) -> HelperLaunchStatus {
        if status != HelperLaunchStatus::Ready || self.broker_confirmed.load(Ordering::SeqCst) {
            return status;
        }
        if socket_is_live(&self.socket) {
            self.broker_confirmed.store(true, Ordering::SeqCst);
            return HelperLaunchStatus::Ready;
        }
        let mut first = self.ready_since.lock().unwrap_or_else(|p| p.into_inner());
        let started = *first.get_or_insert_with(std::time::Instant::now);
        if started.elapsed() < BROKER_READY_GRACE {
            HelperLaunchStatus::Pending
        } else {
            tracing::warn!(
                socket = %self.socket.display(),
                "apfs broker reported launched but its socket never came up; \
                 locked-file backup is unavailable this session"
            );
            HelperLaunchStatus::Disabled
        }
    }

    /// Apply a change to the `macos.apfs_snapshot` setting (called from the
    /// settings IPC on a real toggle). Enabling clears a prior decline so the
    /// eager [`Self::launch_now`] the caller then issues re-prompts; disabling
    /// shuts the broker down and resets the state so a later re-enable
    /// relaunches cleanly.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
        if enabled {
            self.reset_launcher();
        } else {
            self.shutdown();
        }
    }

    /// EAGER launch (the enable-toggle path): ask for status, which triggers the
    /// at-most-once background consent launch, so the admin prompt appears while
    /// the user is still at the Settings screen. Non-blocking and idempotent.
    pub fn launch_now(&self) {
        if !self.is_enabled() {
            return;
        }
        self.launch_attempted.store(true, Ordering::SeqCst);
        let _ = self.current_launcher().launch_status();
    }

    // --- status accessors (for get_apfs_helper_status) ----------------------

    /// STATUS: the broker is up and serving this session.
    #[must_use]
    pub fn helper_alive(&self) -> bool {
        self.observed_status() == Some(HelperLaunchStatus::Ready)
    }

    /// STATUS: a consent prompt is in flight - the UI shows a "waiting for
    /// approval" hint.
    #[must_use]
    pub fn launch_pending(&self) -> bool {
        self.observed_status() == Some(HelperLaunchStatus::Pending)
    }

    /// STATUS: the user declined the admin prompt this session (memoised).
    #[must_use]
    pub fn launch_declined(&self) -> bool {
        self.observed_status() == Some(HelperLaunchStatus::Declined)
    }

    /// STATUS: locked-file backup can (still) happen - the setting is on, the
    /// sidecar exists, and the broker is up / coming up / not yet tried. Only
    /// a declined or failed launch makes it false, which is what drives the
    /// "degraded" state. Matches the Windows twin, which likewise counts
    /// not-yet-attempted as launchable.
    #[must_use]
    pub fn helper_launchable(&self) -> bool {
        if !self.is_enabled() || !self.helper_exe.exists() {
            return false;
        }
        match self.observed_status() {
            // Not yet tried: the broker comes up on the first locked file.
            None => true,
            Some(HelperLaunchStatus::Ready | HelperLaunchStatus::Pending) => true,
            Some(HelperLaunchStatus::Declined | HelperLaunchStatus::Disabled) => false,
        }
    }

    /// Shut the broker down (unmount everything, delete the snapshot, exit) at
    /// app quit or on disable. Best-effort and idempotent: the broker also
    /// self-terminates when the app pid it was launched with exits, so a failure
    /// here leaks nothing beyond the current session.
    pub fn shutdown(&self) {
        #[cfg(target_os = "macos")]
        {
            if let Ok(mut client) = driven_apfs::client::HelperClient::connect(&self.socket) {
                if let Err(e) = client.shutdown() {
                    tracing::debug!(error = %e, "apfs broker shutdown request failed (it exits with the app anyway)");
                }
            }
        }
        // Reset the state machine so a later re-enable relaunches cleanly.
        self.reset_launcher();
    }
}

impl HelperLauncher for ApfsHelperManager {
    fn launch_status(&self) -> HelperLaunchStatus {
        if !self.is_enabled() {
            return HelperLaunchStatus::Disabled;
        }
        // This IS the lazy-launch trigger (the provider calls it on the first
        // locked file), so record that the launcher has been engaged - after
        // this the status accessors may read it without causing a prompt.
        self.launch_attempted.store(true, Ordering::SeqCst);
        self.current_launcher().launch_status()
    }

    fn is_available(&self) -> bool {
        self.is_enabled() && self.current_launcher().is_available()
    }

    fn socket_path(&self) -> PathBuf {
        self.socket.clone()
    }
}

/// This process's real uid, which the broker requires of its socket peer.
fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: `getuid` is always safe; it takes no arguments, reads only
        // process-local credential state, and cannot fail.
        unsafe { libc::getuid() as u32 }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(enabled: bool) -> ApfsHelperManager {
        let dir = std::env::temp_dir();
        ApfsHelperManager::new(
            dir.join("driven-apfs-helper-does-not-exist"),
            &dir,
            vec![PathBuf::from("/Users")],
            enabled,
        )
    }

    #[test]
    fn disabled_manager_reports_disabled_and_unavailable() {
        let m = manager(false);
        // The gate is checked BEFORE the launcher, so a disabled manager never
        // triggers the consent prompt - the whole point of the opt-in setting.
        assert_eq!(m.launch_status(), HelperLaunchStatus::Disabled);
        assert!(!m.is_available());
        assert!(!m.helper_launchable());
    }

    #[test]
    fn disabled_manager_never_launches_even_when_asked_eagerly() {
        let m = manager(false);
        m.launch_now();
        assert_eq!(m.launch_status(), HelperLaunchStatus::Disabled);
    }

    #[test]
    fn socket_path_is_fixed_for_the_session_and_unguessable() {
        let m = manager(true);
        let a = m.socket_path();
        let b = m.socket_path();
        assert_eq!(a, b, "socket path must be stable for the session");
        assert!(a
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("driven-apfs-") && s.ends_with(".sock")));
        // A second manager gets a DIFFERENT socket (random suffix).
        assert_ne!(a, manager(true).socket_path());
    }

    #[test]
    fn toggling_enabled_flips_the_gate() {
        let m = manager(false);
        assert_eq!(m.launch_status(), HelperLaunchStatus::Disabled);
        m.set_enabled(true);
        // Now the launcher (not the gate) decides. Off macOS the osascript stub
        // fails, and a missing sidecar fails on macOS, so the only guarantee
        // that holds on every CI host is "no longer short-circuited to Disabled
        // by the SETTING". Re-disabling must return to Disabled.
        m.set_enabled(false);
        assert_eq!(m.launch_status(), HelperLaunchStatus::Disabled);
        assert!(!m.is_available());
    }

    /// A manager whose sidecar path EXISTS, so `helper_launchable` gets past its
    /// `helper_exe.exists()` guard and the status logic is actually exercised.
    fn manager_with_real_exe(enabled: bool) -> ApfsHelperManager {
        let dir = std::env::temp_dir();
        ApfsHelperManager::new(
            std::env::current_exe().expect("current exe"),
            &dir,
            vec![PathBuf::from("/Users")],
            enabled,
        )
    }

    #[test]
    fn reading_status_never_triggers_a_consent_prompt() {
        // REGRESSION: `OsascriptLauncher::launch_status` is not a pure read - its
        // NotAttempted -> InFlight transition is what spawns the administrator
        // prompt. The status accessors are polled every time the Settings Rules
        // tab opens, so if any of them delegated blindly, merely opening Settings
        // would ask the user for their password.
        let m = manager_with_real_exe(true);
        for _ in 0..3 {
            assert!(!m.helper_alive());
            assert!(
                !m.launch_pending(),
                "a status read must never put the launcher in flight"
            );
            assert!(!m.launch_declined());
            // Not yet tried is LAUNCHABLE (the broker comes up on the first
            // locked file), so the UI must not call this degraded.
            assert!(
                m.helper_launchable(),
                "not-yet-attempted must count as launchable, matching Windows"
            );
        }
    }

    #[test]
    fn a_disabled_manager_is_never_launchable_even_with_a_real_sidecar() {
        let m = manager_with_real_exe(false);
        assert!(!m.helper_launchable());
        assert!(!m.helper_alive());
        assert!(!m.launch_pending());
        assert!(!m.launch_declined());
    }

    #[test]
    fn launch_args_carry_socket_allow_list_uid_and_pid() {
        let m = manager(true);
        let parsed = driven_apfs::launch::parse_helper_args(&m.args)
            .expect("manager-built argv must parse back");
        assert_eq!(parsed.socket, m.socket_path());
        assert_eq!(parsed.allowed_volumes, vec![PathBuf::from("/Users")]);
        assert_eq!(parsed.app_pid, std::process::id());
        assert_eq!(parsed.peer_uid, current_uid());
    }

    /// The bug this guards against cost the whole feature: with
    /// `std::env::temp_dir()` the real socket path was 110 bytes against
    /// Darwin's 104-byte `sockaddr_un.sun_path`, so the broker died with
    /// "path must be shorter than SUN_LEN" before serving a single request -
    /// on every Mac, not just unusual ones. Confirmed against the real broker
    /// binary on macOS 26.6. Nothing else in the suite binds a socket, so only
    /// a length assertion catches it.
    /// The liveness probe must distinguish a LIVE listener from a stale socket
    /// FILE. A broker that died leaves its socket on disk, so an
    /// existence-only check would keep reporting a healthy helper forever -
    /// which is the exact silent shape this probe exists to prevent.
    #[cfg(unix)]
    #[test]
    fn socket_probe_sees_a_live_listener_and_rejects_a_stale_file() {
        // `tempfile` rather than a hand-rolled `temp_dir().join(...)`: CodeQL
        // flags the latter as `rust/path-injection` (the process id is a
        // user-influenced value reaching a filesystem sink), and TempDir also
        // cleans up on panic, which the manual remove at the end did not.
        let dir = tempfile::tempdir().expect("probe tempdir");
        let sock = dir.path().join("probe.sock");

        // Nothing there at all.
        assert!(!socket_is_live(&sock), "absent socket is not live");

        // A real listener answers.
        let listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind probe socket");
        assert!(socket_is_live(&sock), "a bound listener must probe live");

        // Dropping the listener leaves the FILE behind but nothing listening -
        // connect must now fail (ECONNREFUSED), not succeed on file existence.
        drop(listener);
        assert!(sock.exists(), "the stale socket file is still on disk");
        assert!(
            !socket_is_live(&sock),
            "a stale socket file must NOT probe live"
        );
        // `dir` drops here, removing the socket with it.
    }

    #[test]
    fn a_ready_launcher_without_a_live_socket_is_not_reported_ready() {
        // The manager's socket path is never bound in this test, so a launcher
        // claiming Ready is exactly the "osascript spawned it, then it died"
        // case. It must degrade to Pending inside the grace window rather than
        // telling the UI locked-file backup is healthy.
        let m = manager_with_real_exe(true);
        assert_eq!(
            m.confirm_ready(HelperLaunchStatus::Ready),
            HelperLaunchStatus::Pending,
            "an unbacked Ready degrades to Pending while the broker may still be binding"
        );
        // Non-Ready statuses pass through untouched.
        assert_eq!(
            m.confirm_ready(HelperLaunchStatus::Declined),
            HelperLaunchStatus::Declined
        );
        assert_eq!(
            m.confirm_ready(HelperLaunchStatus::Disabled),
            HelperLaunchStatus::Disabled
        );
    }

    #[test]
    fn an_unbacked_ready_becomes_disabled_once_the_grace_window_expires() {
        let m = manager_with_real_exe(true);
        // Backdate the first-Ready stamp past the grace window.
        *m.ready_since.lock().expect("ready_since") = Some(
            std::time::Instant::now()
                .checked_sub(BROKER_READY_GRACE + std::time::Duration::from_secs(5))
                .expect("backdate"),
        );
        assert_eq!(
            m.confirm_ready(HelperLaunchStatus::Ready),
            HelperLaunchStatus::Disabled,
            "a broker whose socket never came up must end DISABLED (degraded + \
             diagnosable), not Pending forever"
        );
        // Disabled is what `helper_launchable` maps to false, which is what the
        // UI renders as degraded. That end-to-end mapping is not asserted here
        // on purpose: reaching it requires `launch_attempted`, and setting that
        // would make the accessors call the real launcher, whose NotAttempted
        // transition spawns an actual osascript consent prompt. See
        // `reading_status_never_triggers_a_consent_prompt` for that guard.
    }

    #[test]
    fn socket_path_fits_in_sockaddr_un() {
        // Darwin's sun_path is 104 bytes including the NUL terminator.
        const SUN_PATH_LEN: usize = 104;
        let dir = ApfsHelperManager::runtime_dir().expect("runtime dir");
        let sock = driven_apfs::launch::generate_socket_path(&dir);
        let len = sock.as_os_str().as_encoded_bytes().len();
        assert!(
            len < SUN_PATH_LEN,
            "socket path must fit in sockaddr_un: {len} bytes >= {SUN_PATH_LEN} for {}",
            sock.display()
        );
    }

    #[test]
    fn runtime_dir_avoids_the_long_per_user_tmpdir() {
        // Regression: macOS points TMPDIR at /var/folders/<xx>/<~20 chars>/T/,
        // which alone eats most of the sockaddr_un budget. The runtime dir must
        // not live under it.
        let dir = ApfsHelperManager::runtime_dir().expect("runtime dir");
        assert!(
            !dir.starts_with("/var/folders"),
            "runtime dir must avoid the long per-user TMPDIR: {}",
            dir.display()
        );
    }

    #[test]
    fn runtime_dir_is_creatable_and_private() {
        let dir = ApfsHelperManager::runtime_dir().expect("runtime dir");
        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700, "socket dir must be owner-only");
        }
    }
}
