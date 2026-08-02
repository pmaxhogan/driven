//! Scenario contract + runner for the app-level e2e suite.
//!
//! Mirrors the `driven-chaos` shape (name / description / run -> verdict)
//! at the APP level: every scenario boots the real desktop binary through
//! WebDriver and asserts through the same UI + IPC surface a user exercises.
//! PASS semantics follow STRESS_HARNESS s9: exit 0 = every scenario passed or
//! skipped (with its skip reason printed); any FAIL is exit 1.

use std::path::PathBuf;
use std::time::Duration;

use crate::session::Env;

/// The outcome of one scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every assertion held.
    Pass,
    /// The environment cannot run this scenario (missing tmpfs mount, running
    /// as root where non-root is required, ...). Not a failure.
    Skip(String),
    /// An assertion failed (message carries the evidence pointer).
    Fail(String),
}

/// Context handed to every scenario run.
pub struct Ctx {
    /// Per-scenario artifacts dir (screenshots, preserved logs, page source).
    pub artifacts: PathBuf,
}

impl Ctx {
    /// Build the context for `scenario_name`, creating its artifacts dir.
    pub fn new(scenario_name: &str) -> anyhow::Result<Self> {
        let artifacts = Env::artifacts_dir().join(scenario_name);
        std::fs::create_dir_all(&artifacts)?;
        Ok(Self { artifacts })
    }
}

/// One app-level scenario.
#[async_trait::async_trait]
pub trait Scenario: Send + Sync {
    /// Stable kebab-case id (the `run <name>` selector).
    fn name(&self) -> &'static str;
    /// One-line human description of what is proven.
    fn description(&self) -> &'static str;
    /// Run to a verdict. Infrastructure errors (driver won't boot, ...) may be
    /// returned as `Err` and are reported as failures with their chain.
    async fn run(&self, ctx: &Ctx) -> anyhow::Result<Verdict>;
}

/// Poll `f` every 250ms until it returns `Some(v)` or `timeout` elapses.
pub async fn poll_until<T, F, Fut>(timeout: Duration, mut f: F) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Option<T>>>,
{
    let start = std::time::Instant::now();
    loop {
        if let Some(v) = f().await? {
            return Ok(v);
        }
        if start.elapsed() > timeout {
            anyhow::bail!("poll_until: condition not met within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
