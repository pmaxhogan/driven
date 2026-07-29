//! Integration tests for the APFS mount broker's trust boundary
//! (DESIGN s5.3.2).
//!
//! These drive the REAL socket server on macOS as an unprivileged user, which
//! means the privileged operations themselves (mount/unmount/delete) cannot
//! run here - the server refuses to start as non-root by design. What IS
//! exercised end-to-end is the part that must never regress: the framing, the
//! handshake, and the CLIENT's refusal to speak to a non-root peer (the
//! same-uid socket-squatting defence). The mount/validation logic is
//! unit-tested in-crate, and the real root path is verified manually on
//! hardware (see the PR description).

#![cfg(target_os = "macos")]

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use driven_apfs::client::{ClientError, HelperClient};
use driven_apfs::protocol::{read_control, write_control, Control, PROTOCOL_VERSION};

/// A fake "broker" bound by the TEST process (uid = the unprivileged test
/// user, NOT root). The client must refuse to speak to it.
#[test]
fn client_refuses_a_non_root_socket_peer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("squatter.sock");
    let listener = UnixListener::bind(&socket).expect("bind");

    // Answer one connection in the background so the client would get a valid
    // handshake IF it were willing to speak - proving the rejection comes from
    // the peer-uid check and not from a dead socket.
    let handle = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            if let Ok(Control::Hello { .. }) = read_control(&mut stream) {
                let _ = write_control(
                    &mut stream,
                    &Control::HelloOk {
                        protocol_version: PROTOCOL_VERSION,
                    },
                );
            }
        }
    });

    let err = HelperClient::connect(&socket).expect_err("must refuse a non-root peer");
    assert!(
        matches!(err, ClientError::NotRoot),
        "expected NotRoot, got {err:?}"
    );

    // Unblock the accept thread if it is still waiting.
    let _ = UnixStream::connect(&socket);
    let _ = handle.join();
}

/// Connecting to a path with nothing listening fails fast (the provider maps
/// this to a TRANSIENT skip, never a hang).
#[test]
fn connect_to_a_dead_socket_fails_fast() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket: PathBuf = dir.path().join("absent.sock");
    let started = std::time::Instant::now();
    let err = HelperClient::connect(&socket).expect_err("no broker is listening");
    assert!(matches!(err, ClientError::Io(_)), "got {err:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "connect must fail fast, took {:?}",
        started.elapsed()
    );
}

/// The broker binary refuses to run as a non-root user - the first line of
/// defence for a privileged helper.
#[test]
fn helper_binary_refuses_to_run_unprivileged() {
    let exe = env!("CARGO_BIN_EXE_driven-apfs-helper");
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("s.sock");
    let out = std::process::Command::new(exe)
        .args([
            "--socket",
            &socket.to_string_lossy(),
            "--peer-uid",
            "501",
            "--app-pid",
            &std::process::id().to_string(),
            "--allowed-volume",
            "/System/Volumes/Data",
        ])
        .output()
        .expect("spawn helper");
    assert!(!out.status.success(), "helper must refuse to run as non-root");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("must run as root"),
        "unexpected stderr: {stderr}"
    );
    assert!(
        !socket.exists(),
        "a refusing helper must not leave a socket behind"
    );
}

/// Malformed argv is rejected before any privileged work.
#[test]
fn helper_binary_rejects_bad_arguments() {
    let exe = env!("CARGO_BIN_EXE_driven-apfs-helper");
    for args in [
        vec!["--socket", "/tmp/x.sock"],           // no uid/pid
        vec!["--bogus", "x"],                      // unknown flag
        vec!["--socket", "/tmp/x.sock", "--peer-uid"], // dangling value
    ] {
        let out = std::process::Command::new(exe)
            .args(&args)
            .output()
            .expect("spawn helper");
        assert_eq!(
            out.status.code(),
            Some(2),
            "argv {args:?} should exit 2, stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// A frame declaring a huge length must be rejected before allocation - the
/// boundary must never allocate on a peer's say-so.
#[test]
fn oversize_frame_is_refused_by_the_reader() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("huge.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        // Declare 4 GiB - 1 of payload, then send nothing.
        let _ = stream.write_all(&u32::MAX.to_be_bytes());
        let _ = stream.flush();
        let mut sink = Vec::new();
        let _ = stream.read_to_end(&mut sink);
    });

    let mut stream = UnixStream::connect(&socket).expect("connect");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("timeout");
    let err = read_control(&mut stream).expect_err("must reject the oversize frame");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    drop(stream);
    let _ = handle.join();
}
