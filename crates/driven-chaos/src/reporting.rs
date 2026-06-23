//! Per-scenario verdicts and run reports (STRESS_HARNESS s6).
//!
//! [`Verdict`] is the outcome of one scenario; a [`RunReport`] bundles the
//! per-scenario verdicts plus the capability banner. JSON (one line per
//! scenario) and the human summary are both rendered from these types
//! (STRESS_HARNESS s6.2). Phase-2 fills the rendering bodies; the
//! interface fixes the verdict shape the scenario driver produces.

use std::time::Duration;

use crate::scenario::{ExpectedOutcome, Outcome};

/// The outcome of one scenario run (STRESS_HARNESS s6.1).
#[derive(Debug)]
pub enum Verdict {
    /// The scenario met its expected outcome and every cross-cutting
    /// invariant (STRESS_HARNESS s6.3).
    Pass {
        /// Wall-clock the scenario took.
        duration: Duration,
    },
    /// The scenario ran but the observed outcome did not match the
    /// expectation, or a cross-cutting invariant failed.
    Fail {
        /// Wall-clock the scenario took.
        duration: Duration,
        /// What `run_assertions` observed.
        observed_outcome: Outcome,
        /// What the scenario declared it expected.
        expected_outcome: ExpectedOutcome,
        /// Human-readable expected-vs-observed diff for the detail block.
        diff: String,
    },
    /// The scenario's required capabilities were not satisfied; reported
    /// informationally, never red (STRESS_HARNESS s2.5 / s6.2).
    Skipped {
        /// Labels of the capabilities the host did not satisfy.
        missing_capabilities: Vec<String>,
    },
    /// A soak scenario that the harness retried with extended timeouts
    /// before reaching a stable verdict. Reserved for soak rows; non-soak
    /// scenarios never produce `Flaky` (STRESS_HARNESS s6.1).
    Flaky {
        /// How many times the scenario was retried.
        retried: u32,
        /// The eventual stable verdict after retries.
        eventual: Box<Verdict>,
    },
}

impl Verdict {
    /// Whether this verdict counts as a run failure (process exit 1,
    /// STRESS_HARNESS s9). PASS / SKIPPED / a `Flaky` that eventually
    /// passed are all non-failures.
    pub fn is_failure(&self) -> bool {
        match self {
            Verdict::Pass { .. } | Verdict::Skipped { .. } => false,
            Verdict::Fail { .. } => true,
            Verdict::Flaky { eventual, .. } => eventual.is_failure(),
        }
    }
}

/// One scenario's name paired with its verdict.
#[derive(Debug)]
pub struct ScenarioReport {
    /// The scenario's stable kebab-case name.
    pub scenario: &'static str,
    /// The verdict the harness reached.
    pub verdict: Verdict,
}

/// A full run's report: the capability banner plus every scenario's
/// verdict (STRESS_HARNESS s6.2).
#[derive(Debug, Default)]
pub struct RunReport {
    /// Per-scenario reports in execution order.
    pub scenarios: Vec<ScenarioReport>,
}

/// Report output format (STRESS_HARNESS s6.2 / the `report` subcommand).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    /// One JSON object per scenario, newline-delimited.
    Json,
    /// The collapsed human summary.
    Human,
}

impl RunReport {
    /// Whether any scenario failed (drives the process exit code,
    /// STRESS_HARNESS s9).
    pub fn any_failed(&self) -> bool {
        self.scenarios.iter().any(|s| s.verdict.is_failure())
    }
}
