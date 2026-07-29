//! The local-folder destination's configuration, and the marker that proves a
//! directory really IS that destination.
//!
//! ## What lives where
//!
//! - [`LocalFsConfig`] - the destination root path plus the destination's
//!   identity UUID. Persisted as JSON in `accounts.backend_config_json`.
//! - There is NO credential. A directory the user already has write access to
//!   needs no key, so unlike the Drive and S3 backends this one touches the OS
//!   keychain not at all.
//!
//! ## The mount-point marker (read this before removing it)
//!
//! "Is my backup drive plugged in?" cannot be answered by `root.exists()`.
//!
//! - On macOS an ejected volume's `/Volumes/Backup` disappears, so existence
//!   almost works.
//! - On Linux (and for every NAS mount everywhere) an UNMOUNTED mount point is
//!   still a perfectly ordinary empty directory on the boot disk. `exists()`
//!   returns true, Driven writes a full backup into `/mnt/backup` on the root
//!   filesystem, the admin remounts the NAS, and the entire backup silently
//!   disappears behind the mount - while `file_state` still says every file is
//!   synced. That is total, silent backup loss, and it is the single most
//!   likely way this backend can hurt someone.
//! - The same class of bug covers "the user plugged in a DIFFERENT stick that
//!   happens to mount at the same path".
//!
//! So account creation writes a [`DestinationMarker`] at the root carrying a
//! UUID, and EVERY operation re-reads it first. Absent or different means
//! [`driven_remote::DriveError::DestFolderMissing`] - "reconnect your backup
//! drive" - and nothing is written.

use serde::{Deserialize, Serialize};

use crate::names::MARKER_FILE;

/// The marker file written at the root of a local-folder destination.
///
/// Deliberately human-readable JSON with a comment-ish `note` field: a user who
/// finds it on their stick should be able to tell what it is and that deleting
/// it will make Driven refuse to use the drive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DestinationMarker {
    /// Stored-format version of the marker itself.
    pub version: u32,
    /// The destination's identity. Matched against
    /// [`LocalFsConfig::destination_id`] on every operation.
    pub destination_id: String,
    /// Unix epoch ms the destination was initialized.
    pub created_at_ms: i64,
    /// A human explanation, for whoever finds this file on a USB stick.
    pub note: String,
}

/// Is `c` a trailing path separator to trim from a destination root?
///
/// Windows accepts BOTH `\` and `/` in a path, and a user who typed
/// `D:/Backups/` should get the same stored root as one who typed `D:\Backups`.
/// On unix only `/` separates - a backslash is an ordinary, legal filename
/// character there, and trimming it would silently rename the destination.
#[cfg(windows)]
fn is_trailing_separator(c: char) -> bool {
    c == '\\' || c == '/'
}

/// Is `c` a trailing path separator to trim from a destination root?
#[cfg(not(windows))]
fn is_trailing_separator(c: char) -> bool {
    c == '/'
}

/// The marker text a user sees if they open the file.
const MARKER_NOTE: &str =
    "This folder is a Driven backup destination. Driven refuses to write here \
if this file is missing, which is how it tells an unplugged drive apart from an empty mount point. \
Do not delete it.";

impl DestinationMarker {
    /// A fresh marker for a new destination.
    pub fn new(destination_id: &str, now_ms: i64) -> Self {
        Self {
            version: 1,
            destination_id: destination_id.to_string(),
            created_at_ms: now_ms,
            note: MARKER_NOTE.to_string(),
        }
    }
}

/// A validation failure in a [`LocalFsConfig`] the user supplied.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LocalFsConfigError {
    /// The root path was empty.
    #[error("localfs.config_invalid: choose a destination folder")]
    EmptyRoot,
    /// The root path was relative. A relative destination would resolve against
    /// whatever the app's working directory happened to be at the time, which
    /// is not a property a backup destination may have.
    #[error("localfs.config_invalid: the destination folder must be an absolute path")]
    RelativeRoot,
    /// The root path contained a NUL, which no filesystem accepts and which
    /// truncates a C string.
    #[error("localfs.config_invalid: the destination folder path is not a valid path")]
    MalformedRoot,
    /// The destination id was empty.
    #[error("localfs.config_invalid: the destination is missing its identity")]
    MissingDestinationId,
}

