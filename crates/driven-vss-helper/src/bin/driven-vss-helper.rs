//! The elevated VSS helper broker binary (DESIGN s5.3.1).
//!
//! Launched elevated by the main app (`ShellExecute runas`). It parses
//! `--pipe <name> --allowed-root <path>...`, refuses to run un-elevated, and
//! runs the named-pipe server loop until the app asks it to shut down. It holds
//! no OAuth tokens, no network stack, and no Drive credentials - only the VSS
//! snapshot capability the un-elevated app cannot use itself.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    #[cfg(windows)]
    {
        // Minimal stderr tracing so an elevated run left in a console is
        // diagnosable; the app does not read the helper's stdout/stderr.
        let _ = tracing_subscriber_init();

        match driven_vss_helper::launch::parse_helper_args(&args) {
            Ok((pipe, roots)) => {
                if let Err(e) = driven_vss_helper::run_server(&pipe, roots) {
                    eprintln!("driven-vss-helper: {e}");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("driven-vss-helper: {e}");
                std::process::exit(2);
            }
        }
    }

    #[cfg(not(windows))]
    {
        let _ = args;
        eprintln!("driven-vss-helper is only supported on Windows");
        std::process::exit(1);
    }
}

/// Best-effort tracing init to stderr (no external subscriber dep; uses the
/// tracing crate's default no-op if unavailable). Kept tiny.
#[cfg(windows)]
fn tracing_subscriber_init() -> Result<(), ()> {
    // The helper deliberately avoids a heavy logging stack; `tracing` events
    // without a subscriber are simply dropped, which is fine for a broker whose
    // errors also surface over the pipe as `Error` frames.
    Ok(())
}
