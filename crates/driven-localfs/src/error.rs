//! Mapping a [`std::io::Error`] onto Driven's SPEC s24 error taxonomy.
//!
//! The pacer, the retry middleware and the per-account circuit breakers all
//! decide what to do next from a [`DriveErrorClassification`]. A local
//! destination produces a completely different set of failures from an HTTP
//! one, and getting the mapping wrong is expensive in both directions:
//!
//! - Classifying a yanked USB stick as [`DriveErrorClassification::Network`]
//!   would spin the retry loop against a device that is not coming back this
//!   cycle, and the user would never see "reconnect your backup drive".
//! - Classifying a flapping NAS mount as fatal would abandon a backup that
//!   would have succeeded thirty seconds later.
//!
//! So the split is by CAUSE, not by severity:
//!
//! | Cause | Classification | Why |
//! |---|---|---|
//! | Destination root gone / not mounted / wrong volume | [`DriveError::DestFolderMissing`] | The user must act (plug the drive in). Maps to `drive.dest_folder_missing`, which the tray already surfaces. |
//! | `EACCES` / `EPERM` / `EROFS` | [`DriveError::DestFolderPermissionDenied`] | Read-only mount or a permissions change; retrying cannot fix it. |
//! | `ENOSPC` / Windows disk-full | [`DriveErrorClassification::StorageQuota`] | The DESTINATION is full. The executor already pauses the account on this and resumes when space appears - exactly the desired behaviour. (`local.disk_full` stays what it has always been: the SOURCE/restore-target disk filling up.) |
//! | `EIO`, `ENXIO`, `ENODEV`, `ESTALE`, `ETIMEDOUT`, `ECONNRESET`, `EHOSTDOWN`, `ENETDOWN`, `ENOTCONN` | [`DriveErrorClassification::Network`] | A transport-shaped fault on the path to the medium: a flaky USB controller or an SMB/NFS mount that dropped. Retryable with backoff. |
//! | `EFBIG` / `EOVERFLOW` / `EINVAL` on a large write | [`DriveErrorClassification::Other`] with a FAT32-specific message | The 4 GiB single-file limit on FAT32. Retrying is pointless; the message must say why. |
//! | anything else | [`DriveErrorClassification::Other`] | Fatal for this op, logged with the raw errno. |
//!
//! One `EIO` is NEVER promoted to `DestFolderMissing`. "The destination is
//! gone" is decided by exactly one check - `crate::config::DestinationMarker`
//! at the root - and never inferred from a per-object failure, so a single bad
//! sector cannot convince Driven the whole drive was unplugged.

use std::io;

use driven_remote::remote_store::DriveErrorClassification;
use driven_remote::DriveError;

/// The FAT32 single-file ceiling: 4 GiB - 1 byte. A write that would cross it
/// is refused up front (with this number in the message) rather than failing
/// opaquely 4 GiB in.
pub const FAT32_MAX_FILE_SIZE: u64 = u32::MAX as u64;

/// Windows `ERROR_HANDLE_DISK_FULL`.
#[cfg(windows)]
const ERROR_HANDLE_DISK_FULL: i32 = 39;
/// Windows `ERROR_DISK_FULL`.
#[cfg(windows)]
const ERROR_DISK_FULL: i32 = 112;
/// Windows `ERROR_NOT_READY` - the drive exists but has no medium.
#[cfg(windows)]
const ERROR_NOT_READY: i32 = 21;
/// Windows `ERROR_DEV_NOT_EXIST` - a disconnected network share.
#[cfg(windows)]
const ERROR_DEV_NOT_EXIST: i32 = 55;

/// Does this error mean the destination filesystem is out of space?
pub fn is_out_of_space(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::StorageFull {
        return true;
    }
    match err.raw_os_error() {
        #[cfg(unix)]
        Some(code) => code == libc::ENOSPC || code == libc::EDQUOT,
        #[cfg(windows)]
        Some(code) => code == ERROR_DISK_FULL || code == ERROR_HANDLE_DISK_FULL,
        #[cfg(not(any(unix, windows)))]
        Some(_) => false,
        None => false,
    }
}