/// Configuration for a local / removable-folder destination.
///
/// Serialized into `accounts.backend_config_json`. Field names are part of the
/// stored format (v1.0.0 stability) - add fields with `#[serde(default)]`, never
/// rename one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalFsConfig {
    /// Absolute path of the destination directory: a USB drive's mount point, a
    /// NAS share, or any plain folder.
    pub root: String,
    /// The identity written into the root's [`DestinationMarker`]. A root whose
    /// marker is absent or carries a different id is NOT this destination.
    pub destination_id: String,
}

impl LocalFsConfig {
    /// Validate and NORMALIZE user input into a config safe to persist.
    ///
    /// Normalization is part of validation so the stored value is canonical and
    /// every later use can assume it: the root loses surrounding whitespace and
    /// any trailing separator (except a bare `/`).
    ///
    /// Deliberately does NOT canonicalize symlinks: a destination is routinely
    /// unplugged, and `canonicalize` on a missing path fails. The marker check
    /// is what proves the path is the right destination, not its spelling.
    pub fn normalized(mut self) -> Result<Self, LocalFsConfigError> {
        let root = self.root.trim().to_string();
        if root.is_empty() {
            return Err(LocalFsConfigError::EmptyRoot);
        }
        if root.contains('\0') {
            return Err(LocalFsConfigError::MalformedRoot);
        }
        let path = std::path::Path::new(&root);
        if !path.is_absolute() {
            return Err(LocalFsConfigError::RelativeRoot);
        }
        // Trim a trailing separator so two configs for the same folder compare
        // equal - but ONLY while the result is still an absolute path. Two roots
        // would otherwise be destroyed by the trim:
        //
        // - POSIX `/` becomes the empty string;
        // - Windows `D:\` becomes `D:`, which is not the drive's root at all but
        //   the DRIVE-RELATIVE "current directory on D:" - so a user who picked
        //   the whole stick would have had their backups written whereever that
        //   process's cwd on D: happened to point.
        //
        // Re-checking `is_absolute` after the trim covers both without special
        // casing either.
        let trimmed = root.trim_end_matches(is_trailing_separator);
        if !trimmed.is_empty() && std::path::Path::new(trimmed).is_absolute() {
            self.root = trimmed.to_string();
        } else {
            self.root = root.clone();
        }

        let id = self.destination_id.trim().to_string();
        if id.is_empty() {
            return Err(LocalFsConfigError::MissingDestinationId);
        }
        self.destination_id = id;

        Ok(self)
    }

    /// The destination root as a [`std::path::PathBuf`].
    pub fn root_path(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.root)
    }

    /// Path of the destination's identity marker.
    pub fn marker_path(&self) -> std::path::PathBuf {
        self.root_path().join(MARKER_FILE)
    }

    /// Parse an `accounts.backend_config_json` blob, validating + normalizing it.
    pub fn from_json(json: &str) -> anyhow::Result<Self> {
        let cfg: LocalFsConfig = serde_json::from_str(json).map_err(|e| {
            anyhow::anyhow!("localfs.config_invalid: could not parse backend config: {e}")
        })?;
        Ok(cfg.normalized()?)
    }

    /// Render to the `accounts.backend_config_json` blob.
    pub fn to_json(&self) -> anyhow::Result<String> {
        serde_json::to_string(self).map_err(|e| {
            anyhow::anyhow!("localfs.config_invalid: could not serialize backend config: {e}")
        })
    }
}

/// The outcome of preparing a directory to be a destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreparedDestination {
    /// The directory had no marker and one was written: a fresh destination.
    Initialized,
    /// The directory already carried a marker; its id was ADOPTED rather than
    /// overwritten, so re-adding a drive that already holds a Driven backup
    /// keeps working with the objects already on it.
    Adopted,
}

