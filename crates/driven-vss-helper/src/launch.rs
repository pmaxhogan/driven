//! Elevated-launch plumbing for the helper broker (DESIGN s5.3.1).
//!
//! The main app spawns `driven-vss-helper.exe` elevated on demand
//! (`ShellExecuteW` `runas` verb -> one UAC prompt). The pipe name and the
//! configured backup roots are passed on the helper's command line, so:
//! - the pipe name carries a random suffix (unguessable), and
//! - the helper's allow-list of snapshot-able roots is fixed at launch by the
//!   app, not chosen per-request by an untrusted caller.
//!
//! The argv construction and the `--pipe`/`--allowed-root` parsing are pure and
//! unit-tested cross-OS; only the actual `ShellExecute` call is Windows-gated.

use std::path::PathBuf;

/// Command-line flag: the named-pipe name the helper serves on.
pub const ARG_PIPE: &str = "--pipe";
/// Command-line flag (repeatable): one configured backup root the helper may
/// snapshot files under.
pub const ARG_ALLOWED_ROOT: &str = "--allowed-root";

/// Generate a fresh, unguessable named-pipe name for one app session:
/// `\\.\pipe\driven-vss-<uuid-v4>`.
pub fn generate_pipe_name() -> String {
    format!(r"\\.\pipe\driven-vss-{}", uuid::Uuid::new_v4().simple())
}

/// The on-demand launch seam the app-side [`BrokeredVssProvider`] consults the
/// FIRST time a locked file needs the helper (DESIGN s5.3.1).
///
/// The provider does not launch the elevated broker itself: launch is an
/// app-level, at-most-once concern (one UAC prompt, one helper process, one
/// pipe name shared across every account's provider), so the app owns a single
/// launcher and hands the SAME `Arc<dyn HelperLauncher>` to each provider.
/// [`Self::ensure_launched`] is called lazily on the locked-file path and MUST
/// be idempotent + memoised: a first call launches the broker; later calls (and
/// calls from other accounts' providers) return the cached verdict without
/// re-prompting - a user who declined the UAC prompt is not asked again for the
/// rest of the session.
pub trait HelperLauncher: Send + Sync {
    /// Ensure the elevated helper has been launched for this session, launching
    /// it at most once. Returns `true` when the helper is believed up (launch
    /// succeeded, or a prior call already launched it), `false` when it could
    /// not be brought up (UAC declined, helper exe missing) so the caller
    /// degrades to skip-the-locked-file.
    fn ensure_launched(&self) -> bool;
}

/// Build the helper's argv (excluding the program path itself):
/// `--pipe <name> [--allowed-root <root>]...`.
pub fn helper_args(pipe_name: &str, allowed_roots: &[PathBuf]) -> Vec<String> {
    let mut args = vec![ARG_PIPE.to_string(), pipe_name.to_string()];
    for root in allowed_roots {
        args.push(ARG_ALLOWED_ROOT.to_string());
        args.push(root.to_string_lossy().into_owned());
    }
    args
}

/// Parse the helper's argv back into `(pipe_name, allowed_roots)`. Used by the
/// `driven-vss-helper` binary. Rejects a missing/empty pipe name.
pub fn parse_helper_args(args: &[String]) -> Result<(String, Vec<PathBuf>), String> {
    let mut pipe_name: Option<String> = None;
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            ARG_PIPE => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| format!("{ARG_PIPE} needs a value"))?;
                pipe_name = Some(v.clone());
                i += 2;
            }
            ARG_ALLOWED_ROOT => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| format!("{ARG_ALLOWED_ROOT} needs a value"))?;
                roots.push(PathBuf::from(v));
                i += 2;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let pipe_name = pipe_name
        .filter(|s| !s.is_empty())
        .ok_or("missing --pipe")?;
    Ok((pipe_name, roots))
}

