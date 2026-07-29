//! The root mount broker binary (DESIGN s5.3.2). Small by design: parse argv,
//! refuse to run as non-root, sweep stale mounts, serve the socket loop. All
//! privileged logic lives in `driven_apfs::server`.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match driven_apfs::launch::parse_helper_args(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("driven-apfs-helper: {e}");
            std::process::exit(2);
        }
    };

    #[cfg(target_os = "macos")]
    {
        if let Err(e) = driven_apfs::server::run(parsed) {
            eprintln!("driven-apfs-helper: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = parsed;
        eprintln!("driven-apfs-helper: only supported on macOS");
        std::process::exit(1);
    }
}
