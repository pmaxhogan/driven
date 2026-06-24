//! Windows "run elevated on login" one-click + "restart elevated now"
//! (ROADMAP M3.5 deferred to M5; CODEX_NOTES "Task Scheduler ... DEFERRED to
//! M5").
//!
//! Volume Shadow Copy (DESIGN s5.3) needs Administrator elevation. M3.5
//! shipped the backend hooks (`is_elevated`, the `VssProvider` degrade path);
//! M5 adds the user-facing actions:
//! - [`set_run_elevated_on_login`] registers (or removes) a Windows Task
//!   Scheduler task that launches Driven at logon with HIGHEST run level
//!   (`schtasks /Create /RL HIGHEST ...` / `schtasks /Delete`), spawned as a
//!   plain `std::process::Command` (no new `windows` crate dep needed);
//! - [`is_run_elevated_on_login_enabled`] queries whether that task exists
//!   (`schtasks /Query /TN ...`);
//! - [`restart_elevated`] relaunches the app elevated now (the DESIGN s5.3
//!   "Restart Driven elevated?" prompt) and exits the current instance.
//!
//! Both are `cfg(windows)`-gated; the non-Windows arms are explicit
//! `Unsupported` errors so the call sites compile cross-platform.
//!
//! ## Error shape
//!
//! Every fallible function returns `anyhow::Result`. On failure the error
//! carries a single-line, secret-free message that names the failing
//! `schtasks` verb, its exit status, and the captured stderr - so the IPC
//! layer's `CommandError { message }` (commands/mod.rs) serialises a stable,
//! displayable JSON shape to the webview without leaking anything sensitive.
//! No function panics; a non-zero process exit is mapped to `Err`, not a
//! `.unwrap()`.

#[cfg(windows)]
use tauri::AppHandle;

/// The Task Scheduler task name Driven registers for elevated logon start.
///
/// Stable + namespaced so a `/Query` / `/Delete` always targets exactly the
/// task this app created (never a user's unrelated "Driven"-named task).
pub const ELEVATED_LOGON_TASK_NAME: &str = "DrivenRunElevatedOnLogin";

// -----------------------------------------------------------------------------
// Pure argv construction (testable without admin or actually touching the
// Task Scheduler - the runtime functions below feed these into Command).
// -----------------------------------------------------------------------------

/// The `/TR` action string for the logon task: the absolute exe path followed
/// by the `--minimized` flag so a login start boots straight to the tray.
///
/// `schtasks` accepts the whole action as one `/TR` argument; when the path
/// contains spaces (`C:\Program Files\...`) the action is wrapped in escaped
/// inner double-quotes so the scheduler stores `"<exe>" --minimized` and
/// launches the right binary. `std::process::Command` passes each argv element
/// verbatim (it does its own Windows quoting), so we build the logical string
/// here and let `Command` handle the outer quoting.
#[cfg(any(windows, test))]
fn task_run_action(exe_path: &str) -> String {
    if exe_path.contains(' ') {
        // Inner quotes so Task Scheduler treats the spaced path as the program
        // and `--minimized` as its argument.
        format!("\"{exe_path}\" --minimized")
    } else {
        format!("{exe_path} --minimized")
    }
}

/// argv for CREATING the elevated-on-logon task.
///
/// `schtasks /Create /TN <name> /TR <action> /SC ONLOGON /RL HIGHEST /F`:
/// - `/SC ONLOGON` - trigger at user logon;
/// - `/RL HIGHEST`  - run with the account's highest privileges (elevated);
/// - `/F`           - force/overwrite an existing task of the same name
///   (idempotent re-enable).
#[cfg(any(windows, test))]
fn create_task_argv(exe_path: &str) -> Vec<String> {
    vec![
        "/Create".to_string(),
        "/TN".to_string(),
        ELEVATED_LOGON_TASK_NAME.to_string(),
        "/TR".to_string(),
        task_run_action(exe_path),
        "/SC".to_string(),
        "ONLOGON".to_string(),
        "/RL".to_string(),
        "HIGHEST".to_string(),
        "/F".to_string(),
    ]
}

/// argv for DELETING the elevated-on-logon task.
///
/// `schtasks /Delete /TN <name> /F` (force, no confirmation prompt). Deleting
/// an absent task is treated as success by the caller (idempotent disable).
#[cfg(any(windows, test))]
fn delete_task_argv() -> Vec<String> {
    vec![
        "/Delete".to_string(),
        "/TN".to_string(),
        ELEVATED_LOGON_TASK_NAME.to_string(),
        "/F".to_string(),
    ]
}

/// argv for QUERYING whether the elevated-on-logon task exists.
///
/// `schtasks /Query /TN <name>` exits 0 when the task is present, non-zero
/// when it is absent.
#[cfg(any(windows, test))]
fn query_task_argv() -> Vec<String> {
    vec![
        "/Query".to_string(),
        "/TN".to_string(),
        ELEVATED_LOGON_TASK_NAME.to_string(),
    ]
}

