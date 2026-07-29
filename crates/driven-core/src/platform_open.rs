//! The one place Driven opens a local file for READING (DESIGN s5.3
//! "Open-file handling").
//!
//! Two things every read-open in Driven must get right, and which are easy to
//! get wrong one call site at a time:
//!
//! 1. **Windows sharing.** DESIGN s5.3 opens by naming the failure mode: Google
//!    Drive Desktop "opens files without `FILE_SHARE_DELETE` and with default
//!    Windows share modes that block other writers", so Word / Excel /
//!    Photoshop cannot save over, rename, or delete a file the backup client is
//!    reading. Driven's contract is the opposite - `FILE_SHARE_READ |
//!    FILE_SHARE_WRITE | FILE_SHARE_DELETE`. Rust's `std::fs::File::open`
//!    happens to default to exactly those three today, but that is a std
//!    implementation detail rather than something Driven asserts; stating the
//!    share mode explicitly, in one place, pins the DESIGN s5.3 contract to
//!    Driven's own code instead of leaving it resting on a default.
//! 2. **The SPEC s22 `io_priority` hint.** On Windows this MUST ride on the
//!    HANDLE: `WorkPriority::Low` demotes only CPU priority at the thread level
//!    (see [`crate::priority`]), and the upload pipeline's reads are performed
//!    by tokio blocking-pool threads Driven has no handle on, so a thread-scoped
//!    lever cannot reach them either. Applying it here means a read is shaped by
//!    `io_priority` no matter which subsystem opened it.
//!
//! Off Windows there is no per-descriptor I/O priority and the default open
//! already permits an unlink/replace under an open handle, so the whole
//! platform split lives here rather than at each call site.

use std::fs::File;
use std::path::Path;

use crate::priority::WorkPriority;

/// Open `path` for read with Driven's DESIGN s5.3 share mode and the SPEC s22
/// `io_priority` hint applied to the returned handle.
///
/// Blocking - call it from a blocking context (a scanner walk worker, a
/// `spawn_blocking` task) or wrap the result for async use as
/// [`crate::executor`]'s open path does.
///
/// Errors are the raw [`std::io::Error`] from the open so each caller applies
/// its own classification (the executor distinguishes a Windows sharing
/// violation from an ACL denial to decide whether VSS can help; the scanner only
/// needs "could not verify").
pub(crate) fn open_read_shared(path: &Path, priority: WorkPriority) -> std::io::Result<File> {
    let file = platform_open(path)?;
    // Best-effort, and a no-op off Windows or at `Normal`. The access mask stays
    // read-only: a read-only handle is enough for this hint class, so setting it
    // never widens the open and never changes its locking behaviour.
    crate::priority::apply_to_file_handle(&file, priority);
    Ok(file)
}

#[cfg(windows)]
fn platform_open(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_MODE)
        .open(path)
}

#[cfg(not(windows))]
fn platform_open(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

/// `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`.
///
/// Spelled out numerically rather than pulled from a Win32 binding because
/// these three bits are stable ABI and `driven-core` deliberately carries no
/// `windows` crate dependency (the same approach [`crate::priority`] and the
/// scanner's `FindFirstStreamW` ADS probe take).
#[cfg(windows)]
const SHARE_MODE: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// The helper opens and reads normally on every platform.
    #[test]
    fn opens_and_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"hello-platform-open").unwrap();

        let mut f = open_read_shared(&path, WorkPriority::Normal).unwrap();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"hello-platform-open");
    }

    /// A missing path surfaces the raw io error for the caller to classify -
    /// the helper must not swallow or rewrap it.
    #[test]
    fn missing_path_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_read_shared(&dir.path().join("nope.txt"), WorkPriority::Normal).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// A non-`Normal` priority must never change whether the open succeeds:
    /// the hint is best-effort and is not allowed to become load-bearing.
    #[test]
    fn priority_hint_does_not_affect_the_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.txt");
        std::fs::write(&path, b"x").unwrap();
        assert!(open_read_shared(&path, WorkPriority::Idle).is_ok());
        assert!(open_read_shared(&path, WorkPriority::Low).is_ok());
    }

    /// DESIGN s5.3, the property the explicit share mode exists to guarantee:
    /// another writer must still be able to RENAME (an editor's atomic save)
    /// and DELETE the file while Driven holds a read handle on it.
    ///
    /// Windows-only because the behaviour is Windows-only - on Unix the default
    /// open already permits an unlink under an open handle, so the assertion
    /// would pass vacuously. Runs in the repo's `windows-latest` CI job.
    #[cfg(windows)]
    #[test]
    fn windows_open_allows_rename_and_delete_while_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held.txt");
        let renamed = dir.path().join("held-renamed.txt");
        std::fs::write(&path, b"in-use").unwrap();

        let handle = open_read_shared(&path, WorkPriority::Normal).unwrap();

        std::fs::rename(&path, &renamed).expect(
            "FILE_SHARE_DELETE must let another writer rename the file we are reading (DESIGN s5.3)",
        );
        std::fs::remove_file(&renamed).expect(
            "FILE_SHARE_DELETE must let another writer delete the file we are reading (DESIGN s5.3)",
        );

        drop(handle);
    }

    /// The share mode is asserted as a VALUE, not just via its behaviour: the
    /// rename/delete test above would still pass if the three flags were
    /// dropped, because Rust's std currently defaults to the same three. This
    /// pins DESIGN s5.3's contract to Driven's own constant so a future std
    /// default change cannot silently take the guarantee away.
    #[cfg(windows)]
    #[test]
    fn windows_share_mode_is_read_write_delete() {
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;
        assert_eq!(
            SHARE_MODE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
        );
    }
}
