//! Crash-safe filesystem primitives.
//!
//! This is a BACKUP DESTINATION. The failure mode that matters is not "the
//! write returned an error" - it is "the write returned success, the machine
//! lost power, and the object on the stick is half of the old file and half of
//! the new one while every index says it is complete". Every write in this
//! crate therefore goes through the same sequence:
//!
//! 1. Write the full content to a temp file in the SAME directory as the
//!    target. Same directory, because `rename(2)` is only atomic within one
//!    filesystem, and "same directory" is the only cheap way to be sure - a
//!    shared top-level temp dir would silently degrade to a copy the moment a
//!    user nests a second mount under their destination.
//! 2. [`sync_file`] the temp file, so its bytes are on the medium.
//! 3. `rename` the temp over the target. A POSIX rename is atomic: a reader
//!    sees either the whole old file or the whole new one, never a mixture.
//! 4. [`sync_dir`] the containing directory, so the DIRECTORY ENTRY that points
//!    at the new inode is durable too. Skipping this is the classic mistake:
//!    the data survives the crash and the name still points at the old inode.
//!
//! ## `fsync` is not enough on macOS
//!
//! `fsync(2)` on macOS returns once the data has reached the drive, NOT once
//! the drive has flushed its own volatile write cache - which is precisely the
//! window that matters when the user yanks a USB stick a second after the
//! backup says it finished. macOS spells the real barrier `F_FULLFSYNC`, and
//! [`sync_file`] uses it, falling back to `fsync` only when the filesystem
//! rejects the fcntl (some network and virtual filesystems return `ENOTSUP`).

use std::fs::File;
use std::io;
use std::path::Path;

/// Flush a file's contents to durable storage.
///
/// On macOS this issues `F_FULLFSYNC`, which additionally forces the DRIVE to
/// flush its own write cache; a plain `fsync` does not, and on removable media
/// that is the difference between "durable" and "durable unless someone unplugs
/// it". `ENOTSUP`/`EINVAL` from the fcntl (network and some virtual
/// filesystems) falls back to `fsync`, which is the strongest barrier those
/// filesystems offer.
pub fn sync_file(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `fd` is owned by `file` and valid for the duration of the
        // call; `F_FULLFSYNC` takes no argument.
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
        if rc != -1 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENOTSUP) | Some(libc::EINVAL) | Some(libc::EPERM) => {
                // The filesystem does not implement the full barrier; fsync is
                // the best it offers.
            }
            _ => return Err(err),
        }
    }
    file.sync_all()
}

/// Flush a DIRECTORY's entries to durable storage, so a `rename` into it
/// survives a crash.
///
/// A no-op on Windows, where directories cannot be opened as files and the
/// rename's metadata durability is the filesystem's own business.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let handle = File::open(dir)?;
        // A directory handle cannot be F_FULLFSYNC'd on every filesystem, and
        // the barrier that matters for the data file was already issued; this
        // only has to push the directory entry out.
        match handle.sync_all() {
            Ok(()) => Ok(()),
            // Some filesystems (notably exFAT/msdos on macOS) reject fsync on a
            // directory handle outright. There is no stronger primitive to fall
            // back to there, and the alternative - refusing to back up to a FAT
            // stick at all - is worse.
            Err(e) if matches!(e.raw_os_error(), Some(libc::EINVAL) | Some(libc::ENOTSUP)) => {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Atomically replace `target` with the already-written, already-synced
/// `temp`, then make the directory entry durable.
///
/// The caller MUST have called [`sync_file`] on `temp` first: renaming an
/// unsynced file publishes a name that may point at zeroes after a crash.
pub fn commit_rename(temp: &Path, target: &Path) -> io::Result<()> {
    std::fs::rename(temp, target)?;
    if let Some(parent) = target.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Write `bytes` to `path` durably: temp file in the same directory, sync,
/// atomic rename, directory sync.
pub fn write_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a destination path must have a parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(temp_name());
    // Scoped so the handle is closed before the rename: on Windows a rename
    // over an open handle fails, and there is no reason to hold it anywhere.
    let result = (|| -> io::Result<()> {
        let mut f = File::create(&temp)?;
        f.write_all(bytes)?;
        f.flush()?;
        sync_file(&f)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }
    if let Err(e) = commit_rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }
    Ok(())
}

/// A fresh temp filename. Reserved by [`crate::names::encode`], so it can never
/// collide with an encoded user filename.
pub fn temp_name() -> String {
    format!("{}{}", crate::names::TMP_PREFIX, uuid::Uuid::new_v4())
}

/// Remove a file, treating "already gone" as success.
pub fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Total and available bytes on the filesystem holding `path`.
///
/// Returns `None` when the platform query fails; [`crate::store::LocalFsStore`]
/// then reports an unknown capacity rather than a wrong one.
#[cfg(unix)]
pub fn volume_capacity(path: &Path) -> Option<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `stat` is a valid, correctly-sized out-parameter and `c` is a
    // NUL-terminated path that outlives the call.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    // `f_frsize` is the fragment size the block counts are expressed in;
    // `f_bsize` is only the preferred I/O size and is wrong here on several
    // platforms. Fall back to it only when `f_frsize` is unset.
    let unit = if stat.f_frsize > 0 {
        stat.f_frsize as u64
    } else {
        stat.f_bsize as u64
    };
    let total = (stat.f_blocks as u64).checked_mul(unit)?;
    // `f_bavail` (available to a NON-root process) rather than `f_bfree`, which
    // includes the reserved-for-root pool Driven can never use.
    let available = (stat.f_bavail as u64).checked_mul(unit)?;
    Some((total, available))
}

/// Total and available bytes on the filesystem holding `path`.
///
/// Always `None` on Windows: deliberately unimplemented rather than guessed.
/// `about()` then reports an unknown capacity, which the UI renders as
/// "unknown", and nothing in the sync engine depends on the figure - so a wrong
/// number would be strictly worse than no number.
#[cfg(windows)]
pub fn volume_capacity(path: &Path) -> Option<(u64, u64)> {
    let _ = path;
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_durable_publishes_the_whole_file_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("obj.bin");
        write_durable(&target, b"hello world").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hello world");

        // Overwrite; the replacement must be complete, not appended-to.
        write_durable(&target, b"hi").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"hi");

        // No temp files survive a successful write.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(crate::names::TMP_PREFIX))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn write_durable_creates_missing_parents() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a").join("b").join("c.bin");
        write_durable(&target, b"x").unwrap();
        assert!(target.exists());
    }

    #[test]
    fn remove_if_present_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("gone.bin");
        remove_if_present(&p).unwrap();
        std::fs::write(&p, b"x").unwrap();
        remove_if_present(&p).unwrap();
        remove_if_present(&p).unwrap();
        assert!(!p.exists());
    }

    #[test]
    fn temp_names_are_reserved_and_unique() {
        let a = temp_name();
        let b = temp_name();
        assert_ne!(a, b);
        assert!(crate::names::is_reserved_control_name(&a));
    }

    #[test]
    fn volume_capacity_reports_something_plausible_for_a_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        if let Some((total, avail)) = volume_capacity(dir.path()) {
            assert!(total > 0, "a mounted filesystem has a non-zero size");
            assert!(avail <= total);
        }
    }
}
