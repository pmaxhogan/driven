//! `driven-chaos` - stress / chaos harness binary.
//!
//! See `design/STRESS_HARNESS.md`. Drives the headless core (does not
//! depend on `src-tauri/`) and exits with code 0 on all-pass/skip,
//! 1 on any FAIL, 2 on harness self-error (`harness.timeout` etc.).

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    eprintln!("driven-chaos: not yet implemented (M3.7)");
    Ok(())
}