// -----------------------------------------------------------------------------
// Windows runtime implementation
// -----------------------------------------------------------------------------

/// Run `schtasks.exe` with the given argv, capturing status + stderr.
///
/// Returns `Ok(output)` for ANY completed process (including a non-zero exit -
/// the caller decides whether that exit is an error or, for delete, an
/// idempotent no-op). Returns `Err` only when the process could not be spawned
/// at all (e.g. `schtasks.exe` missing from PATH).
#[cfg(windows)]
fn run_schtasks(args: &[String]) -> anyhow::Result<std::process::Output> {
    use anyhow::Context as _;
    std::process::Command::new("schtasks.exe")
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn schtasks.exe (args: {})", args.join(" ")))
}

/// Build a single-line, secret-free error message from a failed `schtasks`
/// invocation (verb + exit status + trimmed stderr/stdout). schtasks emits its
/// diagnostics on stdout in some locales and stderr in others, so we fall back
/// to stdout when stderr is empty.
#[cfg(windows)]
fn schtasks_error_message(verb: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let detail = if stderr.is_empty() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.trim().to_string()
    } else {
        stderr.to_string()
    };
    let code = match output.status.code() {
        Some(c) => c.to_string(),
        None => "signal".to_string(),
    };
    // Collapse any embedded newlines so the message stays one line for the
    // serialized CommandError shape.
    let detail = detail.replace(['\r', '\n'], " ");
    let detail = detail.trim();
    if detail.is_empty() {
        format!("schtasks {verb} failed (exit {code})")
    } else {
        format!("schtasks {verb} failed (exit {code}): {detail}")
    }
}

/// `true` when a non-zero `schtasks /Delete` exit means "the task did not
/// exist" - which we treat as an idempotent success, not an error.
///
/// schtasks does not return a documented stable code for "task not found"
/// across Windows versions/locales, so we match on the message text. The
/// English message is `ERROR: The system cannot find the file specified.`
/// (paired with the `8007002`/`0x80070002` HRESULT it sometimes prints); we
/// also accept the localisation-robust signal of the task name appearing with
/// "does not exist" / "cannot find". A false negative here only downgrades an
/// already-absent task to a surfaced error (safe direction); it never deletes
/// the wrong task.
#[cfg(windows)]
fn delete_exit_is_not_found(output: &std::process::Output) -> bool {
    let combined = format!(
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_ascii_lowercase();
    combined.contains("cannot find the file")
        || combined.contains("does not exist")
        || combined.contains("the system cannot find")
        || combined.contains("0x80070002")
        || combined.contains("error: the specified task name")
}

/// Register (or remove) a Windows Task Scheduler task that launches Driven
/// elevated at logon (ROADMAP M3.5 -> M5; DESIGN s5.3).
///
/// `enable = true` creates the task (`/Create ... /SC ONLOGON /RL HIGHEST /F`);
/// `false` deletes it (`/Delete ... /F`). Idempotent in both directions:
/// `/Create /F` overwrites an existing task, and deleting an absent task is
/// reported as success (see [`delete_exit_is_not_found`]).
///
/// Resolves the program to schedule via [`std::env::current_exe`]. Creating
/// (and later running) the task with `/RL HIGHEST` requires Administrator
/// rights; an un-elevated `/Create` fails with an access-denied message that is
/// surfaced verbatim (minus newlines) in the returned error - the caller (the
/// Settings banner) should pair this with [`restart_elevated`] when elevation
/// is not yet available.
#[cfg(windows)]
pub fn set_run_elevated_on_login(enable: bool) -> anyhow::Result<()> {
    if enable {
        let exe = std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("could not resolve current executable path: {e}"))?;
        let exe = exe.to_string_lossy().into_owned();
        let args = create_task_argv(&exe);
        let output = run_schtasks(&args)?;
        if !output.status.success() {
            anyhow::bail!("{}", schtasks_error_message("/Create", &output));
        }
        tracing::info!(
            task = ELEVATED_LOGON_TASK_NAME,
            "registered run-elevated-on-login Task Scheduler task"
        );
        Ok(())
    } else {
        let args = delete_task_argv();
        let output = run_schtasks(&args)?;
        if output.status.success() {
            tracing::info!(
                task = ELEVATED_LOGON_TASK_NAME,
                "removed run-elevated-on-login Task Scheduler task"
            );
            Ok(())
        } else if delete_exit_is_not_found(&output) {
            // Already absent - idempotent disable.
            tracing::debug!(
                task = ELEVATED_LOGON_TASK_NAME,
                "run-elevated-on-login task already absent; disable is a no-op"
            );
            Ok(())
        } else {
            anyhow::bail!("{}", schtasks_error_message("/Delete", &output));
        }
    }
}

