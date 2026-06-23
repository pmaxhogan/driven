//! Subcommand parsing + routing skeleton (STRESS_HARNESS s2.2).
//!
//! The CLI surface is:
//!
//! ```text
//! driven-chaos fixture create <scenario>
//! driven-chaos fixture clean <scenario> | --all
//! driven-chaos scenario list
//! driven-chaos scenario run <scenario>
//! driven-chaos scenario run-all
//! driven-chaos fuzz [--seed N --duration 1h]
//! driven-chaos mutator <fs|drive> --while-syncing --scenario <name>
//! driven-chaos report [--format json|human]
//! ```
//!
//! [`parse`] turns argv into a [`Command`]; [`run`] routes it. The Phase-1
//! interface implements the parse + the `scenario list` body (which only
//! needs the registry) and leaves the run-bodies that need scenario
//! execution as explicit `not-yet-implemented` errors for the Phase-2
//! agents to fill. Exit-code mapping follows STRESS_HARNESS s9:
//! 0 = all pass/skip, 1 = any fail, 2 = harness self-error.

use crate::capabilities::CapabilitySet;
use crate::registry;
use crate::reporting::{ReportFormat, RunReport};
use crate::runner;
use crate::scenarios::mutator as mutator_scenarios;

/// The parsed CLI command (STRESS_HARNESS s2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `fixture create <scenario>` - materialise and leave a fixture.
    FixtureCreate {
        /// Scenario whose fixture to build.
        scenario: String,
    },
    /// `fixture clean <scenario>` - tear one fixture down.
    FixtureClean {
        /// Scenario whose fixture to remove.
        scenario: String,
    },
    /// `fixture clean --all` - tear down every persistent fixture.
    FixtureCleanAll,
    /// `scenario list` - print every scenario + its requirements.
    ScenarioList,
    /// `scenario run <scenario>` - run one scenario end to end.
    ScenarioRun {
        /// Scenario to run.
        scenario: String,
    },
    /// `scenario run-all` - run every scenario, respecting capability gates.
    ScenarioRunAll,
    /// `fuzz [--seed N --duration D]` - property-style soak run.
    Fuzz {
        /// Seed for the weighted mutation distribution; `None` = `now()`.
        seed: Option<u64>,
        /// Soak duration in seconds; `None` = the default.
        duration_secs: Option<u64>,
    },
    /// `mutator <fs|drive> --while-syncing --scenario <name>`.
    Mutator {
        /// Which mutator flavour (`fs` or `drive`).
        flavour: MutatorFlavour,
        /// Scenario the mutator runs against.
        scenario: String,
    },
    /// `report [--format json|human]` - re-print the last run's report.
    Report {
        /// Output format.
        format: ReportFormat,
    },
}

/// The `mutator` subcommand's flavour argument (STRESS_HARNESS s2.2 / s4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutatorFlavour {
    /// Filesystem mutator (STRESS_HARNESS s4.1).
    Fs,
    /// Drive-side mutator (STRESS_HARNESS s4.2).
    Drive,
}