/// Does this error mean the file (or the write) exceeded a filesystem limit -
/// the FAT32 4 GiB ceiling being the one users actually hit?
pub fn is_file_too_large(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::FileTooLarge {
        return true;
    }
    match err.raw_os_error() {
        #[cfg(unix)]
        Some(code) => code == libc::EFBIG || code == libc::EOVERFLOW,
        #[cfg(not(unix))]
        Some(_) => false,
        None => false,
    }
}

/// Does this error mean the operation was refused for permissions reasons
/// (including a read-only mount, which is how a write-protected SD card and a
/// `ro` NFS export both present)?
pub fn is_permission_denied(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::PermissionDenied
        || err.kind() == io::ErrorKind::ReadOnlyFilesystem
    {
        return true;
    }
    match err.raw_os_error() {
        #[cfg(unix)]
        Some(code) => code == libc::EACCES || code == libc::EPERM || code == libc::EROFS,
        #[cfg(not(unix))]
        Some(_) => false,
        None => false,
    }
}

/// Does this error look like a transport fault on the path to the medium - a
/// flaky USB bridge, or a network mount that dropped - rather than a decision
/// the filesystem made?
pub fn is_transient_media_error(err: &io::Error) -> bool {
    match err.raw_os_error() {
        #[cfg(unix)]
        Some(code) => {
            code == libc::EIO
                || code == libc::ENXIO
                || code == libc::ENODEV
                || code == libc::ESTALE
                || code == libc::ETIMEDOUT
                || code == libc::ECONNRESET
                || code == libc::ECONNABORTED
                || code == libc::EHOSTDOWN
                || code == libc::ENETDOWN
                || code == libc::ENOTCONN
                || code == libc::EAGAIN
                || code == libc::EBUSY
        }
        #[cfg(windows)]
        Some(code) => code == ERROR_NOT_READY || code == ERROR_DEV_NOT_EXIST,
        #[cfg(not(any(unix, windows)))]
        Some(_) => false,
        None => matches!(
            err.kind(),
            io::ErrorKind::Interrupted | io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ),
    }
}

/// Classify an I/O failure against the destination.
///
/// `context` is prepended to the error chain so a log line says WHICH operation
/// failed; it is never a filename the user would consider sensitive beyond what
/// the rest of Driven's logs already carry (the relative path).
pub fn classify_io(context: &str, err: io::Error) -> DriveError {
    if is_permission_denied(&err) {
        tracing::warn!(target: crate::TARGET, %context, %err, "destination refused a write for permissions reasons");
        return DriveError::DestFolderPermissionDenied;
    }
    if is_out_of_space(&err) {
        return DriveError::Classified {
            kind: DriveErrorClassification::StorageQuota,
            source: anyhow::anyhow!("{context}: the destination filesystem is out of space: {err}"),
        };
    }
    if is_file_too_large(&err) {
        return DriveError::Classified {
            kind: DriveErrorClassification::Other,
            source: anyhow::anyhow!(
                "{context}: the destination filesystem refused the file as too large \
                 (FAT32 cannot store a single file of 4 GiB or more; reformat the volume \
                 as exFAT to lift the limit): {err}"
            ),
        };
    }
    if is_transient_media_error(&err) {
        return DriveError::Classified {
            kind: DriveErrorClassification::Network,
            source: anyhow::anyhow!("{context}: transient failure reaching the destination: {err}"),
        };
    }
    DriveError::Classified {
        kind: DriveErrorClassification::Other,
        source: anyhow::anyhow!("{context}: {err}"),
    }
}

/// [`classify_io`], wrapped for the `?` operator.
pub fn io_err(context: &str, err: io::Error) -> anyhow::Error {
    anyhow::Error::new(classify_io(context, err))
}

/// The error for "the configured destination is not there": the root directory
/// is missing, or its identity marker is absent or names a different
/// destination.
pub fn dest_missing(reason: &str) -> anyhow::Error {
    tracing::warn!(target: crate::TARGET, %reason, "destination is unavailable");
    anyhow::Error::new(DriveError::DestFolderMissing)
}

