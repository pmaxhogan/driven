//! Boundary input validation for the elevated helper (DESIGN s5.3.1).
//!
//! The un-elevated app is UNTRUSTED: every `OpenLocked` request naming a volume
//! and a path is re-validated here before any COM call, so a buggy or
//! compromised caller can only ever drive the helper to read files the user
//! already configured Driven to back up - never `C:\Windows\...`, another
//! user's profile, or an arbitrary system path.
//!
//! Everything in this module is pure (no I/O) so the rules are unit-tested on
//! every OS. The Windows server pairs these lexical checks with a
//! canonicalisation pass (resolving symlinks in the directory chain) before
//! re-running [`check_within_roots`] on the canonical path - defence in depth
//! against a symlinked directory escaping a configured root.

use std::path::{Path, PathBuf};

/// Why an `OpenLocked` request was rejected at the boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidateError {
    /// The `volume` field did not normalise to a real `X:` drive.
    #[error("invalid volume")]
    InvalidVolume,
    /// The `live_path` was not an absolute path with a drive prefix.
    #[error("path is not absolute")]
    NotAbsolute,
    /// The path contained a `..` component (traversal).
    #[error("path contains a parent-directory traversal")]
    Traversal,
    /// The path's drive letter did not match the requested volume.
    #[error("path is not on the requested volume")]
    VolumeMismatch,
    /// The path did not resolve under any configured Driven source root.
    #[error("path is outside the configured backup roots")]
    OutsideRoots,
    /// The path was empty or absurdly long.
    #[error("path is empty or too long")]
    BadLength,
}

impl ValidateError {
    /// A stable machine token for the wire [`crate::protocol::Control::Error`].
    pub fn code(&self) -> &'static str {
        match self {
            ValidateError::InvalidVolume | ValidateError::VolumeMismatch => "invalid_volume",
            ValidateError::NotAbsolute | ValidateError::Traversal | ValidateError::BadLength => {
                "invalid_request"
            }
            ValidateError::OutsideRoots => "not_allowed",
        }
    }
}

/// The largest `live_path` the helper will consider (bytes of the string). Well
/// beyond any real Windows path (which tops out near 32K wide chars) yet bounds
/// a pathological input.
pub const MAX_PATH_LEN: usize = 32 * 1024;

/// Normalise a `"C:"` / `"C:\\"` / `"c"` volume spec to canonical `"C:"`
/// (uppercased). Returns `None` for anything that is not a single drive letter.
pub fn normalize_volume(input: &str) -> Option<String> {
    let trimmed = input.trim().trim_end_matches(['\\', '/']);
    let mut chars = trimmed.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(c), None, None) if c.is_ascii_alphabetic() => {
            Some(format!("{}:", c.to_ascii_uppercase()))
        }
        (Some(c), Some(':'), None) if c.is_ascii_alphabetic() => {
            Some(format!("{}:", c.to_ascii_uppercase()))
        }
        _ => None,
    }
}

