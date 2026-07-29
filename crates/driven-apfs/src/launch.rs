//! Elevation launch plumbing for the root mount broker (DESIGN s5.3.2).
//!
//! The app spawns `driven-apfs-helper` as root on demand via
//! `osascript -e 'do shell script ... with administrator privileges'` - the
//! one consent path available to an UNSIGNED app (`SMJobBless`/`SMAppService`
//! both require code signing). One admin prompt per app session, mirroring the
//! Windows one-UAC-prompt model (DESIGN s5.3.1).
//!
//! The socket path carries a random suffix (unguessable) and the broker's
//! allow-list of mountable volumes is fixed at launch by the app, not chosen
//! per-request by an untrusted caller. Argv construction/parsing and both
//! quoting layers (shell + AppleScript literal) are pure and unit-tested
//! cross-OS - they compose a string a ROOT SHELL will execute, so they are
//! deliberately paranoid; only the actual `osascript` spawn is macOS-gated.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Command-line flag: the unix-socket path the broker serves on.
pub const ARG_SOCKET: &str = "--socket";
/// Command-line flag (repeatable): one volume mount point the broker may
/// mount snapshots of.
pub const ARG_ALLOWED_VOLUME: &str = "--allowed-volume";
/// Command-line flag: the uid the broker requires its socket peer to have.
pub const ARG_PEER_UID: &str = "--peer-uid";
/// Command-line flag: the app pid whose exit ends the broker's session.
pub const ARG_APP_PID: &str = "--app-pid";

/// Generate a fresh, unguessable socket path for one app session, under the
/// given per-user runtime directory (the caller creates it `0700`):
/// `<dir>/driven-apfs-<uuid>.sock`.
pub fn generate_socket_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(format!(
        "driven-apfs-{}.sock",
        uuid::Uuid::new_v4().simple()
    ))
}

/// Build the broker's argv (excluding the program path itself).
pub fn helper_args(
    socket: &Path,
    allowed_volumes: &[PathBuf],
    peer_uid: u32,
    app_pid: u32,
) -> Vec<String> {
    let mut args = vec![
        ARG_SOCKET.to_string(),
        socket.to_string_lossy().into_owned(),
        ARG_PEER_UID.to_string(),
        peer_uid.to_string(),
        ARG_APP_PID.to_string(),
        app_pid.to_string(),
    ];
    for v in allowed_volumes {
        args.push(ARG_ALLOWED_VOLUME.to_string());
        args.push(v.to_string_lossy().into_owned());
    }
    args
}

/// Parsed broker arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperArgs {
    /// The unix-socket path to serve on.
    pub socket: PathBuf,
    /// The volume mount points the broker may mount snapshots of.
    pub allowed_volumes: Vec<PathBuf>,
    /// The uid every socket peer must present.
    pub peer_uid: u32,
    /// The app pid whose exit ends the broker.
    pub app_pid: u32,
}