/// The error for "this object is not on the destination".
///
/// Kept distinct from a generic I/O failure because `metadata` on a missing
/// object is an ANSWER, not a fault, and callers match on it.
pub fn not_found(id: &str) -> anyhow::Error {
    anyhow::Error::new(DriveError::Classified {
        kind: DriveErrorClassification::Other,
        source: anyhow::anyhow!("localfs.not_found: no object {id:?} on the destination"),
    })
}

/// Is this the error [`not_found`] produces?
pub fn is_not_found(err: &anyhow::Error) -> bool {
    format!("{err:?}").contains("localfs.not_found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_remote::classification_of;

    fn classification(err: io::Error) -> DriveErrorClassification {
        classify_io("test", err).classification()
    }

    #[cfg(unix)]
    #[test]
    fn out_of_space_pauses_the_account_rather_than_retrying() {
        let e = io::Error::from_raw_os_error(libc::ENOSPC);
        assert!(is_out_of_space(&e));
        assert_eq!(classification(e), DriveErrorClassification::StorageQuota);
        // And a quota-exceeded NFS/ext4 export is the same situation.
        let q = io::Error::from_raw_os_error(libc::EDQUOT);
        assert_eq!(classification(q), DriveErrorClassification::StorageQuota);
    }

    #[cfg(unix)]
    #[test]
    fn flapping_media_is_retryable_but_a_readonly_mount_is_not() {
        for code in [
            libc::EIO,
            libc::ESTALE,
            libc::ETIMEDOUT,
            libc::ENODEV,
            libc::ENOTCONN,
        ] {
            assert_eq!(
                classification(io::Error::from_raw_os_error(code)),
                DriveErrorClassification::Network,
                "errno {code} must be retryable"
            );
        }
        for code in [libc::EACCES, libc::EPERM, libc::EROFS] {
            let e = classify_io("test", io::Error::from_raw_os_error(code));
            assert!(
                matches!(e, DriveError::DestFolderPermissionDenied),
                "errno {code} must be a permission refusal, got {e}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_single_io_error_is_never_promoted_to_dest_folder_missing() {
        // The destination is declared missing by exactly one check (the root
        // marker). A bad sector must not be able to fake it, or one unreadable
        // file would stop the whole account with "reconnect your drive".
        let e = classify_io("test", io::Error::from_raw_os_error(libc::EIO));
        assert!(!matches!(e, DriveError::DestFolderMissing), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn the_fat32_size_ceiling_is_fatal_and_says_why() {
        // FAT32 refuses a single file of 4 GiB or more. There is no portable way
        // to know the destination's limit BEFORE writing, so the write runs and
        // the errno is what carries the diagnosis - which means the message has
        // to name the cause and the fix, not just the errno.
        assert_eq!(FAT32_MAX_FILE_SIZE, 4_294_967_295);
        let e = classify_io("test", io::Error::from_raw_os_error(libc::EFBIG));
        assert_eq!(e.classification(), DriveErrorClassification::Other);
        let chain = format!("{:?}", anyhow::Error::new(e));
        assert!(chain.contains("exFAT"), "{chain}");
        assert!(chain.contains("4 GiB"), "{chain}");
    }

    #[test]
    fn not_found_is_recognisable_and_distinct_from_an_io_fault() {
        let e = not_found("a/b.txt");
        assert!(is_not_found(&e));
        assert_eq!(
            classification_of(&e),
            Some(DriveErrorClassification::Other),
            "a missing object is fatal for this op, never a retry"
        );
        assert!(!is_not_found(&anyhow::anyhow!("something else")));
    }

    #[test]
    fn destination_missing_carries_the_spec_s24_code() {
        let e = dest_missing("root gone");
        assert!(e.to_string().contains("drive.dest_folder_missing"), "{e}");
    }

    #[test]
    fn an_unclassified_error_is_fatal_for_this_op_not_silently_retried() {
        let e = io::Error::other("something strange");
        assert_eq!(classification(e), DriveErrorClassification::Other);
    }
}
