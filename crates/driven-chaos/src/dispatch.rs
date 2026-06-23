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
use crate::reporting::ReportFormat;

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

/// Route a parsed [`Command`]. Returns the process exit code
/// (STRESS_HARNESS s9).
///
/// The Phase-1 interface implements `scenario list` (registry-only) and the
/// capability banner; the bodies that execute scenarios return a
/// harness-error exit code with a clear "not yet implemented (M3.7 Phase-2)"
/// message so a partial harness fails loudly rather than reporting a false
/// green. Phase-2 replaces each placeholder with the real runner.
pub async fn run(command: Command, caps: &CapabilitySet) -> i32 {
    match command {
        Command::ScenarioList => {
            for s in registry::registry() {
                let missing = s.requires().missing(caps);
                let gate = if missing.is_empty() {
                    String::from("runnable")
                } else {
                    format!("requires {}", missing.join(", "))
                };
                println!("{:<40} {:<10} {}", s.name(), gate, s.description());
            }
            exit_code::OK
        }
        other => {
            eprintln!(
                "driven-chaos: `{other:?}` not yet implemented (M3.7 Phase-2 fills the runner)"
            );
            exit_code::HARNESS_ERROR
        }
    }
}
