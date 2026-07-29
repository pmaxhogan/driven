//! Firmlink-aware mapping from a live file path to its path inside a mounted
//! snapshot of the file's volume.
//!
//! On modern macOS the boot disk is a volume group: the sealed System volume
//! is mounted at `/` and the writable Data volume at `/System/Volumes/Data`,
//! with firmlinks stitching user-visible paths (`/Users`, `/private/var`,
//! ...) from `/` into the Data volume. `statfs("/Users/me/f")` therefore
//! reports `f_mntonname = /System/Volumes/Data` even though the path the app
//! walks does NOT start with that mount point. Mapping must handle both:
//!
//! - **Direct**: `live` starts with the volume mount (`/System/Volumes/Data/x`
//!   or a plain external volume `/Volumes/USB/x`) - strip the prefix.
//! - **Firmlinked**: `live` (`/Users/me/f`) reaches the Data volume through a
//!   firmlink - the on-volume path is `<mount><live>` and must be VERIFIED to
//!   be the same file (dev+ino) before trusting it, so an exotic mount layout
//!   degrades to skip rather than reading the wrong file.
//!
//! The candidate DECISION is pure and unit-tested cross-OS; only the
//! statfs/stat verification is macOS-gated.

use std::path::{Path, PathBuf};

/// How a live path maps onto its volume, before any filesystem verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapCandidate {
    /// `live` starts with the volume mount point; the payload is the
    /// volume-relative remainder.
    Direct(PathBuf),
    /// `live` does not start with the mount point (firmlink shape). The
    /// payload is (`on_volume_absolute`, `volume_relative`): the caller must
    /// verify `on_volume_absolute` is the SAME FILE as `live` (dev+ino)
    /// before using the relative path.
    Firmlink(PathBuf, PathBuf),
}

/// Compute the mapping candidate for `live` on the volume mounted at
/// `volume_mount`. Pure; no filesystem access. Returns `None` for relative
/// inputs (the executor only ever hands us absolute paths - a relative one is
/// a caller bug we refuse to guess about).
pub fn map_candidate(live: &Path, volume_mount: &Path) -> Option<MapCandidate> {
    if !live.is_absolute() || !volume_mount.is_absolute() {
        return None;
    }
    if let Ok(rel) = live.strip_prefix(volume_mount) {
        return Some(MapCandidate::Direct(rel.to_path_buf()));
    }
    // Firmlink shape: the on-volume path is `<mount>/<live-minus-root>`.
    let rel = live.strip_prefix("/").ok()?.to_path_buf();
    Some(MapCandidate::Firmlink(volume_mount.join(&rel), rel))
}

/// Join a verified volume-relative path under the snapshot mountpoint.
pub fn snapshot_path(mountpoint: &Path, volume_relative: &Path) -> PathBuf {
    mountpoint.join(volume_relative)
}

/// Resolve the mount point of the volume hosting `path` via `statfs(2)`
/// (`f_mntonname`).
#[cfg(target_os = "macos")]
pub fn resolve_volume_mount(path: &Path) -> std::io::Result<PathBuf> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;

    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    // SAFETY: zeroed statfs is a valid out-param; statfs fills it on success.
    let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path, `sfs` a valid out pointer.
    let rc = unsafe { libc::statfs(c.as_ptr(), &mut sfs) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the kernel NUL-terminates f_mntonname (MNAMELEN buffer).
    let mnt = unsafe { CStr::from_ptr(sfs.f_mntonname.as_ptr()) };
    Ok(PathBuf::from(
        std::ffi::OsStr::from_bytes(mnt.to_bytes()).to_os_string(),
    ))
}

/// Whether two paths name the SAME file (`st_dev` + `st_ino` equal). Used to
/// verify a firmlink candidate before trusting it. Any stat failure reads as
/// "not the same" so verification failures degrade to skip.
#[cfg(target_os = "macos")]
pub fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => false,
    }
}

/// Full live-path -> snapshot-path resolution for one file: compute the
/// candidate, verify a firmlink candidate names the same file, and require
/// the mapped path to EXIST inside the snapshot (the file predates the
/// snapshot). `None` = mapping cannot be trusted; the caller degrades to
/// skip-the-locked-file.
#[cfg(target_os = "macos")]
pub fn snapshot_path_for(live: &Path, volume_mount: &Path, mountpoint: &Path) -> Option<PathBuf> {
    let rel = match map_candidate(live, volume_mount)? {
        MapCandidate::Direct(rel) => rel,
        MapCandidate::Firmlink(on_volume, rel) => {
            if !same_file(live, &on_volume) {
                return None;
            }
            rel
        }
    };
    let mapped = snapshot_path(mountpoint, &rel);
    if mapped.symlink_metadata().is_ok() {
        Some(mapped)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_mapping_strips_the_mount_prefix() {
        let c = map_candidate(
            Path::new("/System/Volumes/Data/Users/me/f.txt"),
            Path::new("/System/Volumes/Data"),
        );
        assert_eq!(
            c,
            Some(MapCandidate::Direct(PathBuf::from("Users/me/f.txt")))
        );
    }

    #[test]
    fn external_volume_maps_directly() {
        let c = map_candidate(Path::new("/Volumes/USB/dir/f"), Path::new("/Volumes/USB"));
        assert_eq!(c, Some(MapCandidate::Direct(PathBuf::from("dir/f"))));
    }

    #[test]
    fn firmlink_shape_produces_a_verify_candidate() {
        let c = map_candidate(
            Path::new("/Users/me/f.txt"),
            Path::new("/System/Volumes/Data"),
        );
        assert_eq!(
            c,
            Some(MapCandidate::Firmlink(
                PathBuf::from("/System/Volumes/Data/Users/me/f.txt"),
                PathBuf::from("Users/me/f.txt"),
            ))
        );
    }

    #[test]
    fn root_mount_is_a_direct_map() {
        let c = map_candidate(Path::new("/etc/hosts"), Path::new("/"));
        assert_eq!(c, Some(MapCandidate::Direct(PathBuf::from("etc/hosts"))));
    }

    #[test]
    fn relative_inputs_are_refused() {
        assert_eq!(map_candidate(Path::new("x/y"), Path::new("/")), None);
        assert_eq!(map_candidate(Path::new("/x"), Path::new("rel")), None);
    }

    #[test]
    fn snapshot_path_joins_under_the_mountpoint() {
        assert_eq!(
            snapshot_path(
                Path::new("/private/var/run/da-1/m0"),
                Path::new("Users/me/f")
            ),
            PathBuf::from("/private/var/run/da-1/m0/Users/me/f")
        );
    }
}
