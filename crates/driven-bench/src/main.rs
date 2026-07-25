//! `driven-bench` - the real-world benchmark suite (bench/README.md).
//!
//! Compares Driven's actual backup engine against `rclone` on the two shapes
//! that dominate real backup sets - a few very large files, and very many very
//! small ones in a deep tree - across a cold upload and an incremental re-run
//! after a small change.
//!
//! It is deliberately NOT part of any normal build or test path: it uploads real
//! bytes to a real Google account and costs real time. It runs on demand
//! (`just bench`, `bench/run.ps1`) or from the tag-gated `bench.yml` workflow.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod agent;
mod counting_store;
mod creds;
mod fixture;
mod procstat;
mod report;
mod tools;

use crate::fixture::{human_bytes, Fixture, FixtureSpec, Shape};
use crate::report::{RunReport, ScenarioReport};
use crate::tools::{Phase, PhaseResult, Tool};

/// Default ceiling on the bytes one invocation may upload, summed over every
/// tool and fixture. A benchmark that quietly pushes tens of gigabytes to a
/// personal Drive is a bad benchmark; `--full` lifts it deliberately.
const DEFAULT_MAX_UPLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Fraction of the tree rewritten before the incremental phase.
const MUTATION_FRACTION: f64 = 0.001;

#[derive(Debug, Parser)]
#[command(
    name = "driven-bench",
    version,
    about = "Driven vs rclone benchmark suite"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the benchmark matrix and write a report.
    Run(RunArgs),
    /// Build, mutate or delete fixture trees without benchmarking anything.
    #[command(subcommand)]
    Fixture(FixtureCommand),
    /// Internal: run ONE Driven backup cycle and print its metrics.
    ///
    /// The harness re-invokes itself with this so the engine is measured as a
    /// child process, exactly like rclone is.
    #[command(hide = true)]
    AgentSync(agent::AgentArgs),
}

/// The fixture sizes a run can be invoked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Scale {
    /// Minutes, a few hundred megabytes. For proving the pipeline works.
    Smoke,
    /// The default: enough data for the numbers to mean something.
    Small,
    /// A serious local run.
    Medium,
    /// The shapes the suite is really about: multi-gigabyte files and a
    /// million small ones. Needs `--full` to clear the upload cap.
    Full,
}

impl Scale {
    fn slug(self) -> &'static str {
        match self {
            Scale::Smoke => "smoke",
            Scale::Small => "small",
            Scale::Medium => "medium",
            Scale::Full => "full",
        }
    }

    /// The fixtures this scale runs, in report order.
    fn specs(self, seed: u64) -> Vec<FixtureSpec> {
        let mib = 1024 * 1024;
        let (huge_files, huge_bytes, tiny_files, depth) = match self {
            Scale::Smoke => (2, 8 * mib, 300, 5),
            Scale::Small => (4, 128 * mib, 50_000, 8),
            Scale::Medium => (4, 512 * mib, 200_000, 8),
            Scale::Full => (4, 2048 * mib, 1_000_000, 8),
        };
        vec![
            FixtureSpec {
                shape: Shape::Huge,
                files: huge_files,
                huge_file_bytes: huge_bytes,
                depth: 0,
                seed,
            },
            FixtureSpec {
                shape: Shape::TinyDeep,
                files: tiny_files,
                huge_file_bytes: 0,
                depth,
                seed,
            },
        ]
    }
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// Fixture size.
    #[arg(long, value_enum, default_value = "small")]
    scale: Scale,
    /// Which tools to measure.
    #[arg(long, value_delimiter = ',', default_values = ["driven", "rclone"])]
    tools: Vec<Tool>,
    /// Only run one fixture shape instead of both.
    #[arg(long, value_enum)]
    shape: Option<Shape>,
    /// The destination Drive folder id. Required, by flag or by
    /// `DRIVEN_E2E_DEST_FOLDER_ID` - there is no default.
    #[arg(long)]
    dest: Option<String>,
    /// Fixture PRNG seed. The same seed always produces the same trees.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Lift the default upload cap (needed for `--scale full`).
    #[arg(long)]
    full: bool,
    /// Override the upload cap, in bytes.
    #[arg(long)]
    max_upload_bytes: Option<u64>,
    /// Path to the rclone binary. Defaults to `rclone` on `PATH`.
    #[arg(long)]
    rclone: Option<PathBuf>,
    /// rclone's parallel transfer count. Defaults to rclone's own default (4),
    /// i.e. each tool runs at its stock settings; pass Driven's pool size here
    /// to compare the algorithms at equal concurrency instead.
    #[arg(long, default_value_t = 4)]
    rclone_transfers: u64,
    /// Where to cache generated fixtures.
    #[arg(long)]
    fixture_root: Option<PathBuf>,
    /// Where to write the report.
    #[arg(long)]
    results_dir: Option<PathBuf>,
    /// Leave the uploaded run folder in Drive instead of trashing it.
    #[arg(long)]
    keep_remote: bool,
}