/// Parse the broker's argv back into [`HelperArgs`]. Used by the
/// `driven-apfs-helper` binary. Rejects missing/empty socket, uid, or pid.
pub fn parse_helper_args(args: &[String]) -> Result<HelperArgs, String> {
    let mut socket: Option<PathBuf> = None;
    let mut volumes: Vec<PathBuf> = Vec::new();
    let mut peer_uid: Option<u32> = None;
    let mut app_pid: Option<u32> = None;
    let mut i = 0;
    while i < args.len() {
        let take = |i: usize| -> Result<&String, String> {
            args.get(i + 1)
                .ok_or_else(|| format!("{} needs a value", args[i]))
        };
        match args[i].as_str() {
            ARG_SOCKET => {
                socket = Some(PathBuf::from(take(i)?));
                i += 2;
            }
            ARG_ALLOWED_VOLUME => {
                volumes.push(PathBuf::from(take(i)?));
                i += 2;
            }
            ARG_PEER_UID => {
                peer_uid = Some(
                    take(i)?
                        .parse()
                        .map_err(|_| format!("{ARG_PEER_UID} must be a uid"))?,
                );
                i += 2;
            }
            ARG_APP_PID => {
                app_pid = Some(
                    take(i)?
                        .parse()
                        .map_err(|_| format!("{ARG_APP_PID} must be a pid"))?,
                );
                i += 2;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let socket = socket
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or("missing --socket")?;
    Ok(HelperArgs {
        socket,
        allowed_volumes: volumes,
        peer_uid: peer_uid.ok_or("missing --peer-uid")?,
        app_pid: app_pid.ok_or("missing --app-pid")?,
    })
}

/// POSIX-shell single-quote `s` so a root shell treats it as one literal word.
/// (`'` becomes `'\''`.) Composing a command line for a ROOT shell is the one
/// place quoting bugs become privilege bugs, hence the dedicated tested fn.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Escape `s` for inclusion inside an AppleScript double-quoted string
/// literal (backslashes then quotes).
pub fn applescript_quote(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', r#"\""#)
}

/// Build the full `osascript -e <script>` script string that launches the
/// broker as root, detached (so `do shell script` returns once the consent
/// prompt resolves rather than blocking for the broker's lifetime).
pub fn osascript_script(helper_exe: &Path, args: &[String]) -> String {
    let mut sh = shell_quote(&helper_exe.to_string_lossy());
    for a in args {
        sh.push(' ');
        sh.push_str(&shell_quote(a));
    }
    // Detach: background the broker and silence its stdio; the app talks to it
    // over the socket, never over these pipes.
    let sh = format!("{sh} >/dev/null 2>&1 &");
    format!(
        r#"do shell script "{}" with administrator privileges with prompt "Driven needs administrator access to read locked files through an APFS snapshot.""#,
        applescript_quote(&sh)
    )
}

/// The broker's readiness for one locked-file open. Mirrors the Windows
/// helper's `LaunchStatus` contract (DESIGN s5.3.1) exactly:
/// [`Self::Pending`] must cause a TRANSIENT skip (retry next cycle), never a
/// permanent "locked" report, and [`Self::Declined`] is memoised so the user
/// is never re-prompted within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperLaunchStatus {
    /// The broker launch succeeded this session - connect and mount.
    Ready,
    /// A launch/consent prompt is in flight - skip transiently, retry next
    /// cycle, do not block.
    Pending,
    /// The user declined (or dismissed) the admin prompt this session.
    /// Memoised: no further prompt until app restart or setting re-toggle.
    Declined,
    /// The snapshot helper is not in play (setting off, or not macOS).
    Disabled,
}

/// Why a launch did not succeed. [`Self::Declined`] is memoised for the
/// session; [`Self::Failed`] is transient and may be retried on the next
/// enable-toggle or app start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    /// The user cancelled the admin prompt (`osascript` exits 1 with
    /// "User canceled." / error -128).
    Declined,
    /// Any other launch failure (missing binary, osascript error). Carries a
    /// secret-free detail for logs.
    Failed(String),
}

/// The on-demand launch seam the app-side [`crate::ApfsBrokeredProvider`]
/// consults on the locked-file path. Launch is an app-level at-most-once
/// concern (one admin prompt, one broker, one socket shared across every
/// account's provider), so the app owns a single launcher and hands the same
/// `Arc<dyn HelperLauncher>` to each provider.
pub trait HelperLauncher: Send + Sync {
    /// Report readiness, TRIGGERING an at-most-once lazy launch
    /// (non-blocking) when enabled and not yet attempted. Never blocks on the
    /// consent prompt.
    fn launch_status(&self) -> HelperLaunchStatus;

    /// Capability: is the snapshot helper in play at all this run? The
    /// executor reads this (via the provider) as its `elevated` input to
    /// `fallback_decision` - `true` whenever the broker is up OR can still be
    /// brought up; `false` once disabled or declined.
    fn is_available(&self) -> bool;

    /// The socket path the broker serves on (fixed for the session).
    fn socket_path(&self) -> PathBuf;
}

/// Internal launcher state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchState {
    NotAttempted,
    InFlight,
    Ready,
    Declined,
    Failed(String),
}

/// The production launcher: composes the osascript consent launch, runs it on
/// a background thread (the prompt can sit unanswered for minutes), and
/// memoises the outcome.
pub struct OsascriptLauncher {
    helper_exe: PathBuf,
    socket: PathBuf,
    args: Vec<String>,
    /// `Arc` so the background launch thread can publish its outcome after
    /// `launch_status` has returned `Pending`.
    state: std::sync::Arc<Mutex<LaunchState>>,
}

impl OsascriptLauncher {
    /// A launcher for `helper_exe`, serving `socket`, with the given launch
    /// argv (built via [`helper_args`]).
    pub fn new(helper_exe: PathBuf, socket: PathBuf, args: Vec<String>) -> Self {
        Self {
            helper_exe,
            socket,
            args,
            state: std::sync::Arc::new(Mutex::new(LaunchState::NotAttempted)),
        }
    }

