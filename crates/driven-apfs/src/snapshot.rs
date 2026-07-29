//! Unprivileged `tmutil` snapshot operations and their (pure, cross-OS
//! testable) output parsing + name/date validation.
//!
//! Creation, enumeration AND deletion all need NO privilege: `tmutil` proxies
//! over XPC to Apple's entitled `backupd`. Deletion was originally routed
//! through the root broker on the assumption it needed root; measurement on
//! macOS 26 showed `tmutil deletelocalsnapshots <date>` succeeding as an
//! ordinary user (exit 0, snapshot gone), so it moved here and the broker lost
//! a verb - one less client-controlled string reaching a root argv.
//!
//! # Auto-thinning is a feature, not a bug
//!
//! macOS purges local snapshots under disk pressure and over time. Any
//! consumer of a snapshot name must treat later ENOENT (name gone between
//! create and mount, or files gone mid-read) as an expected race that degrades
//! to skip-the-locked-file - never as a hard failure.

use std::process::Command;

/// The prefix + suffix every APFS local Time Machine snapshot name carries.
pub const SNAPSHOT_NAME_PREFIX: &str = "com.apple.TimeMachine.";
/// See [`SNAPSHOT_NAME_PREFIX`].
pub const SNAPSHOT_NAME_SUFFIX: &str = ".local";

/// Errors from the unprivileged snapshot operations.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// Spawning `tmutil` failed (missing binary, exec error).
    #[error("tmutil could not be run: {0}")]
    Spawn(String),
    /// `tmutil` exited non-zero. Carries a short, secret-free detail.
    #[error("tmutil failed: {0}")]
    Failed(String),
    /// `tmutil` succeeded but its output did not carry a parseable snapshot
    /// date/name (format drift across macOS versions).
    #[error("tmutil output was not parseable: {0}")]
    Parse(String),
}

/// Validate a `tmutil` snapshot date stamp: exactly `YYYY-MM-DD-HHMMSS`
/// (digits and dashes in fixed positions). This is the shape both
/// `tmutil localsnapshot` prints and `tmutil deletelocalsnapshots` accepts,
/// so it is strict: anything else is rejected before reaching a subprocess
/// argv.
pub fn is_valid_snapshot_date(date: &str) -> bool {
    let b = date.as_bytes();
    if b.len() != 17 {
        return false;
    }
    for (i, c) in b.iter().enumerate() {
        match i {
            4 | 7 | 10 => {
                if *c != b'-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_digit() {
                    return false;
                }
            }
        }
    }
    true
}

/// Validate a full APFS local snapshot name:
/// `com.apple.TimeMachine.<valid date>.local`. The broker's boundary check
/// for [`Control::MountSnapshot`] - strict by design; Driven only ever mounts
/// Time Machine local snapshots it just created.
///
/// [`Control::MountSnapshot`]: crate::protocol::Control::MountSnapshot
pub fn is_valid_snapshot_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(SNAPSHOT_NAME_PREFIX) else {
        return false;
    };
    let Some(date) = rest.strip_suffix(SNAPSHOT_NAME_SUFFIX) else {
        return false;
    };
    is_valid_snapshot_date(date)
}

/// The snapshot name for a `tmutil` date stamp.
pub fn snapshot_name_for_date(date: &str) -> String {
    format!("{SNAPSHOT_NAME_PREFIX}{date}{SNAPSHOT_NAME_SUFFIX}")
}

/// The date stamp inside a valid snapshot name (`None` when the name does not
/// validate).
pub fn date_of_snapshot_name(name: &str) -> Option<&str> {
    if !is_valid_snapshot_name(name) {
        return None;
    }
    name.strip_prefix(SNAPSHOT_NAME_PREFIX)?
        .strip_suffix(SNAPSHOT_NAME_SUFFIX)
}

/// Parse `tmutil localsnapshot` stdout into the created snapshot's date stamp.
///
/// Known shape (Catalina through Tahoe):
/// `Created local snapshot with date: 2026-07-29-154532`. Parsed leniently
/// (scan every line for the last `: `-separated token that validates) so
/// cosmetic wording drift does not break us, but the extracted date is always
/// re-validated strictly.
pub fn parse_localsnapshot_output(stdout: &str) -> Result<String, SnapshotError> {
    for line in stdout.lines() {
        let candidate = line.rsplit(':').next().map(str::trim).unwrap_or("");
        if is_valid_snapshot_date(candidate) {
            return Ok(candidate.to_string());
        }
    }
    Err(SnapshotError::Parse(format!(
        "no snapshot date in {} line(s) of tmutil output",
        stdout.lines().count()
    )))
}

/// Parse `tmutil listlocalsnapshots <mount>` stdout into valid snapshot names
/// (invalid/foreign lines are skipped, e.g. the `Snapshots for volume group`
/// header newer macOS prints).
pub fn parse_listlocalsnapshots_output(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| is_valid_snapshot_name(l))
        .map(str::to_string)
        .collect()
}

/// Create an APFS local snapshot (unprivileged). Returns the new snapshot's
/// date stamp.
///
/// `tmutil localsnapshot` snapshots every locally-mounted APFS volume in the
/// Time Machine set at once - there is no per-volume create - so the caller
/// treats the returned date as covering ALL volumes it will subsequently
/// mount-and-map this cycle.
#[cfg(target_os = "macos")]
pub fn create_local_snapshot() -> Result<String, SnapshotError> {
    let out = Command::new("/usr/bin/tmutil")
        .arg("localsnapshot")
        .output()
        .map_err(|e| SnapshotError::Spawn(e.to_string()))?;
    if !out.status.success() {
        return Err(SnapshotError::Failed(format!(
            "tmutil localsnapshot exited {}",
            out.status
        )));
    }
    parse_localsnapshot_output(&String::from_utf8_lossy(&out.stdout))
}