#[derive(Debug, Subcommand)]
enum FixtureCommand {
    /// Materialise a fixture tree without uploading anything.
    Build {
        #[arg(long, value_enum)]
        shape: Shape,
        #[arg(long, value_enum, default_value = "small")]
        scale: Scale,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long)]
        fixture_root: Option<PathBuf>,
    },
    /// Delete every cached fixture.
    Clean {
        #[arg(long)]
        fixture_root: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    // A gitignored .env.test at the repo root is the local credential source;
    // in CI the same names arrive as secrets and always win.
    let _ = creds::load_dotenv(&repo_root().join(".env.test"));

    match Cli::parse().command {
        Command::Run(args) => run(args).await,
        Command::Fixture(cmd) => run_fixture(cmd),
        Command::AgentSync(args) => agent::run(args).await,
    }
}

/// The repo root, derived from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Removes repeated tools while preserving the order they were given in.
fn dedupe_tools(tools: &[Tool]) -> Vec<Tool> {
    let mut unique: Vec<Tool> = Vec::with_capacity(tools.len());
    for &tool in tools {
        if !unique.contains(&tool) {
            unique.push(tool);
        }
    }
    unique
}

fn fixture_root(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| repo_root().join("target").join("bench-fixtures"))
}

fn run_fixture(cmd: FixtureCommand) -> Result<()> {
    match cmd {
        FixtureCommand::Build {
            shape,
            scale,
            seed,
            fixture_root: root,
        } => {
            let root = fixture_root(root);
            let spec = scale
                .specs(seed)
                .into_iter()
                .find(|s| s.shape == shape)
                .expect("every scale defines both shapes");
            let fixture = Fixture::build(&root, &spec)?;
            println!("{}", fixture.tree().display());
            Ok(())
        }
        FixtureCommand::Clean { fixture_root: root } => {
            let root = fixture_root(root);
            fixture::clean(&root)?;
            println!("removed {}", root.display());
            Ok(())
        }
    }
}