/// Non-Windows: elevated logon start is unsupported (VSS is Windows-only).
#[cfg(not(windows))]
pub fn set_run_elevated_on_login(enable: bool) -> anyhow::Result<()> {
    let _ = enable;
    anyhow::bail!("run-elevated-on-login is only supported on Windows")
}

/// `true` when the elevated-on-logon Task Scheduler task currently exists.
///
/// Runs `schtasks /Query /TN <name>`: exit 0 -> present, any non-zero exit ->
/// absent. Querying does NOT require elevation. Returns `Err` only when
/// `schtasks.exe` itself cannot be spawned.
#[cfg(windows)]
pub fn is_run_elevated_on_login_enabled() -> anyhow::Result<bool> {
    let args = query_task_argv();
    let output = run_schtasks(&args)?;
    Ok(output.status.success())
}

/// Non-Windows: there is no such task; always `false`.
#[cfg(not(windows))]
pub fn is_run_elevated_on_login_enabled() -> anyhow::Result<bool> {
    Ok(false)
}

/// Relaunch Driven elevated now and exit the current (un-elevated) instance
/// (the DESIGN s5.3 "Restart Driven elevated?" action).
///
/// Mechanism: spawn the current executable through PowerShell's
/// `Start-Process -Verb RunAs`, which performs a `ShellExecute("runas")` and
/// raises the UAC consent prompt. The relaunched instance carries
/// `--minimized` so it boots to the tray exactly like an autostart launch.
/// Once the elevated process has been handed off to PowerShell we
/// `app.exit(0)` the current (un-elevated) instance so only one runs.
///
/// Why PowerShell `Start-Process -Verb RunAs` rather than a raw spawn: the
/// "runas" verb is the documented user-mode path to trigger UAC elevation, and
/// it is reachable from `std::process::Command` with NO new crate dependency
/// (the alternative, `ShellExecuteExW`, would pull in `windows`/`Win32_UI_Shell`
/// bindings this module is required to avoid). If the user declines the UAC
/// prompt, `Start-Process` fails and we DO NOT exit the current instance - we
/// return the error so the un-elevated app keeps running (fail-safe: never
/// leave the user with no running instance).
///
/// Note: this does NOT go through Task Scheduler. `set_run_elevated_on_login`
/// is the persistent "every logon" path; `restart_elevated` is the immediate
/// one-shot "elevate right now" path, exactly mirroring the two DESIGN s5.3
/// affordances ("Run as administrator" now vs. "add a Task Scheduler entry").
#[cfg(windows)]
pub fn restart_elevated(app: &AppHandle) -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("could not resolve current executable path: {e}"))?;
    let exe = exe.to_string_lossy().into_owned();

    // Build: powershell -NoProfile -NonInteractive -Command
    //   Start-Process -FilePath '<exe>' -ArgumentList '--minimized' -Verb RunAs
    // Single-quote the path for PowerShell and double any embedded single
    // quotes (PowerShell's literal-string escape) so a path with a quote in it
    // cannot break out of the string. `Start-Process` blocks until the
    // elevated child is launched (or the UAC prompt is declined).
    let ps_exe = exe.replace('\'', "''");
    let command =
        format!("Start-Process -FilePath '{ps_exe}' -ArgumentList '--minimized' -Verb RunAs");
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &command])
        .status()
        .map_err(|e| anyhow::anyhow!("failed to spawn powershell.exe for elevated restart: {e}"))?;

    if !status.success() {
        // UAC declined, or PowerShell failed. Keep the current instance alive.
        let code = match status.code() {
            Some(c) => c.to_string(),
            None => "signal".to_string(),
        };
        anyhow::bail!(
            "elevated restart was not started (the UAC prompt may have been declined; \
             powershell exit {code})"
        );
    }

    tracing::info!("relaunched Driven elevated; exiting un-elevated instance");
    app.exit(0);
    Ok(())
}