/// Parse argv (excluding the binary name) into a [`Command`].
///
/// Returns an error string suitable for stderr + exit code 2 on a malformed
/// invocation (STRESS_HARNESS s9 "harness itself errored").
pub fn parse(args: &[String]) -> anyhow::Result<Command> {
    let mut it = args.iter().map(String::as_str);
    match it.next() {
        Some("fixture") => match it.next() {
            Some("create") => {
                let scenario = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("fixture create needs a <scenario>"))?;
                Ok(Command::FixtureCreate {
                    scenario: scenario.to_string(),
                })
            }
            Some("clean") => match it.next() {
                Some("--all") => Ok(Command::FixtureCleanAll),
                Some(scenario) => Ok(Command::FixtureClean {
                    scenario: scenario.to_string(),
                }),
                None => anyhow::bail!("fixture clean needs a <scenario> or --all"),
            },
            other => anyhow::bail!("unknown fixture subcommand: {other:?}"),
        },
        Some("scenario") => match it.next() {
            Some("list") => Ok(Command::ScenarioList),
            Some("run") => {
                let scenario = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("scenario run needs a <scenario>"))?;
                Ok(Command::ScenarioRun {
                    scenario: scenario.to_string(),
                })
            }
            Some("run-all") => Ok(Command::ScenarioRunAll),
            other => anyhow::bail!("unknown scenario subcommand: {other:?}"),
        },
        // Top-level aliases for the `scenario` subcommands. The harness is
        // hermetic by construction (each scenario boots its own tempdir-backed
        // state + fake remote), so `--hermetic` is accepted and is a no-op -
        // it documents intent and matches the CI / smoke invocation
        // `driven-chaos run-all --hermetic`.
        Some("list") => Ok(Command::ScenarioList),
        Some("run") => {
            let scenario = it
                .next()
                .ok_or_else(|| anyhow::anyhow!("run needs a <scenario>"))?;
            Ok(Command::ScenarioRun {
                scenario: scenario.to_string(),
            })
        }
        Some("run-all") => {
            // Accept and ignore a trailing `--hermetic` (the only mode there is).
            for flag in it {
                match flag {
                    "--hermetic" => {}
                    other => anyhow::bail!("unknown run-all flag: {other}"),
                }
            }
            Ok(Command::ScenarioRunAll)
        }
        Some("fuzz") => {
            let mut seed = None;
            let mut duration_secs = None;
            while let Some(flag) = it.next() {
                match flag {
                    "--seed" => {
                        seed = Some(
                            it.next()
                                .ok_or_else(|| anyhow::anyhow!("--seed needs a value"))?
                                .parse()?,
                        );
                    }
                    "--duration" => {
                        duration_secs =
                            Some(parse_duration_secs(it.next().ok_or_else(|| {
                                anyhow::anyhow!("--duration needs a value")
                            })?)?);
                    }
                    other => anyhow::bail!("unknown fuzz flag: {other}"),
                }
            }
            Ok(Command::Fuzz {
                seed,
                duration_secs,
            })
        }
        Some("mutator") => {
            let flavour = match it.next() {
                Some("fs") => MutatorFlavour::Fs,
                Some("drive") => MutatorFlavour::Drive,
                other => anyhow::bail!("mutator needs `fs` or `drive`, got {other:?}"),
            };
            let mut scenario = None;
            while let Some(flag) = it.next() {
                match flag {
                    "--while-syncing" => {}
                    "--scenario" => {
                        scenario = Some(
                            it.next()
                                .ok_or_else(|| anyhow::anyhow!("--scenario needs a value"))?
                                .to_string(),
                        );
                    }
                    other => anyhow::bail!("unknown mutator flag: {other}"),
                }
            }
            Ok(Command::Mutator {
                flavour,
                scenario: scenario
                    .ok_or_else(|| anyhow::anyhow!("mutator needs --scenario <name>"))?,
            })
        }
        Some("report") => {
            let mut format = ReportFormat::Human;
            while let Some(flag) = it.next() {
                match flag {
                    "--format" => {
                        format = match it.next() {
                            Some("json") => ReportFormat::Json,
                            Some("human") => ReportFormat::Human,
                            other => anyhow::bail!("--format needs json|human, got {other:?}"),
                        };
                    }
                    other => anyhow::bail!("unknown report flag: {other}"),
                }
            }
            Ok(Command::Report { format })
        }
        other => anyhow::bail!("unknown subcommand: {other:?}"),
    }
}

/// Parse a duration like `1h`, `30m`, `45s` into whole seconds.
fn parse_duration_secs(s: &str) -> anyhow::Result<u64> {
    let (num, mult) = if let Some(stripped) = s.strip_suffix('h') {
        (stripped, 3600)
    } else if let Some(stripped) = s.strip_suffix('m') {
        (stripped, 60)
    } else if let Some(stripped) = s.strip_suffix('s') {
        (stripped, 1)
    } else {
        (s, 1)
    };
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration: {s}"))?;
    Ok(n.saturating_mul(mult))
}

/// Process exit codes (STRESS_HARNESS s9).
pub mod exit_code {
    /// Every scenario passed or skipped.
    pub const OK: i32 = 0;
    /// One or more scenarios failed.
    pub const FAIL: i32 = 1;
    /// The harness itself errored.
    pub const HARNESS_ERROR: i32 = 2;
}

