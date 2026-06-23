//! Windows "run elevated on login" one-click + "restart elevated now"
//! (ROADMAP M3.5 deferred to M5; CODEX_NOTES "Task Scheduler ... DEFERRED to
//! M5").
//!
//! Volume Shadow Copy (DESIGN s5.3) needs Administrator elevation. M3.5
//! shipped the backend hooks (`is_elevated`, the `VssProvider` degrade path);
//! M5 adds the user-facing actions:
//! - [`set_run_elevated_on_login`] registers (or removes) a Windows Task
//!   Scheduler task that launches Driven at logon with HIGHEST run level
//!   (`schtasks /create /RL HIGHEST ...` / `schtasks /delete`), spawned as a
//!   plain `std::process::Command` (no new `windows` crate dep needed);
//! - [`restart_elevated`] relaunches the app elevated now (the DESIGN s5.3
//!   "Restart Driven elevated?" prompt) and exits the current instance.
//!
//! Both are `cfg(windows)`-gated; the non-Windows arms are explicit
//! `Unsupported` no-ops so the call sites compile cross-platform.

#[cfg(windows)]
use tauri::AppHandle;

/// The Task Scheduler task name Driven registers for elevated logon start.
#[cfg(windows)]
pub const ELEVATED_LOGON_TASK_NAME: &str = "DrivenRunElevatedOnLogin";

/// Register (or remove) a Windows Task Scheduler task that launches Driven
/// elevated at logon (ROADMAP M3.5 -> M5).
///
/// `enable = true` creates the task; `false` deletes it. Idempotent: deleting
/// an absent task is not an error.
///
/// TODO(M5, windows): build a `schtasks.exe` invocation -
/// `/create /tn ELEVATED_LOGON_TASK_NAME /tr "<current_exe> --minimized" /sc
/// ONLOGON /RL HIGHEST /f` to enable, `/delete /tn ... /f` to disable - run
/// it via `std::process::Command`, and map a non-zero exit to an error.
#[cfg(windows)]
pub fn set_run_elevated_on_login(enable: bool) -> anyhow::Result<()> {
    let _ = enable;
    todo!("M5 windows: schtasks /create /RL HIGHEST ... | /delete via std::process::Command")
}

/// Non-Windows: elevated logon start is unsupported (VSS is Windows-only).
#[cfg(not(windows))]
pub fn set_run_elevated_on_login(enable: bool) -> anyhow::Result<()> {
    let _ = enable;
    anyhow::bail!("run-elevated-on-login is only supported on Windows")
}

/// Relaunch Driven elevated now and exit the current (un-elevated) instance
/// (the DESIGN s5.3 "Restart Driven elevated?" action).
///
/// TODO(M5, windows): `ShellExecuteW`-style elevated relaunch - run the
/// current exe via `cmd`/`powershell Start-Process -Verb RunAs` (or a
/// `runas`-equivalent), preserving the `--minimized` arg, then
/// `app.exit(0)` once the elevated process is launched.
#[cfg(windows)]
pub fn restart_elevated(app: &AppHandle) -> anyhow::Result<()> {
    let _ = app;
    todo!("M5 windows: relaunch current exe elevated (RunAs), then app.exit(0)")
}

/// Non-Windows: elevated restart is unsupported.
#[cfg(not(windows))]
pub fn restart_elevated<A>(app: &A) -> anyhow::Result<()> {
    let _ = app;
    anyhow::bail!("restart-elevated is only supported on Windows")
}