/// Non-Windows: elevated restart is unsupported.
#[cfg(not(windows))]
pub fn restart_elevated<A>(app: &A) -> anyhow::Result<()> {
    let _ = app;
    anyhow::bail!("restart-elevated is only supported on Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_action_no_spaces_unquoted() {
        let action = task_run_action(r"C:\Driven\driven.exe");
        assert_eq!(action, r"C:\Driven\driven.exe --minimized");
    }

    #[test]
    fn task_action_with_spaces_is_inner_quoted() {
        let action = task_run_action(r"C:\Program Files\Driven\driven.exe");
        assert_eq!(
            action,
            r#""C:\Program Files\Driven\driven.exe" --minimized"#
        );
    }

    #[test]
    fn create_argv_is_exact_spec_shape() {
        let argv = create_task_argv(r"C:\Driven\driven.exe");
        assert_eq!(
            argv,
            vec![
                "/Create".to_string(),
                "/TN".to_string(),
                "DrivenRunElevatedOnLogin".to_string(),
                "/TR".to_string(),
                r"C:\Driven\driven.exe --minimized".to_string(),
                "/SC".to_string(),
                "ONLOGON".to_string(),
                "/RL".to_string(),
                "HIGHEST".to_string(),
                "/F".to_string(),
            ]
        );
    }

    #[test]
    fn create_argv_carries_highest_run_level_and_onlogon() {
        let argv = create_task_argv(r"C:\Driven\driven.exe");
        // /RL HIGHEST is the elevation knob - it MUST be present and paired.
        let rl = argv.iter().position(|a| a == "/RL").expect("/RL present");
        assert_eq!(argv.get(rl + 1).map(String::as_str), Some("HIGHEST"));
        // /SC ONLOGON is the logon trigger.
        let sc = argv.iter().position(|a| a == "/SC").expect("/SC present");
        assert_eq!(argv.get(sc + 1).map(String::as_str), Some("ONLOGON"));
        // /F forces overwrite for an idempotent re-enable.
        assert!(argv.iter().any(|a| a == "/F"), "/F (force) present");
    }

    #[test]
    fn create_argv_targets_only_our_task_name() {
        let argv = create_task_argv(r"C:\Driven\driven.exe");
        let tn = argv.iter().position(|a| a == "/TN").expect("/TN present");
        assert_eq!(
            argv.get(tn + 1).map(String::as_str),
            Some(ELEVATED_LOGON_TASK_NAME)
        );
    }

    #[test]
    fn create_argv_with_spaced_path_inner_quotes_the_action() {
        let argv = create_task_argv(r"C:\Program Files\Driven\driven.exe");
        let tr = argv.iter().position(|a| a == "/TR").expect("/TR present");
        assert_eq!(
            argv.get(tr + 1).map(String::as_str),
            Some(r#""C:\Program Files\Driven\driven.exe" --minimized"#)
        );
    }

    #[test]
    fn delete_argv_is_exact_spec_shape() {
        assert_eq!(
            delete_task_argv(),
            vec![
                "/Delete".to_string(),
                "/TN".to_string(),
                "DrivenRunElevatedOnLogin".to_string(),
                "/F".to_string(),
            ]
        );
    }

    #[test]
    fn query_argv_is_exact_spec_shape() {
        assert_eq!(
            query_task_argv(),
            vec![
                "/Query".to_string(),
                "/TN".to_string(),
                "DrivenRunElevatedOnLogin".to_string(),
            ]
        );
    }

    #[test]
    fn task_name_is_namespaced_constant() {
        // Guards against an accidental rename to a generic "Driven" that could
        // collide with an unrelated user task on /Delete or /Query.
        assert_eq!(ELEVATED_LOGON_TASK_NAME, "DrivenRunElevatedOnLogin");
    }

    // -------------------------------------------------------------------------
    // Live Task Scheduler round-trip. This actually creates + deletes the task
    // and so needs Administrator rights (a /RL HIGHEST create from an
    // un-elevated token fails with access-denied). We HONESTLY gate-skip when
    // not elevated (NOT #[ignore]-faked): a non-admin or non-Windows CI run
    // records a clear skip reason and returns, while a local `cargo test` from
    // an elevated shell exercises the real create -> query -> delete cycle.
    // -------------------------------------------------------------------------
    #[cfg(windows)]
    #[test]
    fn live_create_query_delete_roundtrip_requires_admin() {
        if !driven_vss::is_elevated() {
            eprintln!(
                "SKIP live_create_query_delete_roundtrip_requires_admin: not elevated \
                 (creating a /RL HIGHEST Task Scheduler task needs Administrator); \
                 run from an elevated shell to exercise the real schtasks path"
            );
            return;
        }

        // Clean slate in case a prior aborted run left the task behind.
        let _ = set_run_elevated_on_login(false);

        // Enable -> the task must now exist.
        set_run_elevated_on_login(true).expect("enable (create) should succeed when elevated");
        assert!(
            is_run_elevated_on_login_enabled().expect("query should not error"),
            "task should be present after enable"
        );

        // Re-enable is idempotent (/F overwrites) - must not error.
        set_run_elevated_on_login(true).expect("re-enable should be idempotent");

        // Disable -> the task must be gone.
        set_run_elevated_on_login(false).expect("disable (delete) should succeed");
        assert!(
            !is_run_elevated_on_login_enabled().expect("query should not error"),
            "task should be absent after disable"
        );

        // Disabling an already-absent task is an idempotent no-op (not an error).
        set_run_elevated_on_login(false)
            .expect("disable of an absent task should be an idempotent success");
    }
}