/// The `"C:"` drive prefix of an absolute Windows path, or `None` if the path
/// has no drive prefix. Accepts a leading `\\?\` extended prefix.
pub fn drive_of(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    let s = s
        .strip_prefix(r"\\?\")
        .or_else(|| s.strip_prefix(r"\\.\"))
        .unwrap_or(&s);
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some(format!("{}:", (bytes[0] as char).to_ascii_uppercase()))
    } else {
        None
    }
}

/// Split a Windows path into its non-empty segments, treating BOTH `\` and `/`
/// as separators and dropping a leading `\\?\` / `\\.\` extended prefix.
///
/// Rust's `Path::components` is platform-separator-specific (a `\` is NOT a
/// separator off Windows), so parsing segments ourselves makes this logic
/// behave IDENTICALLY on the Windows helper and in the cross-OS unit tests /
/// coverage run - the helper only ever handles Windows paths, so `\`/`/` are
/// the separators regardless of the host we test on.
pub(crate) fn path_segments(path: &Path) -> Vec<String> {
    let s = path.to_string_lossy();
    let s = s
        .strip_prefix(r"\\?\")
        .or_else(|| s.strip_prefix(r"\\.\"))
        .unwrap_or(&s);
    s.split(['/', '\\'])
        .filter(|seg| !seg.is_empty())
        .map(|seg| seg.to_string())
        .collect()
}

/// `true` when any segment of `path` is a `..` (parent-directory) reference.
/// A `.` (current dir) is harmless and allowed; only `..` can walk upward out
/// of a configured root lexically.
pub fn has_traversal(path: &Path) -> bool {
    path_segments(path).iter().any(|seg| seg == "..")
}

/// `true` when `candidate` is `root` itself or lies underneath it, compared
/// SEGMENT BY SEGMENT (so `C:\Docs` never "contains" `C:\DocsEvil`, the classic
/// string-prefix trap). Comparison is ASCII-case-insensitive because Windows
/// paths are. A `root` with more segments than `candidate` can never contain it.
pub fn is_within_root(root: &Path, candidate: &Path) -> bool {
    let root_segs = path_segments(root);
    let cand_segs = path_segments(candidate);
    if root_segs.len() > cand_segs.len() {
        return false;
    }
    root_segs
        .iter()
        .zip(cand_segs.iter())
        .all(|(r, c)| r.eq_ignore_ascii_case(c))
}

/// `true` when `candidate` is within ANY of the configured roots.
pub fn check_within_roots(roots: &[PathBuf], candidate: &Path) -> bool {
    roots.iter().any(|r| is_within_root(r, candidate))
}

/// Full lexical validation of an `OpenLocked` request (DESIGN s5.3.1). The
/// Windows server runs this first, then re-checks the CANONICALISED path
/// against the roots to defeat symlinked-directory escapes.
///
/// Returns the normalised volume on success so the caller does not re-parse it.
pub fn validate_open_request(
    roots: &[PathBuf],
    volume: &str,
    live_path: &Path,
) -> Result<String, ValidateError> {
    let norm_volume = normalize_volume(volume).ok_or(ValidateError::InvalidVolume)?;

    let path_str = live_path.to_string_lossy();
    if path_str.is_empty() || path_str.len() > MAX_PATH_LEN {
        return Err(ValidateError::BadLength);
    }

    // Must be an absolute path with a drive prefix. A drive-qualified path
    // (`X:\...`) IS absolute; `drive_of` returning `None` (no `X:` prefix) is
    // exactly the "not absolute" case, so we do NOT use the platform-specific
    // `Path::is_absolute` (which is false for a Windows path off Windows).
    let drive = drive_of(live_path).ok_or(ValidateError::NotAbsolute)?;

    if has_traversal(live_path) {
        return Err(ValidateError::Traversal);
    }

    if drive != norm_volume {
        return Err(ValidateError::VolumeMismatch);
    }

    if !check_within_roots(roots, live_path) {
        return Err(ValidateError::OutsideRoots);
    }

    Ok(norm_volume)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> Vec<PathBuf> {
        vec![
            PathBuf::from(r"C:\Users\me\Documents"),
            PathBuf::from(r"D:\Backup Source"),
        ]
    }

    #[test]
    fn normalize_volume_variants() {
        assert_eq!(normalize_volume("C:").as_deref(), Some("C:"));
        assert_eq!(normalize_volume("c:\\").as_deref(), Some("C:"));
        assert_eq!(normalize_volume("d").as_deref(), Some("D:"));
        assert_eq!(normalize_volume(" e:\\ ").as_deref(), Some("E:"));
        assert_eq!(normalize_volume("not a volume"), None);
        assert_eq!(normalize_volume(""), None);
        assert_eq!(normalize_volume("CC:"), None);
    }

    #[test]
    fn drive_of_reads_prefix_including_extended() {
        assert_eq!(drive_of(Path::new(r"C:\Users\x")).as_deref(), Some("C:"));
        assert_eq!(
            drive_of(Path::new(r"\\?\C:\Users\x")).as_deref(),
            Some("C:")
        );
        assert_eq!(drive_of(Path::new(r"\\server\share\x")), None);
        assert_eq!(drive_of(Path::new("relative")), None);
    }

    #[test]
    fn traversal_detection() {
        assert!(has_traversal(Path::new(r"C:\Users\me\..\other\f")));
        assert!(!has_traversal(Path::new(r"C:\Users\me\Documents\f.pst")));
        assert!(!has_traversal(Path::new(r"C:\Users\me\.\Documents\f")));
    }

    #[test]
    fn within_root_is_component_wise_not_string_prefix() {
        let root = Path::new(r"C:\Users\me\Documents");
        assert!(is_within_root(
            root,
            Path::new(r"C:\Users\me\Documents\f.pst")
        ));
        assert!(is_within_root(root, root));
        // The string-prefix trap: a SIBLING whose name starts with the root's
        // leaf must NOT be considered inside.
        assert!(!is_within_root(
            root,
            Path::new(r"C:\Users\me\DocumentsEvil\f.pst")
        ));
        // A shorter path is never inside a longer root.
        assert!(!is_within_root(root, Path::new(r"C:\Users\me")));
    }

    #[test]
    fn within_root_is_case_insensitive() {
        // Windows paths are case-insensitive; the helper only handles Windows
        // paths, so the check is case-insensitive on every host it is tested on.
        let root = Path::new(r"C:\Users\me\Documents");
        assert!(is_within_root(
            root,
            Path::new(r"c:\users\ME\documents\Sub\F.PST")
        ));
    }

    #[test]
    fn valid_request_under_a_root_passes() {
        let v = validate_open_request(
            &roots(),
            "C:",
            Path::new(r"C:\Users\me\Documents\mail\Outlook.pst"),
        )
        .expect("valid request");
        assert_eq!(v, "C:");
    }

    #[test]
    fn request_outside_all_roots_is_rejected() {
        let err =
            validate_open_request(&roots(), "C:", Path::new(r"C:\Windows\System32\config\SAM"))
                .unwrap_err();
        assert_eq!(err, ValidateError::OutsideRoots);
        assert_eq!(err.code(), "not_allowed");
    }

    #[test]
    fn traversal_request_is_rejected_even_if_it_would_land_in_a_root() {
        // Lexically walks out of Documents and back to a system path.
        let err = validate_open_request(
            &roots(),
            "C:",
            Path::new(r"C:\Users\me\Documents\..\..\..\Windows\win.ini"),
        )
        .unwrap_err();
        assert_eq!(err, ValidateError::Traversal);
    }

    #[test]
    fn volume_mismatch_is_rejected() {
        // Path is under a D: root but the request claims C:.
        let err = validate_open_request(&roots(), "C:", Path::new(r"D:\Backup Source\db.mdf"))
            .unwrap_err();
        assert_eq!(err, ValidateError::VolumeMismatch);
    }

    #[test]
    fn bad_volume_is_rejected() {
        let err =
            validate_open_request(&roots(), "not-a-vol", Path::new(r"C:\Users\me\Documents\f"))
                .unwrap_err();
        assert_eq!(err, ValidateError::InvalidVolume);
    }

    #[test]
    fn non_absolute_path_is_rejected() {
        let err = validate_open_request(&roots(), "C:", Path::new("relative/path")).unwrap_err();
        assert_eq!(err, ValidateError::NotAbsolute);
    }
}