/// Launch `helper_exe` elevated with `args` via the shell `runas` verb
/// (raises one UAC prompt). Returns when the process has been STARTED, not when
/// it exits (the helper runs for the session). On non-Windows this is
/// unsupported.
#[cfg(windows)]
pub fn launch_elevated(helper_exe: &std::path::Path, args: &[String]) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_CANCELLED};
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    // Join the args into a single parameter string with Windows quoting: any
    // arg containing a space or quote is wrapped in double quotes with inner
    // quotes escaped, so a spaced path (`C:\Program Files\...`) round-trips.
    let params: String = args
        .iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ");

    let verb = wide(std::ffi::OsStr::new("runas"));
    let file = wide(helper_exe.as_os_str());
    let params_w = wide(std::ffi::OsStr::new(&params));

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params_w.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };

    // SAFETY: `info` is a fully-initialised SHELLEXECUTEINFOW; the wide strings
    // outlive the call.
    let result = unsafe { ShellExecuteExW(&mut info) };
    match result {
        Ok(()) => {
            // Close the process handle we asked to keep open (we do not wait on
            // it here; the app manages the helper via the pipe).
            if !info.hProcess.is_invalid() {
                // SAFETY: hProcess is a valid handle SEE_MASK_NOCLOSEPROCESS gave us.
                let _ = unsafe { CloseHandle(info.hProcess) };
            }
            Ok(())
        }
        Err(_) => {
            // SAFETY: reading the thread's last-error code.
            let code = unsafe { GetLastError() };
            if code == ERROR_CANCELLED {
                Err("elevation was declined at the UAC prompt".to_string())
            } else {
                Err(format!("ShellExecuteEx(runas) failed (error {})", code.0))
            }
        }
    }
}

/// Non-Windows: elevated launch is unsupported (VSS is Windows-only).
#[cfg(not(windows))]
pub fn launch_elevated(_helper_exe: &std::path::Path, _args: &[String]) -> Result<(), String> {
    Err("the VSS helper is only supported on Windows".to_string())
}

/// Quote a single argv element for a Windows command-line parameter string.
#[cfg(any(windows, test))]
fn quote_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_string();
    }
    if arg.contains([' ', '\t', '"']) {
        // Escape embedded quotes and wrap.
        let escaped = arg.replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        arg.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_prefixed_and_unique() {
        let a = generate_pipe_name();
        let b = generate_pipe_name();
        assert!(a.starts_with(r"\\.\pipe\driven-vss-"));
        assert_ne!(a, b, "each session gets a fresh pipe name");
    }

    #[test]
    fn args_round_trip_through_parse() {
        let pipe = r"\\.\pipe\driven-vss-abc";
        let roots = vec![
            PathBuf::from(r"C:\Users\me\Documents"),
            PathBuf::from(r"D:\Backup Source"),
        ];
        let argv = helper_args(pipe, &roots);
        let (parsed_pipe, parsed_roots) = parse_helper_args(&argv).unwrap();
        assert_eq!(parsed_pipe, pipe);
        assert_eq!(parsed_roots, roots);
    }

    #[test]
    fn parse_rejects_missing_pipe() {
        let argv = vec![
            ARG_ALLOWED_ROOT.to_string(),
            r"C:\Users\me\Documents".to_string(),
        ];
        assert!(parse_helper_args(&argv).is_err());
    }

    #[test]
    fn parse_rejects_unknown_flag() {
        let argv = vec!["--bogus".to_string(), "x".to_string()];
        assert!(parse_helper_args(&argv).is_err());
    }

    #[test]
    fn parse_rejects_dangling_value_flag() {
        let argv = vec![ARG_PIPE.to_string()];
        assert!(parse_helper_args(&argv).is_err());
    }

    #[test]
    fn quote_arg_wraps_spaced_paths() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(r"C:\Program Files\x"), "\"C:\\Program Files\\x\"");
        assert_eq!(quote_arg(""), "\"\"");
    }
}