async fn run(mut args: RunArgs) -> Result<()> {
    if args.tools.is_empty() {
        anyhow::bail!("--tools must name at least one tool");
    }
    // A repeated tool would reuse the same scenario folder AND the same state
    // database, so its second "cold" phase would silently be an incremental one
    // reported as cold. Collapse duplicates rather than measure a lie.
    args.tools = dedupe_tools(&args.tools);

    // --- preconditions, all checked before a single byte is generated -------
    let dest_folder_id = creds::resolve_dest_folder_id(args.dest.as_deref())?;
    let creds = creds::BenchCreds::from_env()?;

    let specs: Vec<FixtureSpec> = args
        .scale
        .specs(args.seed)
        .into_iter()
        .filter(|s| args.shape.is_none_or(|shape| shape == s.shape))
        .collect();

    let cap = args.max_upload_bytes.unwrap_or(DEFAULT_MAX_UPLOAD_BYTES);
    let planned: u64 = specs.iter().map(|s| s.total_bytes()).sum::<u64>() * args.tools.len() as u64;
    if !args.full && planned > cap {
        anyhow::bail!(
            "this run would upload {} ({} per tool x {} tool(s)), over the {} cap.\n\
             Pass --full to lift the cap, --max-upload-bytes to raise it, or use a smaller --scale.",
            human_bytes(planned),
            human_bytes(planned / args.tools.len() as u64),
            args.tools.len(),
            human_bytes(cap)
        );
    }

    let rclone_binary = if args.tools.contains(&Tool::Rclone) {
        let found = tools::find_rclone(args.rclone.as_deref()).context(
            "rclone was requested but no rclone binary was found: install it, put it on PATH, \
             or pass --rclone <path> (see bench/README.md)",
        )?;
        Some(found)
    } else {
        None
    };

    // --- one run folder, created up front, trashed at the end ---------------
    let store: Arc<dyn driven_drive::remote_store::RemoteStore> = Arc::new(creds.build_store()?);
    let run_name = format!("driven-bench-{}", uuid::Uuid::new_v4());
    let run_folder = creds::RunFolder::create(store, &dest_folder_id, run_name.clone())
        .await
        .context("creating the run folder - check the destination folder id and the credentials")?;
    println!("run folder: {run_name} ({})", run_folder.id);
    println!(
        "plan: {} fixture(s) x {} tool(s), up to {} uploaded",
        specs.len(),
        args.tools.len(),
        human_bytes(planned)
    );

    // Everything after this point must reach the cleanup below, so the body's
    // error is captured rather than returned.
    let outcome = run_scenarios(&args, &specs, &creds, &run_folder, rclone_binary.as_deref()).await;

    if args.keep_remote {
        println!(
            "--keep-remote: leaving {run_name} ({}) in Drive",
            run_folder.id
        );
    } else if let Err(err) = run_folder.cleanup().await {
        eprintln!("WARNING: failed to trash the run folder {run_name}: {err:#}");
        eprintln!("         trash it by hand: folder id {}", run_folder.id);
    } else {
        println!("cleaned up run folder {run_name}");
    }

    let scenarios = outcome?;

    let report = RunReport {
        started_at: report::utc_timestamp(),
        scale: args.scale.slug().to_string(),
        seed: args.seed,
        host: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        cpus: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        driven_version: env!("CARGO_PKG_VERSION").to_string(),
        rclone_version: rclone_binary.as_deref().and_then(tools::rclone_version),
        tools: args.tools.clone(),
        scenarios,
    };

    let results_dir = args
        .results_dir
        .clone()
        .unwrap_or_else(|| repo_root().join("bench").join("results"));
    let (md, json) = report.write_to(&results_dir)?;

    println!("\n{}", report.to_markdown());
    println!("report: {}", md.display());
    println!("raw:    {}", json.display());

    if !report.all_ok() {
        anyhow::bail!("at least one benchmark phase failed - see the table above");
    }
    Ok(())
}