/// Validate that `root` can serve as a destination, and make sure it carries a
/// [`DestinationMarker`].
///
/// Called once, at account-creation time, from the setup command - not on the
/// hot path. Proves four things a user would otherwise discover at 2am:
///
/// 1. The path exists and is a DIRECTORY (not a file, not a dangling symlink).
/// 2. Driven can actually WRITE there - by writing, syncing and deleting a real
///    file, because a read-only mount, an ACL, and a full disk are all invisible
///    to a permissions bit.
/// 3. Whether the folder is already a Driven destination, in which case its
///    existing identity is adopted instead of being stamped over (which would
///    orphan every object already on the drive).
/// 4. The marker is durable before the account row exists.
///
/// Returns the destination id to persist in [`LocalFsConfig::destination_id`].
pub fn prepare_destination(
    root: &std::path::Path,
    now_ms: i64,
) -> anyhow::Result<(String, PreparedDestination)> {
    let meta = std::fs::metadata(root).map_err(|e| {
        anyhow::anyhow!(
            "localfs.dest_unavailable: cannot read {}: {e}",
            root.display()
        )
    })?;
    if !meta.is_dir() {
        anyhow::bail!(
            "localfs.dest_not_a_directory: {} is not a folder",
            root.display()
        );
    }

    // Adopt an existing destination rather than re-stamping it: a user
    // re-adding a drive that already holds their backups must keep the objects
    // on it, and a new id would make every one of them invisible.
    let marker_path = root.join(MARKER_FILE);
    if let Ok(raw) = std::fs::read_to_string(&marker_path) {
        if let Ok(existing) = serde_json::from_str::<DestinationMarker>(&raw) {
            if !existing.destination_id.trim().is_empty() {
                probe_writable(root)?;
                return Ok((existing.destination_id, PreparedDestination::Adopted));
            }
        }
        tracing::warn!(
            target: crate::TARGET,
            path = %marker_path.display(),
            "the destination marker is unreadable; re-initializing it"
        );
    }

    probe_writable(root)?;
    let destination_id = uuid::Uuid::new_v4().to_string();
    let marker = DestinationMarker::new(&destination_id, now_ms);
    let json = serde_json::to_vec_pretty(&marker)
        .map_err(|e| anyhow::anyhow!("localfs.config_invalid: {e}"))?;
    crate::fsx::write_durable(&marker_path, &json).map_err(|e| {
        anyhow::anyhow!(
            "localfs.dest_not_writable: could not write the destination marker to {}: {e}",
            marker_path.display()
        )
    })?;
    Ok((destination_id, PreparedDestination::Initialized))
}