/// Delete the APFS local snapshot with this `tmutil` date stamp
/// (unprivileged - see the module docs).
///
/// The date is validated before it reaches the argv even though this is no
/// longer a privileged call: it is still a subprocess argument, and the
/// validation is free.
///
/// A snapshot that APFS already auto-thinned away exits non-zero; that is an
/// expected race, so this reports success for it. Only a spawn failure or an
/// invalid date is an error.
#[cfg(target_os = "macos")]
pub fn delete_local_snapshot(date: &str) -> Result<(), SnapshotError> {
    if !is_valid_snapshot_date(date) {
        return Err(SnapshotError::Parse(
            "snapshot date does not match the required shape".to_string(),
        ));
    }
    let out = Command::new("/usr/bin/tmutil")
        .arg("deletelocalsnapshots")
        .arg(date)
        .output()
        .map_err(|e| SnapshotError::Spawn(e.to_string()))?;
    if !out.status.success() {
        // Already gone (auto-thinned, or a prior delete won the race) reads as
        // success; the caller only ever wants "it is not there any more".
        tracing::debug!(
            status = %out.status,
            "tmutil deletelocalsnapshots exited non-zero; treating as already-deleted"
        );
    }
    Ok(())
}

/// List the APFS local snapshot names for `volume_mount` (unprivileged).
#[cfg(target_os = "macos")]
pub fn list_local_snapshots(volume_mount: &str) -> Result<Vec<String>, SnapshotError> {
    let out = Command::new("/usr/bin/tmutil")
        .arg("listlocalsnapshots")
        .arg(volume_mount)
        .output()
        .map_err(|e| SnapshotError::Spawn(e.to_string()))?;
    if !out.status.success() {
        return Err(SnapshotError::Failed(format!(
            "tmutil listlocalsnapshots exited {}",
            out.status
        )));
    }
    Ok(parse_listlocalsnapshots_output(&String::from_utf8_lossy(
        &out.stdout,
    )))
}

// Keep the unused-import lint quiet on non-macOS targets, where only the pure
// parsing/validation half of this module compiles.
#[cfg(not(target_os = "macos"))]
#[allow(unused_imports)]
use Command as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_validation_is_strict() {
        assert!(is_valid_snapshot_date("2026-07-29-154532"));
        assert!(!is_valid_snapshot_date("2026-07-29-15453")); // short
        assert!(!is_valid_snapshot_date("2026-07-29-1545321")); // long
        assert!(!is_valid_snapshot_date("2026_07-29-154532")); // wrong sep
        assert!(!is_valid_snapshot_date("2026-07-29-15453a")); // non-digit
        assert!(!is_valid_snapshot_date("")); // empty
        assert!(!is_valid_snapshot_date("2026-07-29-154532; rm -rf /")); // injection
    }

    #[test]
    fn name_validation_requires_exact_shape() {
        assert!(is_valid_snapshot_name(
            "com.apple.TimeMachine.2026-07-29-154532.local"
        ));
        assert!(!is_valid_snapshot_name(
            "com.apple.TimeMachine.2026-07-29-154532"
        ));
        assert!(!is_valid_snapshot_name("2026-07-29-154532.local"));
        assert!(!is_valid_snapshot_name(
            "com.evil.TimeMachine.2026-07-29-154532.local"
        ));
        assert!(!is_valid_snapshot_name(
            "com.apple.TimeMachine.2026-07-29-154532.local/../../etc"
        ));
    }

    #[test]
    fn name_and_date_round_trip() {
        let date = "2026-07-29-154532";
        let name = snapshot_name_for_date(date);
        assert_eq!(date_of_snapshot_name(&name), Some(date));
        assert_eq!(date_of_snapshot_name("garbage"), None);
    }

    #[test]
    fn localsnapshot_output_parses_the_documented_shape() {
        let out = "Created local snapshot with date: 2026-07-29-154532\n";
        assert_eq!(
            parse_localsnapshot_output(out).unwrap(),
            "2026-07-29-154532"
        );
    }

    #[test]
    fn localsnapshot_output_tolerates_extra_lines_and_wording_drift() {
        let out = "NOTE: local snapshots are thinned automatically\n\
                   Snapshot created: 2026-01-02-030405\n";
        assert_eq!(
            parse_localsnapshot_output(out).unwrap(),
            "2026-01-02-030405"
        );
    }

    #[test]
    fn localsnapshot_output_without_a_date_is_a_parse_error() {
        let err = parse_localsnapshot_output("Created local snapshot\n").unwrap_err();
        assert!(matches!(err, SnapshotError::Parse(_)));
    }

    #[test]
    fn listlocalsnapshots_output_skips_headers_and_garbage() {
        let out = "Snapshots for volume group containing disk mounted at '/':\n\
                   com.apple.TimeMachine.2026-07-28-101010.local\n\
                   com.apple.TimeMachine.2026-07-29-154532.local\n\
                   some-unrelated-line\n";
        assert_eq!(
            parse_listlocalsnapshots_output(out),
            vec![
                "com.apple.TimeMachine.2026-07-28-101010.local".to_string(),
                "com.apple.TimeMachine.2026-07-29-154532.local".to_string(),
            ]
        );
    }
}