    /// Run the blocking osascript consent launch. macOS only; extracted so
    /// the state machine above it stays cross-OS testable.
    #[cfg(target_os = "macos")]
    fn run_osascript(helper_exe: &Path, args: &[String]) -> Result<(), LaunchError> {
        let script = osascript_script(helper_exe, args);
        let out = std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(&script)
            .output()
            .map_err(|e| LaunchError::Failed(format!("osascript spawn: {e}")))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        // "User canceled." / error -128 = the user did not approve. Anything
        // else is a transient failure.
        if stderr.contains("-128") || stderr.to_lowercase().contains("user cancel") {
            Err(LaunchError::Declined)
        } else {
            Err(LaunchError::Failed(format!(
                "osascript exited {}: {}",
                out.status,
                stderr.trim()
            )))
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn run_osascript(_helper_exe: &Path, _args: &[String]) -> Result<(), LaunchError> {
        Err(LaunchError::Failed(
            "the APFS snapshot helper is only supported on macOS".to_string(),
        ))
    }
}

impl HelperLauncher for OsascriptLauncher {
    fn launch_status(&self) -> HelperLaunchStatus {
        let mut state = self.state.lock().expect("launcher state lock");
        match &*state {
            LaunchState::Ready => HelperLaunchStatus::Ready,
            LaunchState::InFlight => HelperLaunchStatus::Pending,
            LaunchState::Declined => HelperLaunchStatus::Declined,
            // A prior transient failure memoises as Disabled-for-this-session;
            // an enable-toggle constructs a fresh launcher and retries.
            LaunchState::Failed(_) => HelperLaunchStatus::Disabled,
            LaunchState::NotAttempted => {
                // At-most-once: only this NotAttempted -> InFlight transition
                // (made under the lock) spawns the consent thread.
                *state = LaunchState::InFlight;
                drop(state);
                let helper_exe = self.helper_exe.clone();
                let args = self.args.clone();
                let shared = std::sync::Arc::clone(&self.state);
                std::thread::Builder::new()
                    .name("driven-apfs-launch".into())
                    .spawn(move || {
                        let outcome = Self::run_osascript(&helper_exe, &args);
                        let mut s = shared.lock().expect("launcher state lock");
                        *s = match outcome {
                            Ok(()) => LaunchState::Ready,
                            Err(LaunchError::Declined) => LaunchState::Declined,
                            Err(LaunchError::Failed(d)) => LaunchState::Failed(d),
                        };
                    })
                    // A thread-spawn failure is a transient launch failure.
                    .map_err(|e| {
                        *self.state.lock().expect("launcher state lock") =
                            LaunchState::Failed(format!("spawn launch thread: {e}"));
                    })
                    .ok();
                HelperLaunchStatus::Pending
            }
        }
    }

    fn is_available(&self) -> bool {
        !matches!(
            &*self.state.lock().expect("launcher state lock"),
            LaunchState::Declined | LaunchState::Failed(_)
        )
    }

    fn socket_path(&self) -> PathBuf {
        self.socket.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_prefixed_and_unique() {
        let dir = Path::new("/tmp/driven-test");
        let a = generate_socket_path(dir);
        let b = generate_socket_path(dir);
        assert!(a
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("driven-apfs-"));
        assert!(a.to_string_lossy().ends_with(".sock"));
        assert_ne!(a, b, "each session gets a fresh socket path");
    }

    #[test]
    fn args_round_trip_through_parse() {
        let socket = Path::new("/Users/me/Library/Application Support/driven/x.sock");
        let vols = vec![PathBuf::from("/System/Volumes/Data"), PathBuf::from("/")];
        let argv = helper_args(socket, &vols, 501, 4242);
        let parsed = parse_helper_args(&argv).unwrap();
        assert_eq!(parsed.socket, socket);
        assert_eq!(parsed.allowed_volumes, vols);
        assert_eq!(parsed.peer_uid, 501);
        assert_eq!(parsed.app_pid, 4242);
    }

    #[test]
    fn parse_rejects_missing_required_flags() {
        assert!(parse_helper_args(&[]).is_err());
        let no_uid = helper_args(Path::new("/s.sock"), &[], 501, 1)
            .into_iter()
            .filter(|a| a != ARG_PEER_UID && a != "501")
            .collect::<Vec<_>>();
        assert!(parse_helper_args(&no_uid).is_err());
    }

    #[test]
    fn parse_rejects_unknown_flag() {
        assert!(parse_helper_args(&["--bogus".into(), "x".into()]).is_err());
    }

    #[test]
    fn shell_quote_neutralises_quotes_and_spaces() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        // A metacharacter-laden path stays one inert word.
        assert_eq!(shell_quote("$(rm -rf /);`x`"), "'$(rm -rf /);`x`'");
    }

    #[test]
    fn applescript_quote_escapes_backslashes_then_quotes() {
        assert_eq!(applescript_quote(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn osascript_script_quotes_the_whole_chain() {
        let script = osascript_script(
            Path::new("/Applications/Driven.app/Contents/MacOS/driven-apfs-helper"),
            &helper_args(
                Path::new("/tmp/dir with space/s.sock"),
                &[PathBuf::from("/")],
                501,
                7,
            ),
        );
        assert!(script.starts_with(r#"do shell script ""#));
        assert!(script.contains("with administrator privileges"));
        // The spaced socket path must ride inside shell quotes, AppleScript-escaped.
        assert!(script.contains(r"dir with space"));
        // Detached: the composed shell command backgrounds the broker.
        assert!(script.contains(r"2>&1 &"));
    }
}