/// Prove the directory is writable by actually writing (and syncing, and
/// removing) a file.
///
/// A permissions-bit check is not enough: a read-only mount, a restrictive ACL,
/// an immutable flag and a full filesystem all pass it and then fail the first
/// real write - at which point the user has an account that looks configured
/// and never backs anything up.
fn probe_writable(root: &std::path::Path) -> anyhow::Result<()> {
    let probe = root.join(crate::fsx::temp_name());
    crate::fsx::write_durable(&probe, b"driven writability probe").map_err(|e| {
        anyhow::anyhow!(
            "localfs.dest_not_writable: {} is not writable: {e}",
            root.display()
        )
    })?;
    if let Err(e) = std::fs::remove_file(&probe) {
        tracing::warn!(
            target: crate::TARGET,
            path = %probe.display(),
            %e,
            "could not remove the writability probe file"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(root: &str) -> LocalFsConfig {
        LocalFsConfig {
            root: root.to_string(),
            destination_id: "dest-1".to_string(),
        }
    }

    /// A destination root for the platform under test, and the shapes that must
    /// be refused on it.
    ///
    /// Platform-AWARE rather than `#[cfg(unix)]`-gated: a Windows user picking
    /// `D:\Backups` is the normal case for this backend, and gating these tests
    /// would leave the whole Windows path unasserted.
    #[cfg(windows)]
    const ABS: &str = r"D:\Backups";
    #[cfg(windows)]
    const ABS_TRAILING: &str = r"D:\Backups\";
    /// The volume root: the trim must NOT reduce it to the drive-relative `D:`.
    #[cfg(windows)]
    const VOLUME_ROOT: &str = r"D:\";
    #[cfg(windows)]
    const RELATIVE: &[&str] = &["backups", "../backups", r"\Backups", r"D:Backups"];

    #[cfg(not(windows))]
    const ABS: &str = "/Volumes/Backup";
    #[cfg(not(windows))]
    const ABS_TRAILING: &str = "/Volumes/Backup/";
    /// The filesystem root: the trim must NOT reduce it to the empty string.
    #[cfg(not(windows))]
    const VOLUME_ROOT: &str = "/";
    #[cfg(not(windows))]
    const RELATIVE: &[&str] = &["backups", "../backups", r"D:\Backups"];

    #[test]
    fn normalization_trims_whitespace_and_a_trailing_separator() {
        let c = cfg(&format!("  {ABS_TRAILING}  ")).normalized().unwrap();
        assert_eq!(c.root, ABS);
        assert_eq!(cfg(ABS).normalized().unwrap().root, ABS);
    }

    #[test]
    fn trimming_never_turns_a_volume_root_into_a_relative_path() {
        // POSIX `/` would become the empty string; Windows `D:\` would become
        // `D:`, which is not the drive's root but the DRIVE-RELATIVE current
        // directory on D: - so a user who picked the whole stick would have had
        // their backups written wherever that process's cwd on D: pointed.
        let c = cfg(VOLUME_ROOT).normalized().unwrap();
        assert_eq!(c.root, VOLUME_ROOT);
        assert!(
            std::path::Path::new(&c.root).is_absolute(),
            "the normalized root must stay absolute, got {:?}",
            c.root
        );
    }

    #[test]
    fn relative_and_empty_roots_are_rejected() {
        assert_eq!(
            cfg("").normalized().unwrap_err(),
            LocalFsConfigError::EmptyRoot
        );
        assert_eq!(
            cfg("   ").normalized().unwrap_err(),
            LocalFsConfigError::EmptyRoot
        );
        for bad in RELATIVE {
            assert_eq!(
                cfg(bad).normalized().unwrap_err(),
                LocalFsConfigError::RelativeRoot,
                "{bad:?} must be refused as relative"
            );
        }
        assert_eq!(
            cfg(&format!("{ABS}\0sub")).normalized().unwrap_err(),
            LocalFsConfigError::MalformedRoot
        );
    }

    /// A UNC share is how a Windows user names a NAS - one of the destinations
    /// this backend exists for.
    #[cfg(windows)]
    #[test]
    fn a_unc_share_normalizes_like_any_other_absolute_root() {
        let c = cfg(r"  \\server\share\Backups\  ").normalized().unwrap();
        assert_eq!(c.root, r"\\server\share\Backups");
        // The share root itself must survive the trim.
        let c = cfg(r"\\server\share").normalized().unwrap();
        assert!(std::path::Path::new(&c.root).is_absolute(), "{:?}", c.root);
    }

    #[test]
    fn a_config_without_a_destination_id_is_rejected() {
        let c = LocalFsConfig {
            root: ABS.to_string(),
            destination_id: "  ".to_string(),
        };
        assert_eq!(
            c.normalized().unwrap_err(),
            LocalFsConfigError::MissingDestinationId
        );
    }

    #[test]
    fn json_round_trips() {
        let c = cfg(ABS).normalized().unwrap();
        let json = c.to_json().unwrap();
        assert_eq!(LocalFsConfig::from_json(&json).unwrap(), c);
    }

    #[test]
    fn preparing_a_fresh_directory_writes_a_marker_and_a_second_call_adopts_it() {
        let dir = tempfile::tempdir().unwrap();
        let (id, outcome) = prepare_destination(dir.path(), 1_700_000_000_000).unwrap();
        assert_eq!(outcome, PreparedDestination::Initialized);
        assert!(!id.is_empty());
        assert!(dir.path().join(MARKER_FILE).exists());

        // Re-adding the same drive must ADOPT, never re-stamp: a new id would
        // orphan every object already on it.
        let (id2, outcome2) = prepare_destination(dir.path(), 1_700_000_001_000).unwrap();
        assert_eq!(id2, id);
        assert_eq!(outcome2, PreparedDestination::Adopted);
    }

    #[test]
    fn preparing_a_file_or_a_missing_path_fails() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let err = prepare_destination(&file, 0).unwrap_err().to_string();
        assert!(err.contains("localfs.dest_not_a_directory"), "{err}");

        let missing = dir.path().join("nope");
        let err = prepare_destination(&missing, 0).unwrap_err().to_string();
        assert!(err.contains("localfs.dest_unavailable"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_directory_is_rejected_at_setup_time() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = prepare_destination(&ro, 0);

        // Restore write permission so the TempDir can clean itself up.
        let _ = std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700));

        // Running as root defeats the mode bits entirely; skip rather than
        // assert a falsehood.
        if nix_is_root() {
            return;
        }
        let err = result.unwrap_err().to_string();
        assert!(err.contains("localfs.dest_not_writable"), "{err}");
    }

    #[cfg(unix)]
    fn nix_is_root() -> bool {
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn a_corrupt_marker_is_re_initialized_rather_than_wedging_setup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MARKER_FILE), b"{ not json").unwrap();
        let (id, outcome) = prepare_destination(dir.path(), 0).unwrap();
        assert!(!id.is_empty());
        assert_eq!(outcome, PreparedDestination::Initialized);
    }
}
