//! The scenario runner the `scenario run` / `run-all` subcommands drive
//! (STRESS_HARNESS s2.2 / s6 / s9).
//!
//! Phase-1 fixed the [`crate::scenario::Scenario`] lifecycle
//! (`setup -> run_assertions -> teardown`, teardown ALWAYS called) and the
//! [`crate::reporting`] verdict shape. This module is the Phase-3 integration
//! glue that actually executes a scenario against a freshly-booted
//! [`DrivenHandle`], enforces the s6.3 "no infinite loop" wall-clock cap, and
//! folds the observed [`Outcome`] against the scenario's
//! [`ExpectedOutcome`] into a [`Verdict`] (s9 PASS semantics).
//!
//! The runner is hermetic by construction: every scenario gets its own
//! tempdir-backed SQLite state and a fixture root under
//! `target/chaos-fixtures/<name>/`. A scenario whose `requires()` is not
//! satisfied by the probed [`CapabilitySet`] is SKIPPED with the exact
//! missing-capability list - capability gaps never turn a run red
//! (STRESS_HARNESS s1.1 / s2.5).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::capabilities::CapabilitySet;
use crate::handle::DrivenHandleBuilder;
use crate::reporting::{RunReport, ScenarioReport, Verdict};
use crate::scenario::{ExpectedOutcome, Outcome, Scenario, ScenarioContext};

/// Hard per-scenario wall-clock cap (STRESS_HARNESS s6.3 "no infinite loop":
/// each scenario has a hard cap; exceeded -> FAIL with `harness.timeout`).
///
/// Generous because the cacheable big-fixture rows (`million-files-nested`,
/// `huge-file-10gb`) materialise real bytes on first run. Deterministic
/// `FakeClock`-driven cycles are fast, so a scenario that hits this cap is a
/// genuine hang, not slowness.
const SCENARIO_WALL_CAP: Duration = Duration::from_secs(300);

/// Where cacheable / throwaway fixtures live (STRESS_HARNESS s2.2):
/// `target/chaos-fixtures/<scenario>/` so `cargo clean` blows them away.
fn fixture_root_for(name: &str) -> PathBuf {
    PathBuf::from("target/chaos-fixtures").join(name)
}

/// Remove a fixture dir, retrying a few times to absorb a Windows
/// handle-release lag (a just-exited lock-holder thread can keep the OS file
/// handle held for a moment, making the first `remove_dir_all` fail with
/// ERROR_SHARING_VIOLATION). Best-effort: a persistent failure is logged but
/// never aborts the run.
fn remove_dir_all_with_retry(dir: &std::path::Path) {
    for attempt in 0..5u32 {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) if attempt < 4 => {
                std::thread::sleep(Duration::from_millis(20 * (attempt as u64 + 1)));
            }
            Err(e) => {
                tracing::warn!(dir = %dir.display(), %e, "could not remove fixture dir after retries");
                return;
            }
        }
    }
}

/// Run one scenario end to end and produce its [`Verdict`].
///
/// Lifecycle (STRESS_HARNESS s2.3): capability-gate -> `setup` -> boot a
/// hermetic [`crate::handle::DrivenHandle`] -> `run_assertions` -> ALWAYS
/// `teardown` -> compare observed vs expected. The whole run is wrapped in a
/// [`SCENARIO_WALL_CAP`] timeout (s6.3 no-infinite-loop).
pub async fn run_one(scenario: &dyn Scenario, caps: &CapabilitySet) -> Verdict {
    let missing = scenario.requires().missing(caps);
    if !missing.is_empty() {
        return Verdict::Skipped {
            missing_capabilities: missing,
        };
    }

    let started = Instant::now();
    match tokio::time::timeout(SCENARIO_WALL_CAP, execute(scenario)).await {
        Ok(Ok(outcome)) => verdict_for(scenario, outcome, started.elapsed()),
        Ok(Err(e)) => Verdict::Fail {
            duration: started.elapsed(),
            observed_outcome: Outcome {
                notes: vec![format!("scenario errored: {e:#}")],
                ..Outcome::default()
            },
            expected_outcome: scenario.expected_outcome(),
            diff: format!("scenario run errored before producing an outcome: {e:#}"),
        },
        Err(_elapsed) => Verdict::Fail {
            duration: started.elapsed(),
            observed_outcome: Outcome {
                notes: vec![format!(
                    "harness.timeout: scenario exceeded the {}s wall-clock cap (s6.3 no-infinite-loop)",
                    SCENARIO_WALL_CAP.as_secs()
                )],
                ..Outcome::default()
            },
            expected_outcome: scenario.expected_outcome(),
            diff: format!(
                "harness.timeout: exceeded {}s cap",
                SCENARIO_WALL_CAP.as_secs()
            ),
        },
    }
}

