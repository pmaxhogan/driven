//! Real pre/post backup hook runner (V2 pre/post backup hooks, DESIGN s17).
//!
//! `driven-core` stays free of process I/O, so the orchestrator runs hook
//! commands through this injected [`CommandRunner`]. This is the only place a
//! user-configured shell command is actually spawned. Commands run through the
//! platform shell (`sh -c` / `cmd /C`) with the orchestrator-supplied env vars
//! and a hard kill-on-timeout.

use std::time::Duration;

use async_trait::async_trait;
use driven_core::hooks::{CommandRunner, HookOutcome};

/// Runs hook commands through the platform shell, killing them on timeout.
#[derive(Debug, Default)]
pub struct TokioCommandRunner;

#[async_trait]
impl CommandRunner for TokioCommandRunner {
    async fn run(&self, command: &str, env: &[(String, String)], timeout: Duration) -> HookOutcome {
        let mut cmd = build_command(command);
        for (key, value) in env {
            cmd.env(key, value);
        }
        // Kill the child if this future is dropped (e.g. app shutdown).
        cmd.kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return HookOutcome {
                    exit_code: None,
                    timed_out: false,
                    spawn_error: Some(e.to_string()),
                }
            }
        };

        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => HookOutcome {
                exit_code: status.code(),
                timed_out: false,
                spawn_error: None,
            },
            Ok(Err(e)) => HookOutcome {
                exit_code: None,
                timed_out: false,
                spawn_error: Some(e.to_string()),
            },
            Err(_elapsed) => {
                // Exceeded the timeout: kill it and report a timeout.
                let _ = child.kill().await;
                HookOutcome {
                    exit_code: None,
                    timed_out: true,
                    spawn_error: None,
                }
            }
        }
    }
}

#[cfg(not(windows))]
fn build_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

#[cfg(windows)]
fn build_command(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.arg("/C").arg(command);
    cmd
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn zero_exit_succeeds() {
        let out = TokioCommandRunner
            .run("exit 0", &[], Duration::from_secs(5))
            .await;
        assert!(out.succeeded());
    }

    #[tokio::test]
    async fn nonzero_exit_is_reported() {
        let out = TokioCommandRunner
            .run("exit 3", &[], Duration::from_secs(5))
            .await;
        assert_eq!(out.exit_code, Some(3));
        assert!(!out.succeeded());
    }

    #[tokio::test]
    async fn env_vars_are_passed() {
        let out = TokioCommandRunner
            .run(
                "test \"$DRIVEN_HOOK\" = pre",
                &[("DRIVEN_HOOK".to_string(), "pre".to_string())],
                Duration::from_secs(5),
            )
            .await;
        assert!(out.succeeded(), "the hook saw DRIVEN_HOOK=pre");
    }

    #[tokio::test]
    async fn timeout_kills_a_long_command() {
        let out = TokioCommandRunner
            .run("sleep 10", &[], Duration::from_millis(100))
            .await;
        assert!(out.timed_out);
        assert!(!out.succeeded());
    }

    #[tokio::test]
    async fn unspawnable_shell_is_a_spawn_error_not_a_panic() {
        // A command that the shell cannot find still EXITS non-zero (the shell
        // runs), so spawn succeeds; assert the non-zero is surfaced.
        let out = TokioCommandRunner
            .run(
                "this-binary-does-not-exist-12345",
                &[],
                Duration::from_secs(5),
            )
            .await;
        assert!(!out.succeeded());
    }
}
