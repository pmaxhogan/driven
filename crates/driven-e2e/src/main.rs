//! `driven-e2e` - WebDriver end-to-end suite for the REAL Driven desktop app.
//!
//! Drives the actual Tauri binary (WebKitGTK webview on Linux) through
//! `tauri-driver` -> `WebKitWebDriver`, inside the `e2e-runtime` Docker image
//! (see the repo `Dockerfile`). This is the app-level complement to the
//! engine-level `driven-chaos` harness: the wizard, source add, sync, restore,
//! settings persistence, and fault surfacing are exercised through the same
//! IPC + UI path a user takes. See `.claude/skills/driven-agent-qa` for the
//! agent-facing runbook.
//!
//! Environment:
//! - `DRIVEN_E2E_APP_BINARY` - the app binary tauri-driver launches
//!   (default `/usr/local/bin/driven-app`).
//! - `DRIVEN_E2E_TAURI_DRIVER` / `DRIVEN_E2E_NATIVE_DRIVER` - driver binaries
//!   (defaults: `tauri-driver` on PATH, `/usr/bin/WebKitWebDriver`).
//! - `DRIVEN_E2E_ARTIFACTS` - where screenshots / preserved evidence land
//!   (default `/tmp/driven-e2e-artifacts`).
//!
//! Exit codes (STRESS_HARNESS s9 semantics): 0 = every scenario passed or
//! skipped; 1 = at least one failure; 2 = infrastructure error.

// The harness drives WebKitGTK/tauri-driver and injects POSIX permission
// faults - it is unix-only by design (it ships and runs inside its Linux
// container). Everything is cfg-gated so `cargo build --workspace
// --all-targets` on the Windows CI runner compiles a stub instead of
// tripping over unix-only APIs (PermissionsExt, geteuid).
#[cfg(unix)]
mod flows;
#[cfg(unix)]
mod scenario;
#[cfg(unix)]
mod scenarios;
#[cfg(unix)]
mod session;

#[cfg(unix)]
use clap::{Parser, Subcommand};

#[cfg(unix)]
use scenario::{Ctx, Verdict};

#[cfg(not(unix))]
fn main() {
    eprintln!(
        "driven-e2e runs only inside its Linux e2e container (see the repo \
         Dockerfile e2e-runtime stage / `just e2e`)."
    );
    std::process::exit(2);
}

/// Driven app-level e2e suite (WebDriver, Linux container).
#[cfg(unix)]
#[derive(Debug, Parser)]
#[command(name = "driven-e2e", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[cfg(unix)]
#[derive(Debug, Subcommand)]
enum Command {
    /// List every registered scenario.
    List,
    /// Run the named scenarios (kebab-case ids from `list`).
    Run {
        /// Scenario names to run.
        names: Vec<String>,
    },
    /// Run every scenario.
    RunAll,
    /// Verify the e2e environment (driver binaries, app binary, display) and
    /// print a diagnosis. Exit 0 = ready.
    Doctor,
}

#[cfg(unix)]
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let code = match cli.command {
        Command::List => {
            for s in scenarios::all() {
                println!("{:<28} {}", s.name(), s.description());
            }
            0
        }
        Command::Run { names } => run_selected(&names).await,
        Command::RunAll => run_selected(&[]).await,
        Command::Doctor => doctor().await,
    };
    std::process::exit(code);
}

#[cfg(unix)]
/// Run the selected scenarios (empty = all). Returns the process exit code.
async fn run_selected(names: &[String]) -> i32 {
    let all = scenarios::all();
    let selected: Vec<_> = if names.is_empty() {
        all
    } else {
        let wanted: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();
        let matched: Vec<_> = all
            .into_iter()
            .filter(|s| wanted.contains(s.name()))
            .collect();
        if matched.len() != wanted.len() {
            let have: std::collections::HashSet<&str> = matched.iter().map(|s| s.name()).collect();
            for miss in wanted.difference(&have) {
                eprintln!("unknown scenario: {miss} (see `driven-e2e list`)");
            }
            return 2;
        }
        matched
    };

    let mut failures = 0usize;
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for s in selected {
        let name = s.name().to_string();
        println!("=== {name}: {desc}", desc = s.description());
        let started = std::time::Instant::now();
        let verdict = match Ctx::new(&name) {
            Ok(ctx) => match s.run(&ctx).await {
                Ok(v) => v,
                Err(e) => Verdict::Fail(format!("infrastructure error: {e:#}")),
            },
            Err(e) => Verdict::Fail(format!("artifacts dir: {e:#}")),
        };
        let secs = started.elapsed().as_secs();
        let (tag, detail) = match &verdict {
            Verdict::Pass => ("PASS".to_string(), String::new()),
            Verdict::Skip(why) => ("SKIP".to_string(), why.clone()),
            Verdict::Fail(why) => {
                failures += 1;
                ("FAIL".to_string(), why.clone())
            }
        };
        println!("--- {name}: {tag} ({secs}s) {detail}");
        rows.push((name, tag, detail));
    }

    println!("\n==== driven-e2e report ====");
    for (name, tag, detail) in &rows {
        println!("{tag:<5} {name:<28} {detail}");
    }
    let artifacts = session::Env::artifacts_dir();
    println!("artifacts: {}", artifacts.display());
    if failures > 0 {
        println!("{failures} scenario(s) FAILED");
        1
    } else {
        println!("all scenarios passed or skipped");
        0
    }
}

#[cfg(unix)]
/// Environment diagnosis (binaries present, display up, driver bootable).
async fn doctor() -> i32 {
    let mut ok = true;
    let app = session::Env::app_binary();
    let present = std::path::Path::new(&app).is_file();
    println!(
        "app binary        {app}: {}",
        if present { "ok" } else { "MISSING" }
    );
    ok &= present;
    for (label, bin) in [
        ("tauri-driver", session::Env::tauri_driver()),
        ("native driver", session::Env::native_driver()),
    ] {
        let found = std::path::Path::new(&bin).is_file()
            || std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .any(|d| d.join(&bin).is_file());
        println!(
            "{label:<17} {bin}: {}",
            if found { "ok" } else { "MISSING" }
        );
        ok &= found;
    }
    let display = std::env::var("DISPLAY").unwrap_or_default();
    println!(
        "DISPLAY           {}",
        if display.is_empty() {
            "(unset)"
        } else {
            &display
        }
    );
    ok &= !display.is_empty();

    // The full proof: boot one real session and read the app's route.
    match session::AppSession::launch(session::SessionConfig {
        fake_remote: true,
        ..Default::default()
    })
    .await
    {
        Ok(s) => {
            let path = s
                .eval("location.pathname")
                .await
                .map(|v| v.to_string())
                .unwrap_or_else(|e| format!("(eval failed: {e})"));
            println!("app boot          ok (route {path})");
        }
        Err(e) => {
            println!("app boot          FAILED: {e:#}");
            ok = false;
        }
    }
    if ok {
        println!("doctor: ready");
        0
    } else {
        println!("doctor: NOT ready");
        2
    }
}