/// Drive `setup -> run_assertions -> teardown`, guaranteeing teardown runs
/// even when an assertion fails (STRESS_HARNESS s2.3). Returns the observed
/// [`Outcome`], or the first hard error encountered.
async fn execute(scenario: &dyn Scenario) -> anyhow::Result<Outcome> {
    let fixture_root = fixture_root_for(scenario.name());
    std::fs::create_dir_all(&fixture_root)?;

    // Hermetic-state guard: a scenario's SQLite state DB must be fresh every
    // run, but some rows (the file-size category) place `state.db` INSIDE the
    // fixture root that a cacheable run preserves. On Windows a prior run's
    // teardown `remove_dir_all` can silently fail while the WAL/SHM handles are
    // still settling, leaving a STALE `state.db` whose `synced` rows reference
    // the PREVIOUS run's (now-gone) fake-remote file ids - which then surfaces
    // as a flaky "no object with file_id" on the next run. Proactively remove
    // any stale state-DB family here (it is never cacheable fixture CONTENT),
    // while leaving the cached source tree intact.
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(fixture_root.join(format!("state.db{suffix}")));
    }

    let mut ctx = ScenarioContext {
        fixture_root: fixture_root.clone(),
        cacheable: false,
    };

    scenario.setup(&mut ctx).await?;

    // A hermetic handle for scenarios that use the provided one. Most rows
    // boot their own custom-remote handle internally; this generic handle is
    // the default the trait requires (Phase-1 `_handle` contract).
    let state_dir = tempfile::tempdir()?;
    let handle = DrivenHandleBuilder::new(state_dir.path().join("state.db"))
        .boot()
        .await?;

    let result = scenario.run_assertions(&handle).await;

    // teardown ALWAYS runs (STRESS_HARNESS s2.3), even on assertion failure.
    let teardown = scenario.teardown(&mut ctx).await;

    // A throwaway fixture is cleaned; a cacheable one survives for the next
    // run (STRESS_HARNESS s8 big-fixture caching). On Windows a just-joined
    // lock-holder thread (the HoldLocked / LockUnlock soak rows) can leave the
    // OS file handle briefly held AFTER the thread exits, so a single
    // `remove_dir_all` can race it with ERROR_SHARING_VIOLATION (os error 32).
    // Retry a few times with a short backoff so a transient handle-release lag
    // does not strand a locked fixture that fails the NEXT run's setup write.
    if !ctx.cacheable {
        remove_dir_all_with_retry(&fixture_root);
    }

    let outcome = result?;
    teardown?;
    Ok(outcome)
}

/// Fold an observed [`Outcome`] against the scenario's declared
/// [`ExpectedOutcome`] into a PASS / FAIL [`Verdict`] (STRESS_HARNESS s9).
fn verdict_for(scenario: &dyn Scenario, observed: Outcome, duration: Duration) -> Verdict {
    let expected = scenario.expected_outcome();
    let passed = match &expected {
        // Success: the scenario completed with no surfaced error code.
        ExpectedOutcome::Success => observed.error_codes_seen.is_empty(),
        // Graceful failure: Driven surfaced exactly this stable code at least
        // once and did not crash (a crash would have errored `run_assertions`
        // and never reached here). s9: assert the stable code, not text.
        ExpectedOutcome::GracefulFailureWith { code } => observed.error_codes_seen.contains(code),
        // Documented behaviour: the scenario's own `run_assertions` carries
        // the snapshot-diff check; reaching here (Ok outcome) means it held.
        ExpectedOutcome::DocumentedBehaviour => true,
    };

    if passed {
        Verdict::Pass { duration }
    } else {
        let diff = match &expected {
            ExpectedOutcome::Success => format!(
                "expected Success (no error code) but observed codes: {:?}",
                observed.error_codes_seen
            ),
            ExpectedOutcome::GracefulFailureWith { code } => format!(
                "expected graceful failure with {} but observed codes: {:?}",
                code.code(),
                observed.error_codes_seen
            ),
            ExpectedOutcome::DocumentedBehaviour => {
                "documented-behaviour check did not hold".to_string()
            }
        };
        Verdict::Fail {
            duration,
            observed_outcome: observed,
            expected_outcome: expected,
            diff,
        }
    }
}

/// Run every scenario in `scenarios`, respecting capability gates, into a
/// [`RunReport`] (STRESS_HARNESS s6.2). The caller maps the report to a
/// process exit code via [`RunReport::any_failed`] (s9).
pub async fn run_all(scenarios: Vec<Box<dyn Scenario>>, caps: &CapabilitySet) -> RunReport {
    let mut report = RunReport::default();
    for scenario in scenarios {
        let verdict = run_one(scenario.as_ref(), caps).await;
        report.scenarios.push(ScenarioReport {
            scenario: scenario.name(),
            verdict,
        });
    }
    report
}
