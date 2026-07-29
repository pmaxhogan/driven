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

/// Directory (created `0700`) holding this session's broker socket.
const RUNTIME_DIR_NAME: &str = "driven-apfs";

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
        let dir = std::env::temp_dir().join(RUNTIME_DIR_NAME);
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
    /// decline / transient failure so the next status read re-prompts.
    fn reset_launcher(&self) {
        let fresh = Arc::new(OsascriptLauncher::new(
            self.helper_exe.clone(),
            self.socket.clone(),
            self.args.clone(),
        ));
        *self.launcher.lock().unwrap_or_else(|p| p.into_inner()) = fresh;
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
        let _ = self.current_launcher().launch_status();
    }

    // --- status accessors (for get_apfs_helper_status) ----------------------

    /// STATUS: the broker is up and serving this session.
    #[must_use]
    pub fn helper_alive(&self) -> bool {
        self.is_enabled() && self.current_launcher().launch_status() == HelperLaunchStatus::Ready
    }

    /// STATUS: a consent prompt is in flight - the UI shows a "waiting for
    /// approval" hint.
    #[must_use]
    pub fn launch_pending(&self) -> bool {
        self.is_enabled() && self.current_launcher().launch_status() == HelperLaunchStatus::Pending
    }

    /// STATUS: the user declined the admin prompt this session (memoised).
    #[must_use]
    pub fn launch_declined(&self) -> bool {
        self.is_enabled() && self.current_launcher().launch_status() == HelperLaunchStatus::Declined
    }

    /// STATUS: locked-file backup can (still) happen - the setting is on, the
    /// sidecar exists, and the broker is up / coming up / not yet tried.
    #[must_use]
    pub fn helper_launchable(&self) -> bool {
        if !self.is_enabled() || !self.helper_exe.exists() {
            return false;
        }
        matches!(
            self.current_launcher().launch_status(),
            HelperLaunchStatus::Ready | HelperLaunchStatus::Pending
        )
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
