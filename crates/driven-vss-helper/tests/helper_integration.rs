//! End-to-end integration test for the least-privilege VSS helper (DESIGN
//! s5.3.1). Windows only.
//!
//! It spins the real named-pipe server (in a background thread of THIS process),
//! then drives the real [`HelperClient`] over the pipe to (1) snapshot a
//! genuinely EXCLUSIVELY-LOCKED file and stream its bytes back - proving the
//! server + client + protocol + validation + auth + VSS + streaming path,
//! (2) reject a request for a path OUTSIDE the configured backup roots, and
//! (3) reject a `..` traversal request. A second test proves the app-side
//! [`BrokeredVssProvider`] temp-copy path.
//!
//! Honestly GATE-SKIPPED when the process is not elevated (VSS snapshot creation
//! needs Administrator): CI is non-elevated, so this prints a SKIP reason and
//! returns rather than failing - it is NOT `#[ignore]`-faked. A local elevated
//! `cargo test` (e.g. via `sudo`) exercises the real COM + pipe path end-to-end.

#![cfg(windows)]

use std::os::windows::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use driven_vss::{SnapshotOutcome, VssMode, VssProvider};
use driven_vss_helper::launch::generate_pipe_name;
use driven_vss_helper::{BrokeredVssProvider, HelperClient};

/// Spawn the helper server on `pipe_name` allowing `roots`, returning the join
/// handle. The server runs in-process (same integrity level as this elevated
/// test), so both the client- and server-image identity checks pass naturally.
fn spawn_server(pipe_name: String, roots: Vec<PathBuf>) -> thread::JoinHandle<Result<(), String>> {
    thread::spawn(move || driven_vss_helper::run_server(&pipe_name, roots))
}

/// Open `path` with an exclusive write lock (no sharing) and return the held
/// handle - a plain shared read then fails with ERROR_SHARING_VIOLATION, the
/// exact case VSS exists to handle.
fn lock_exclusively(path: &std::path::Path) -> std::fs::File {
    const GENERIC_WRITE: u32 = 0x4000_0000;
    std::fs::OpenOptions::new()
        .access_mode(GENERIC_WRITE)
        .share_mode(0)
        .write(true)
        .open(path)
        .expect("open the file exclusively")
}

#[test]
fn helper_streams_a_locked_file_and_enforces_the_boundary() {
    if !driven_vss::is_elevated() {
        eprintln!(
            "SKIP helper_streams_a_locked_file_and_enforces_the_boundary: process is not \
             elevated; VSS snapshot creation requires Administrator. Run an elevated \
             `cargo test` (e.g. via sudo) to exercise the real COM + pipe path (CI is \
             non-elevated by design)."
        );
        return;
    }

    let src = tempfile::tempdir().expect("temp source dir");
    let root = src.path().to_path_buf();
    let contents = b"locked-outlook-pst-like-bytes-that-must-still-back-up-via-the-helper";
    let live = root.join("locked.dat");
    std::fs::write(&live, contents).expect("write the source file");

    // Hold it under an exclusive lock for the whole test.
    let _exclusive = lock_exclusively(&live);
    // Sanity: a plain shared read must now be blocked.
    assert!(
        std::fs::File::open(&live).is_err(),
        "test setup: the file must be exclusively locked"
    );

    let pipe = generate_pipe_name();
    let server = spawn_server(pipe.clone(), vec![root.clone()]);
    let helper_dir = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    let volume = live.to_string_lossy().chars().take(2).collect::<String>(); // "C:"

    let mut client =
        HelperClient::connect(&pipe, &helper_dir).expect("client connects + handshakes");

    // 1. Positive: stream the locked file's bytes back and compare.
    let size = client
        .open_locked(&volume, &live.to_string_lossy())
        .expect("helper opens the locked file via VSS");
    assert_eq!(size, contents.len() as u64, "reported size matches");

    let mut streamed = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = client.read_chunk(&mut buf).expect("read a chunk");
        if n == 0 {
            break;
        }
        streamed.extend_from_slice(&buf[..n]);
    }
    assert_eq!(
        streamed, contents,
        "the bytes streamed from the VSS snapshot must equal the locked file's contents"
    );
    client.close_file().expect("close the file");

    // 2. Negative: a path OUTSIDE the configured roots is rejected.
    let outside = r"C:\Windows\System32\drivers\etc\hosts";
    let err = client
        .open_locked("C:", outside)
        .expect_err("a path outside the roots must be rejected");
    assert!(
        err.contains("not_allowed"),
        "expected a not_allowed rejection, got: {err}"
    );

    // 3. Negative: a `..` traversal request is rejected.
    let traversal = format!(r"{}\..\..\Windows\win.ini", root.display());
    let err = client
        .open_locked(&volume, &traversal)
        .expect_err("a traversal path must be rejected");
    assert!(
        err.contains("invalid_request") || err.contains("not_allowed"),
        "expected a traversal rejection, got: {err}"
    );

    // Done: shut the server down cleanly and join.
    client.shutdown().expect("shutdown the helper");
    drop(client);
    let server_result = server.join().expect("server thread joins");
    assert!(
        server_result.is_ok(),
        "server exited cleanly: {server_result:?}"
    );
}

#[test]
fn brokered_provider_maps_a_locked_file_to_a_readable_temp_copy() {
    if !driven_vss::is_elevated() {
        eprintln!(
            "SKIP brokered_provider_maps_a_locked_file_to_a_readable_temp_copy: not elevated; \
             VSS needs Administrator. Run an elevated `cargo test` to exercise it."
        );
        return;
    }

    let src = tempfile::tempdir().expect("temp source dir");
    let root = src.path().to_path_buf();
    let contents = b"brokered-provider-temp-copy-roundtrip-bytes";
    let live = root.join("db.mdf");
    std::fs::write(&live, contents).expect("write source");
    let _exclusive = lock_exclusively(&live);

    let pipe = generate_pipe_name();
    let server = spawn_server(pipe.clone(), vec![root.clone()]);
    let helper_dir = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let temp_dir = tempfile::tempdir().expect("temp dir for streamed copies");

    let provider: Arc<dyn VssProvider> = Arc::new(BrokeredVssProvider::new(
        VssMode::Auto,
        pipe.clone(),
        helper_dir.clone(),
        temp_dir.path().to_path_buf(),
    ));

    // Downcast is not needed - probe via a concrete handle for the test setup.
    let concrete = BrokeredVssProvider::new(
        VssMode::Auto,
        pipe.clone(),
        helper_dir.clone(),
        temp_dir.path().to_path_buf(),
    );
    assert!(concrete.probe(), "provider should reach the helper");
    assert!(concrete.available(), "provider should report available");

    let outcome = concrete.map_for_volume(&live);
    match outcome {
        SnapshotOutcome::Mapped(temp) => {
            let got = std::fs::read(&temp).expect("read the streamed temp copy");
            assert_eq!(got, contents, "temp copy must equal the locked file");
            concrete.end_cycle();
            assert!(!temp.exists(), "end_cycle must delete the temp copy");
        }
        SnapshotOutcome::Unavailable => panic!("provider degraded unexpectedly"),
    }

    // The Arc<dyn VssProvider> path also degrades cleanly when unprobed.
    assert_eq!(
        provider.map_for_volume(&live),
        SnapshotOutcome::Unavailable,
        "an unprobed provider must degrade"
    );

    // Shut the helper down and join.
    concrete.shutdown_helper();
    let _ = server.join().expect("server joins");
}
