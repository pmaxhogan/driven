//! `driven-chaos` - stress / chaos harness binary (STRESS_HARNESS s2).
//!
//! Drives the headless core (does not depend on `src-tauri/`) and exits
//! with code 0 on all-pass/skip, 1 on any FAIL, 2 on harness self-error
//! (STRESS_HARNESS s9). The subcommand surface lives in
//! [`driven_chaos::dispatch`]; this binary only parses argv, probes
//! capabilities once, and routes.

use std::process::ExitCode;

use driven_chaos::capabilities::CapabilitySet;
use driven_chaos::dispatch::{self, exit_code};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match dispatch::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("driven-chaos: {e}");
            eprintln!("usage: driven-chaos <fixture|scenario|fuzz|mutator|report> ...");
            return ExitCode::from(exit_code::HARNESS_ERROR as u8);
        }
    };

    // Probe host capabilities once, cached for the run (STRESS_HARNESS s2.5).
    let caps = CapabilitySet::probe();

    let code = dispatch::run(command, &caps).await;
    ExitCode::from(code as u8)
}
