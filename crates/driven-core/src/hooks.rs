//! Pre/post backup hook seam (V2 pre/post backup shell hooks, DESIGN s17).
//!
//! `driven-core` stays free of direct process I/O, so the orchestrator runs a
//! user-configured shell command through this injected [`CommandRunner`]
//! trait. The app wires a real tokio-process implementation; tests inject a
//! fake. The default [`NoopCommandRunner`] reports success without running
//! anything, so the gate is inert until a real runner is attached.

use std::time::Duration;

use async_trait::async_trait;

/// Which hook is being run, for env (`DRIVEN_HOOK`) and the activity row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    /// Runs before a backup cycle touches any source.
    Pre,
    /// Runs after a backup cycle's source loop, regardless of outcome.
    Post,
}

impl HookKind {
    /// The lowercase discriminant used in env vars + the `hook.<kind>`
    /// activity event type.
    pub fn as_str(self) -> &'static str {
        match self {
            HookKind::Pre => "pre",
            HookKind::Post => "post",
        }
    }
}

/// The outcome of running a hook command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    /// The process exit code when it exited normally; `None` when it was
    /// killed (timeout) or never produced an exit status.
    pub exit_code: Option<i32>,
    /// True when the command was killed for exceeding its timeout.
    pub timed_out: bool,
    /// A spawn / wait error (e.g. the shell or binary was not found) when the
    /// runner could not run the command at all; `None` otherwise.
    pub spawn_error: Option<String>,
}

impl HookOutcome {
    /// A clean success: exit 0, not timed out, spawned fine.
    pub fn success() -> Self {
        Self {
            exit_code: Some(0),
            timed_out: false,
            spawn_error: None,
        }
    }

    /// True only when the command ran to completion with a zero exit code.
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.spawn_error.is_none() && self.exit_code == Some(0)
    }

    /// A short human description for the activity-log message.
    pub fn describe(&self) -> String {
        if let Some(err) = &self.spawn_error {
            format!("failed to run ({err})")
        } else if self.timed_out {
            "timed out".to_string()
        } else {
            match self.exit_code {
                Some(0) => "ok".to_string(),
                Some(code) => format!("exited with code {code}"),
                None => "killed".to_string(),
            }
        }
    }
}

/// Runs a user-configured shell command (the pre/post backup hooks).
///
/// Implementations receive the raw command string, a set of `(key, value)`
/// environment variables to pass to it, and a timeout after which the command
/// must be killed (returning `timed_out: true`). They must never panic or
/// propagate an error: a command that cannot be spawned returns a
/// [`HookOutcome`] with `spawn_error` set.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `command`, passing `env`, killing it after `timeout`.
    async fn run(&self, command: &str, env: &[(String, String)], timeout: Duration) -> HookOutcome;
}

/// The default runner: reports success without running anything. Used when no
/// real runner is injected (the orchestrator's `new` default), so a configured
/// hook is simply inert until the app wires a real [`CommandRunner`].
#[derive(Debug, Default)]
pub struct NoopCommandRunner;

#[async_trait]
impl CommandRunner for NoopCommandRunner {
    async fn run(
        &self,
        _command: &str,
        _env: &[(String, String)],
        _timeout: Duration,
    ) -> HookOutcome {
        HookOutcome::success()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn succeeded_only_on_clean_zero_exit() {
        assert!(HookOutcome::success().succeeded());
        assert!(!HookOutcome {
            exit_code: Some(1),
            timed_out: false,
            spawn_error: None,
        }
        .succeeded());
        assert!(!HookOutcome {
            exit_code: None,
            timed_out: true,
            spawn_error: None,
        }
        .succeeded());
        assert!(!HookOutcome {
            exit_code: None,
            timed_out: false,
            spawn_error: Some("not found".into()),
        }
        .succeeded());
    }

    #[test]
    fn describe_is_human_readable() {
        assert_eq!(HookOutcome::success().describe(), "ok");
        assert_eq!(
            HookOutcome {
                exit_code: Some(2),
                timed_out: false,
                spawn_error: None
            }
            .describe(),
            "exited with code 2"
        );
        assert_eq!(
            HookOutcome {
                exit_code: None,
                timed_out: true,
                spawn_error: None
            }
            .describe(),
            "timed out"
        );
    }

    #[tokio::test]
    async fn noop_runner_reports_success() {
        let r = NoopCommandRunner;
        let out = r.run("anything", &[], Duration::from_secs(1)).await;
        assert!(out.succeeded());
    }
}
