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

    /// Counts of `(pass, skip, fail, flaky, total)` for the human banner.
    fn tallies(&self) -> (usize, usize, usize, usize) {
        let mut pass = 0;
        let mut skip = 0;
        let mut fail = 0;
        let mut flaky = 0;
        for s in &self.scenarios {
            match &s.verdict {
                Verdict::Pass { .. } => pass += 1,
                Verdict::Skipped { .. } => skip += 1,
                Verdict::Fail { .. } => fail += 1,
                Verdict::Flaky { eventual, .. } => {
                    flaky += 1;
                    if eventual.is_failure() {
                        fail += 1;
                    } else {
                        pass += 1;
                    }
                }
            }
        }
        (pass, skip, fail, flaky)
    }

    /// Render the report in the requested format (STRESS_HARNESS s6.2).
    pub fn render(&self, format: ReportFormat) -> String {
        match format {
            ReportFormat::Json => self.render_json(),
            ReportFormat::Human => self.render_human(),
        }
    }

    /// JSON, one object per scenario, newline-delimited (STRESS_HARNESS s6.2).
    pub fn render_json(&self) -> String {
        let mut out = String::new();
        for s in &self.scenarios {
            out.push_str(&serde_json::to_string(&s.to_json()).unwrap_or_else(|e| {
                format!(
                    "{{\"scenario\":\"{}\",\"render_error\":\"{e}\"}}",
                    s.scenario
                )
            }));
            out.push('\n');
        }
        out
    }

    /// The collapsed human summary (STRESS_HARNESS s6.2): a count banner, the
    /// SKIPPED list with reasons, then a detail block per FAIL.
    pub fn render_human(&self) -> String {
        let (pass, skip, fail, flaky) = self.tallies();
        let total = self.scenarios.len();
        let mut out = String::new();
        out.push_str(&format!(
            "\n {pass} PASS    {skip} SKIP    {fail} FAIL    {flaky} FLAKY    of {total} total\n"
        ));

        let skipped: Vec<&ScenarioReport> = self
            .scenarios
            .iter()
            .filter(|s| matches!(s.verdict, Verdict::Skipped { .. }))
            .collect();
        if !skipped.is_empty() {
            out.push_str(&format!("\n SKIPPED ({}):\n", skipped.len()));
            for s in skipped {
                if let Verdict::Skipped {
                    missing_capabilities,
                } = &s.verdict
                {
                    out.push_str(&format!(
                        "   {:<34} (missing: {})\n",
                        s.scenario,
                        missing_capabilities.join(", ")
                    ));
                }
            }
        }

        let failed: Vec<&ScenarioReport> = self
            .scenarios
            .iter()
            .filter(|s| s.verdict.is_failure())
            .collect();
        if !failed.is_empty() {
            out.push_str(&format!("\n FAILED ({}):\n", failed.len()));
            for s in failed {
                if let Verdict::Fail { diff, .. } = unwrap_eventual(&s.verdict) {
                    out.push_str(&format!("   {}\n     {diff}\n", s.scenario));
                }
            }
        }
        out
    }
}

/// Peel a `Flaky` wrapper down to its eventual verdict for rendering.
fn unwrap_eventual(v: &Verdict) -> &Verdict {
    match v {
        Verdict::Flaky { eventual, .. } => unwrap_eventual(eventual),
        other => other,
    }
}

/// A render-friendly, serializable view of one scenario's verdict
/// (STRESS_HARNESS s6.2). The Phase-1 [`Verdict`] / [`Outcome`] types stay
/// non-`Serialize` (the canonical surface); this is the JSON projection.
#[derive(serde::Serialize)]
struct ScenarioJson {
    scenario: &'static str,
    verdict: &'static str,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    missing_capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed: Option<ObservedJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<String>,
}

#[derive(serde::Serialize)]
struct ObservedJson {
    error_codes_seen: Vec<String>,
    final_drive_object_count: u64,
    final_hash_matches_local: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
}

impl ScenarioReport {
    fn to_json(&self) -> ScenarioJson {
        let observed_json = |o: &Outcome| ObservedJson {
            error_codes_seen: o
                .error_codes_seen
                .iter()
                .map(|c| c.code().to_string())
                .collect(),
            final_drive_object_count: o.final_drive_object_count,
            final_hash_matches_local: o.final_hash_matches_local,
            notes: o.notes.clone(),
        };
        match unwrap_eventual(&self.verdict) {
            Verdict::Pass { duration } => ScenarioJson {
                scenario: self.scenario,
                verdict: "pass",
                duration_ms: duration.as_millis() as u64,
                missing_capabilities: vec![],
                observed: None,
                diff: None,
            },
            Verdict::Skipped {
                missing_capabilities,
            } => ScenarioJson {
                scenario: self.scenario,
                verdict: "skipped",
                duration_ms: 0,
                missing_capabilities: missing_capabilities.clone(),
                observed: None,
                diff: None,
            },
            Verdict::Fail {
                duration,
                observed_outcome,
                diff,
                ..
            } => ScenarioJson {
                scenario: self.scenario,
                verdict: "fail",
                duration_ms: duration.as_millis() as u64,
                missing_capabilities: vec![],
                observed: Some(observed_json(observed_outcome)),
                diff: Some(diff.clone()),
            },
            // unwrap_eventual guarantees we never see Flaky here.
            Verdict::Flaky { .. } => ScenarioJson {
                scenario: self.scenario,
                verdict: "flaky",
                duration_ms: 0,
                missing_capabilities: vec![],
                observed: None,
                diff: None,
            },
        }
    }
}
