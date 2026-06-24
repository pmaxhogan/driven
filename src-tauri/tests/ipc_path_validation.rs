//! IPC path-validation integration tests (SPEC s11.6.1).
//!
//! SPEC s11.6.1 mandates these cases for the path-bearing write commands:
//! traversal attempt, symlink-at-leaf, non-existent parent, a path OUTSIDE the
//! dialog handle (rejected), and the valid case. They exercise the REAL
//! `validate_writable_dest` + `DialogToken` helpers (now `pub` for this test)
//! plus `atomic_write`, which back `add_source` / `export_diagnostic_bundle`.
//!
//! C1: the dialog-token confinement model is that the backend mints a token
//! bound to the directory the user picked via a native dialog; a write is
//! confined to that directory (no `..`, no symlink-at-leaf). A path the webview
//! shapes outside the dialog-approved root - or a leaf symlink - is rejected.

use std::path::PathBuf;

use driven_app_lib::commands::{atomic_write, validate_writable_dest, DialogToken};

/// A unique temp directory under the OS temp dir (no `tempfile` dep needed).
fn tempdir() -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("driven-ipc-it-{nonce}-{:p}", &nonce));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn cleanup(dir: PathBuf) {
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn valid_dest_in_the_dialog_root_is_accepted() {
    let dir = tempdir();
    let token = DialogToken::for_root(dir.to_string_lossy().to_string());
    let dest = dir.join("driven-diagnostics.zip");
    let confined = validate_writable_dest(&dest, &token).expect("valid dest accepted");
    assert_eq!(
        confined.file_name().unwrap(),
        std::ffi::OsStr::new("driven-diagnostics.zip")
    );
    // The confined path sits under the dialog-approved (canonical) root.
    let canon_root = dunce::canonicalize(&dir).unwrap();
    assert!(confined.starts_with(&canon_root));
    cleanup(dir);
}

#[test]
fn traversal_is_rejected() {
    let dir = tempdir();
    let token = DialogToken::for_root(dir.to_string_lossy().to_string());
    // A `..` segment must be rejected before any filesystem write.
    let dest = dir.join("..").join("escape.zip");
    let err = validate_writable_dest(&dest, &token).expect_err("traversal rejected");
    assert_eq!(err.code.to_string(), "local.io_error");
    cleanup(dir);
}

#[test]
fn nonexistent_parent_is_rejected() {
    let dir = tempdir();
    let token = DialogToken::for_root(dir.to_string_lossy().to_string());
    let dest = dir.join("no-such-subdir").join("bundle.zip");
    let err = validate_writable_dest(&dest, &token).expect_err("missing parent rejected");
    assert_eq!(err.code.to_string(), "local.io_error");
    cleanup(dir);
}

#[test]
fn path_outside_the_dialog_root_is_rejected() {
    // SPEC s11.6.1: a path outside the dialog handle is rejected - the webview
    // cannot escape the directory the user actually picked.
    let root = tempdir();
    let other = tempdir();
    let token = DialogToken::for_root(root.to_string_lossy().to_string());
    let dest = other.join("bundle.zip");
    let err = validate_writable_dest(&dest, &token).expect_err("outside-root rejected");
    assert_eq!(err.code.to_string(), "local.io_error");
    assert!(err.message.contains("outside"));
    cleanup(root);
    cleanup(other);
}

#[cfg(unix)]
#[test]
fn symlink_at_leaf_is_rejected() {
    use std::os::unix::fs::symlink;
    let dir = tempdir();
    let token = DialogToken::for_root(dir.to_string_lossy().to_string());
    let target = dir.join("real-target");
    std::fs::write(&target, b"x").unwrap();
    let link = dir.join("link.zip");
    symlink(&target, &link).unwrap();
    let err = validate_writable_dest(&link, &token).expect_err("symlink leaf rejected");
    assert_eq!(err.code.to_string(), "local.io_error");
    assert!(err.message.contains("symlink"));
    cleanup(dir);
}

#[test]
fn atomic_write_round_trips_in_the_confined_dest() {
    // SPEC s11.6.1 step 5: the confined dest is written atomically and leaves no
    // temp file behind - the export path's exact contract.
    let dir = tempdir();
    let token = DialogToken::for_root(dir.to_string_lossy().to_string());
    let confined = validate_writable_dest(&dir.join("out.zip"), &token).unwrap();
    atomic_write(&confined, b"PK\x03\x04 driven").expect("atomic write");
    assert_eq!(std::fs::read(&confined).unwrap(), b"PK\x03\x04 driven");
    let temps: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(".driven-tmp."))
        .collect();
    assert!(temps.is_empty(), "atomic write must leave no temp files");
    cleanup(dir);
}