/// Where the most-recent run's report is persisted so the `report`
/// subcommand can re-print it (STRESS_HARNESS s2.2 / s6.2).
const LAST_RUN_JSON: &str = "target/chaos-runs/last-run.json";

/// A default fuzz step budget for a CLI `fuzz` invocation with no
/// `--duration`. The weekly soak (s7) drives a much larger budget via the
/// `--duration` -> step-budget mapping below.
const DEFAULT_FUZZ_STEPS: u64 = 200;

/// Map a `--duration` in seconds onto a fuzz step budget. Each second of
/// requested soak buys a fixed number of mutation steps; the run's wall-clock
/// cap inside `run_fuzz` still bounds an over-long request.
fn steps_for_duration(secs: u64) -> u64 {
    secs.saturating_mul(50).max(DEFAULT_FUZZ_STEPS)
}

/// Persist a finished run so `report` can re-print it. Best-effort: a write
/// failure must not flip an otherwise-green run red, so it only warns.
fn persist_last_run(report: &RunReport) {
    let json = report.render_json();
    if let Some(parent) = std::path::Path::new(LAST_RUN_JSON).parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("could not create chaos-runs dir: {e}");
            return;
        }
    }
    if let Err(e) = std::fs::write(LAST_RUN_JSON, json) {
        tracing::warn!("could not persist last-run report: {e}");
    }
}