/// Runs every (fixture x tool) scenario, each in its own remote subfolder and
/// its own state database.
async fn run_scenarios(
    args: &RunArgs,
    specs: &[FixtureSpec],
    creds: &creds::BenchCreds,
    run_folder: &creds::RunFolder,
    rclone_binary: Option<&Path>,
) -> Result<Vec<ScenarioReport>> {
    let fixture_cache = fixture_root(args.fixture_root.clone());
    let work = tempfile::tempdir().context("creating the harness work directory")?;

    let mut scenarios = Vec::new();
    for spec in specs {
        // Built once and shared by every tool, so they upload identical bytes.
        let mut fixture = Fixture::build(&fixture_cache, spec)?;
        let mut results = Vec::new();

        for &tool in &args.tools {
            let scenario = format!("{}-{}", tool.slug(), spec.shape.slug());
            println!("\n=== {scenario} ===");
            let folder_id = run_folder.child(&scenario).await?;

            // The cold and incremental phases share one destination folder and,
            // for Driven, one state database. A fresh state db per phase would
            // make the incremental phase a second cold upload.
            let state_db = work.path().join(&scenario).join("state.db");
            let rclone_config = work.path().join(format!("{scenario}.conf"));
            std::fs::create_dir_all(state_db.parent().expect("state db has a parent"))?;
            if tool == Tool::Rclone {
                tools::write_rclone_config(&rclone_config, creds, &folder_id)?;
            }

            // Resolved before the closure so mutating the fixture between
            // phases does not collide with a live borrow of it.
            let tree = fixture.tree();
            let phase = |phase: Phase| -> Result<PhaseResult> {
                let result = match tool {
                    Tool::Driven => tools::run_driven(phase, &tree, &folder_id, &state_db)?,
                    Tool::Rclone => tools::run_rclone(
                        phase,
                        rclone_binary.expect("checked when rclone was requested"),
                        &rclone_config,
                        &tree,
                        args.rclone_transfers,
                    )?,
                };
                println!(
                    "  {phase:<12} {:>8.1}s  {:>6} files  {:>10}{}",
                    result.wall_secs,
                    result
                        .files_transferred
                        .map(|f| f.to_string())
                        .unwrap_or_else(|| "?".into()),
                    result
                        .bytes_transferred
                        .map(human_bytes)
                        .unwrap_or_else(|| "?".into()),
                    result
                        .detail
                        .as_deref()
                        .map(|d| format!("  [{d}]"))
                        .unwrap_or_default(),
                );
                Ok(result)
            };

            results.push(phase(Phase::Cold)?);

            let touched = fixture.mutate(MUTATION_FRACTION)?;
            println!("  mutated {} of {} file(s)", touched.len(), spec.files);

            results.push(phase(Phase::Incremental)?);

            // Hand the next tool a pristine tree - it must see exactly what this
            // one saw on its cold phase.
            fixture.restore()?;
        }

        scenarios.push(ScenarioReport {
            spec: spec.clone(),
            results,
        });
    }
    Ok(scenarios)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_scale_defines_both_shapes() {
        for scale in [Scale::Smoke, Scale::Small, Scale::Medium, Scale::Full] {
            let specs = scale.specs(1);
            assert_eq!(specs.len(), 2, "{} must define both shapes", scale.slug());
            assert!(specs.iter().any(|s| s.shape == Shape::Huge));
            assert!(specs.iter().any(|s| s.shape == Shape::TinyDeep));
        }
    }

    #[test]
    fn scales_increase_monotonically() {
        let total = |scale: Scale| -> u64 { scale.specs(1).iter().map(|s| s.total_bytes()).sum() };
        assert!(total(Scale::Smoke) < total(Scale::Small));
        assert!(total(Scale::Small) < total(Scale::Medium));
        assert!(total(Scale::Medium) < total(Scale::Full));
    }

    #[test]
    fn the_smoke_scale_fits_under_the_default_cap_for_both_tools() {
        let planned: u64 = Scale::Smoke
            .specs(1)
            .iter()
            .map(|s| s.total_bytes())
            .sum::<u64>()
            * 2;
        assert!(
            planned < DEFAULT_MAX_UPLOAD_BYTES,
            "the smoke scale must never trip the upload cap"
        );
    }

    #[test]
    fn the_full_scale_exceeds_the_default_cap_so_it_needs_an_explicit_opt_in() {
        let planned: u64 = Scale::Full.specs(1).iter().map(|s| s.total_bytes()).sum();
        assert!(
            planned > DEFAULT_MAX_UPLOAD_BYTES,
            "the full scale must require --full rather than running by accident"
        );
    }

    #[test]
    fn the_full_scale_is_the_shape_the_suite_is_about() {
        let specs = Scale::Full.specs(1);
        let tiny = specs.iter().find(|s| s.shape == Shape::TinyDeep).unwrap();
        assert_eq!(tiny.files, 1_000_000);
        let huge = specs.iter().find(|s| s.shape == Shape::Huge).unwrap();
        assert_eq!(huge.huge_file_bytes, 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn dedupe_tools_collapses_repeats_and_keeps_order() {
        assert_eq!(
            dedupe_tools(&[Tool::Rclone, Tool::Driven, Tool::Rclone]),
            vec![Tool::Rclone, Tool::Driven]
        );
        assert_eq!(dedupe_tools(&[Tool::Driven]), vec![Tool::Driven]);
        assert!(dedupe_tools(&[]).is_empty());
    }

    #[test]
    fn repo_root_contains_the_workspace_manifest() {
        assert!(
            repo_root().join("Cargo.toml").is_file(),
            "repo_root() must resolve to the workspace root"
        );
    }
}