/// Route a parsed [`Command`]. Returns the process exit code
/// (STRESS_HARNESS s9): 0 = all pass/skip, 1 = any fail, 2 = harness
/// self-error.
pub async fn run(command: Command, caps: &CapabilitySet) -> i32 {
    match command {
        Command::ScenarioList => {
            for s in registry::registry() {
                let reqs = s.requires();
                // The DECLARED requires() set - what STRESS_HARNESS acceptance
                // asks of the host, independent of THIS host (P2-H). Rendered
                // from the capability labels so the column lines up with the
                // s3 catalogue's privilege column.
                let declared: Vec<String> = reqs.required.iter().map(|c| c.label()).collect();
                let requires = if declared.is_empty() {
                    String::from("-")
                } else {
                    declared.join(",")
                };
                // Whether THIS host can run it (a separate column): runnable, or
                // the subset of declared caps the host is missing.
                let missing = reqs.missing(caps);
                let host = if missing.is_empty() {
                    String::from("runnable")
                } else {
                    format!("missing {}", missing.join(","))
                };
                println!(
                    "{:<40} {:<32} {:<22} {}",
                    s.name(),
                    requires,
                    host,
                    s.description()
                );
            }
            exit_code::OK
        }
        Command::ScenarioRun { scenario } => {
            let Some(s) = registry::find(&scenario) else {
                eprintln!("driven-chaos: unknown scenario {scenario:?}");
                return exit_code::HARNESS_ERROR;
            };
            let verdict = runner::run_one(s.as_ref(), caps).await;
            let mut report = RunReport::default();
            report.scenarios.push(crate::reporting::ScenarioReport {
                scenario: s.name(),
                verdict,
            });
            print!("{}", report.render_json());
            print!("{}", report.render_human());
            persist_last_run(&report);
            if report.any_failed() {
                exit_code::FAIL
            } else {
                exit_code::OK
            }
        }
        Command::ScenarioRunAll => {
            let report = runner::run_all(registry::registry(), caps).await;
            print!("{}", report.render_json());
            print!("{}", report.render_human());
            persist_last_run(&report);
            if report.any_failed() {
                exit_code::FAIL
            } else {
                exit_code::OK
            }
        }
        Command::Fuzz {
            seed,
            duration_secs,
        } => {
            let seed = seed.unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            });
            let steps = duration_secs
                .map(steps_for_duration)
                .unwrap_or(DEFAULT_FUZZ_STEPS);
            println!("driven-chaos fuzz: seed={seed} steps={steps}");
            match mutator_scenarios::run_fuzz(seed, steps).await {
                Ok(report) => {
                    if let Some(violation) = &report.violation {
                        match mutator_scenarios::write_fuzz_failure(&report) {
                            Ok(path) => eprintln!(
                                "FUZZ FAIL seed={seed}: {violation} (replay log: {})",
                                path.display()
                            ),
                            Err(e) => eprintln!(
                                "FUZZ FAIL seed={seed}: {violation} (could not write replay log: {e})"
                            ),
                        }
                        exit_code::FAIL
                    } else {
                        println!("FUZZ PASS seed={seed} steps={}", report.steps);
                        exit_code::OK
                    }
                }
                Err(e) => {
                    eprintln!("driven-chaos: fuzz harness error: {e:#}");
                    exit_code::HARNESS_ERROR
                }
            }
        }
        Command::Mutator { flavour, scenario } => {
            let result = match flavour {
                MutatorFlavour::Fs => mutator_scenarios::run_fs_mutator(&scenario).await,
                MutatorFlavour::Drive => mutator_scenarios::run_drive_mutator(&scenario).await,
            };
            match result {
                Ok(outcome) => {
                    println!(
                        "mutator {scenario}: {} object(s), hash_ok={}, notes={:?}",
                        outcome.final_drive_object_count,
                        outcome.final_hash_matches_local,
                        outcome.notes
                    );
                    exit_code::OK
                }
                Err(e) => {
                    eprintln!("driven-chaos: mutator error: {e:#}");
                    exit_code::HARNESS_ERROR
                }
            }
        }
        Command::Report { format } => match std::fs::read_to_string(LAST_RUN_JSON) {
            Ok(contents) => {
                // The persisted form is already the JSON projection. For
                // `--format json` echo it; for `human` we cannot re-derive the
                // full human block from JSON alone, so point at the run.
                match format {
                    ReportFormat::Json => print!("{contents}"),
                    ReportFormat::Human => {
                        println!("last run report (JSON projection):\n{contents}");
                    }
                }
                exit_code::OK
            }
            Err(e) => {
                eprintln!(
                    "driven-chaos: no persisted run at {LAST_RUN_JSON} ({e}); run `run-all` first"
                );
                exit_code::HARNESS_ERROR
            }
        },
        Command::FixtureCreate { scenario } => {
            let Some(s) = registry::find(&scenario) else {
                eprintln!("driven-chaos: unknown scenario {scenario:?}");
                return exit_code::HARNESS_ERROR;
            };
            let root = std::path::PathBuf::from("target/chaos-fixtures").join(s.name());
            if let Err(e) = std::fs::create_dir_all(&root) {
                eprintln!("driven-chaos: could not create fixture root: {e}");
                return exit_code::HARNESS_ERROR;
            }
            let mut ctx = crate::scenario::ScenarioContext {
                fixture_root: root.clone(),
                cacheable: false,
            };
            match s.setup(&mut ctx).await {
                Ok(()) => {
                    println!("fixture for {scenario} materialised at {}", root.display());
                    exit_code::OK
                }
                Err(e) => {
                    eprintln!("driven-chaos: fixture create failed: {e:#}");
                    exit_code::HARNESS_ERROR
                }
            }
        }
        Command::FixtureClean { scenario } => {
            let root = std::path::PathBuf::from("target/chaos-fixtures").join(&scenario);
            match std::fs::remove_dir_all(&root) {
                Ok(()) => {
                    println!("removed fixture {}", root.display());
                    exit_code::OK
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("no fixture at {} (already clean)", root.display());
                    exit_code::OK
                }
                Err(e) => {
                    eprintln!("driven-chaos: fixture clean failed: {e}");
                    exit_code::HARNESS_ERROR
                }
            }
        }
        Command::FixtureCleanAll => {
            let root = std::path::PathBuf::from("target/chaos-fixtures");
            match std::fs::remove_dir_all(&root) {
                Ok(()) => {
                    println!("removed all fixtures under {}", root.display());
                    exit_code::OK
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    println!("no fixtures to clean");
                    exit_code::OK
                }
                Err(e) => {
                    eprintln!("driven-chaos: fixture clean --all failed: {e}");
                    exit_code::HARNESS_ERROR
                }
            }
        }
    }
}
