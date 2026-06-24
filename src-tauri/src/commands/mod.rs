//! Tauri IPC command surface (SPEC s11).
//!
//! M6 lands the accounts / sources / settings surface (SPEC s11.1 / s11.2 /
//! s11.6) alongside the M5 sync commands (SPEC s11.3); the restore + activity
//! surface (SPEC s11.4 / s11.5) is M7/M8. The shared [`CommandError`] + the
//! [`validate_writable_dest`] path-safety helper live here; the per-area
//! commands live in [`accounts`], [`sources`], [`settings`], and [`sync`], and
//! the shared DTOs in [`dtos`].

pub mod accounts;
pub mod dtos;
pub mod settings;
pub mod sources;
pub mod sync;

use std::fs::File;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use driven_core::types::ErrorCode;

/// The serializable error every IPC command returns on failure (SPEC s24).
///
/// Tauri requires a command's `Err` type to be `Serialize`; `anyhow::Error`
/// is not, so the command bodies map their internal errors into this stable
/// SPEC s24 shape: `{ code, message, retry_after_ms?, details? }`. The
/// [`code`](Self::code) is the load-bearing i18n key the webview resolves via
/// `t('errors.${code}.short')` (DESIGN s8.7); `message` is a redacted
/// human-readable fallback.
/// Wire casing note: the M6 typed-IPC surface is camelCase over the wire (see
/// `design/CODEX_NOTES.md` M6), so this error shape renders `retryAfterMs`
/// (not the SPEC s24 example's literal `retry_after_ms`). `code` + `message` are
/// identical in both casings and `details` is single-word; only the retry-after
/// hint differs, and the frontend reads only `.code`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    /// The stable dotted SPEC s24 error code (i18n key), e.g.
    /// `drive.rate_limited`. Serialised as its dotted string form.
    pub code: ErrorCode,
    /// Human-readable error message (already redacted of secrets).
    pub message: String,
    /// Optional retry-after hint in milliseconds, populated for codes that
    /// carry one (e.g. `drive.rate_limited`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// Optional free-form structured detail (SPEC s24 `details`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl CommandError {
    /// Build a command error from any displayable message, defaulting the code
    /// to [`ErrorCode::InternalBug`]. Kept so the M5 sync commands (which only
    /// surfaced a message) compile unchanged; the M6 commands prefer
    /// [`CommandError::with_code`] so the webview gets the right i18n key.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::InternalBug,
            message: message.into(),
            retry_after_ms: None,
            details: None,
        }
    }

    /// Build a command error with an explicit SPEC s24 [`ErrorCode`] (the i18n
    /// key) and a redacted human-readable fallback message.
    pub fn with_code(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retry_after_ms: None,
            details: None,
        }
    }

    /// Attach a retry-after hint (milliseconds) - used for `drive.rate_limited`.
    #[must_use]
    pub fn with_retry_after_ms(mut self, retry_after_ms: u64) -> Self {
        self.retry_after_ms = Some(retry_after_ms);
        self
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CommandError {}

impl From<anyhow::Error> for CommandError {
    /// Map an internal [`anyhow::Error`] to the stable IPC shape, recovering a
    /// SPEC s24 code from the error chain where we can: a classified
    /// [`driven_drive::google::DriveError`] (rate-limit / 5xx / network / auth /
    /// quota), an embedded dotted code substring in the message (the
    /// `DriveError` `Display` and the OAuth refresh path both embed one), or the
    /// crypto / state error families. Anything unrecognised falls back to
    /// [`ErrorCode::InternalBug`] rather than guessing.
    fn from(e: anyhow::Error) -> Self {
        code_from_anyhow(&e)
    }
}

/// Derive the best SPEC s24 [`CommandError`] for an [`anyhow::Error`].
///
/// Order: (1) the classified `DriveError` downcast (authoritative for Drive +
/// the OAuth-refresh `invalid_grant` path, which surfaces a classified
/// `DriveError`); (2) a dotted-code substring scan of the message (the
/// `DriveError`/OAuth `Display` text embeds the literal SPEC s24 code, and the
/// loopback flow's denial/timeout messages carry recognisable markers); (3)
/// fall back to `internal.bug`. The message is the error's `Display` (already
/// free of secrets - tokens never reach `Display` here).
fn code_from_anyhow(e: &anyhow::Error) -> CommandError {
    use driven_drive::google::classification_of;
    use driven_drive::remote_store::DriveErrorClassification as C;

    if let Some(class) = classification_of(e) {
        return match class {
            C::RateLimited { retry_after_ms } => {
                CommandError::with_code(ErrorCode::DriveRateLimited, e.to_string())
                    .with_retry_after_ms(retry_after_ms)
            }
            C::Transient5xx => CommandError::with_code(ErrorCode::DriveUnreachable, e.to_string()),
            C::Network => CommandError::with_code(ErrorCode::NetIntermittent, e.to_string()),
            C::AuthInvalidGrant => {
                CommandError::with_code(ErrorCode::AuthInvalidGrant, e.to_string())
            }
            C::DailyQuota => {
                CommandError::with_code(ErrorCode::DriveDailyQuotaExhausted, e.to_string())
            }
            C::StorageQuota => {
                CommandError::with_code(ErrorCode::DriveQuotaExhausted, e.to_string())
            }
            C::Other => CommandError::with_code(ErrorCode::DriveUnreachable, e.to_string()),
        };
    }

    let msg = e.to_string();
    if let Some(code) = code_from_message(&msg) {
        return CommandError::with_code(code, msg);
    }
    CommandError::with_code(ErrorCode::InternalBug, msg)
}

/// Recover a SPEC s24 [`ErrorCode`] from a recognisable dotted-code substring in
/// `msg`. The `DriveError`/OAuth `Display` text and the crypto/keystore error
/// families embed the literal code (e.g. `auth.invalid_grant`,
/// `crypto.decrypt_failed`); this scans for the longest matching known code so
/// a more specific code wins (e.g. `drive.daily_quota_exhausted` before
/// `drive.quota_exhausted`).
fn code_from_message(msg: &str) -> Option<ErrorCode> {
    // Candidate codes ordered most-specific-first so a substring scan does not
    // shadow a longer code with a shorter prefix of it.
    const CANDIDATES: &[&str] = &[
        "auth.invalid_grant",
        "auth.consent_required",
        "auth.network_unreachable",
        "drive.rate_limited",
        "drive.daily_quota_exhausted",
        "drive.quota_exhausted",
        "drive.resumable_session_invalid",
        "drive.dest_folder_permission_denied",
        "drive.dest_folder_missing",
        "drive.checksum_mismatch",
        "drive.upload_size_limit",
        "drive.unreachable",
        "crypto.recovery_phrase_invalid",
        "crypto.decrypt_failed",
        "crypto.key_missing",
        "state.db_corrupt",
        "state.db_locked",
        "net.captive_portal",
        "net.dns_failed",
        "net.no_internet",
        "net.proxy_required",
        "net.timeout",
        "net.intermittent",
        "net.offline",
        "update.signature_invalid",
        "update.endpoint_unreachable",
    ];
    CANDIDATES
        .iter()
        .find(|code| msg.contains(**code))
        .and_then(|code| ErrorCode::from_code(code))
}

/// The result alias every IPC command returns (SPEC s11).
pub type CommandResult<T> = Result<T, CommandError>;

/// A token proving a path was produced by a `tauri-plugin-dialog` dialog
/// (SPEC s11.6.1), NOT injected as a raw string by the (untrusted) webview.
///
/// The webview never fabricates one of these: the backend confines every write
/// to a dialog-derived path. In M6 the path-bearing write commands
/// (`export_diagnostic_bundle`) derive the allowed root from the SAVE LOCATION
/// the user chose (its parent directory) and pass it here, so
/// [`validate_writable_dest`] confines the actual write to that one directory -
/// a webview that tampers with the leaf filename can still only write inside the
/// dialog-approved directory, never escape it via `..` or a symlink.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DialogToken(pub String);

impl DialogToken {
    /// Build a token confining writes to `root` (a directory the user approved
    /// via a native dialog). The stored value is the directory path; the
    /// backend canonicalises it before confinement so it cannot itself contain
    /// a traversal.
    pub fn for_root(root: impl Into<String>) -> Self {
        Self(root.into())
    }
}

/// Validate a webview-supplied destination path before ANY filesystem write
/// (SPEC s11.6.1). The single choke point every path-bearing write command
/// (`restore_files`, `export_diagnostic_bundle`) routes through.
///
/// Enforces, in order (SPEC s11.6.1 1-4):
/// 1. canonicalise the leaf's PARENT via `dunce::canonicalize` (the leaf itself
///    may not exist yet - we are about to create it; a non-existent parent is
///    rejected with a clear error);
/// 2. confine to the allowed root proven by `dialog_token` (the dialog-approved
///    directory): the canonical parent must equal or sit under the canonical
///    token root - reject any path the webview shaped outside it;
/// 3. reject any `..` component surviving canonicalisation (path-traversal
///    defence - canonicalisation eats them, this double-checks);
/// 4. reject a symlink AT THE LEAF (no write through a symlink): if the leaf
///    already exists as a symlink, refuse rather than dereference.
///
/// Returns the canonical, confined [`PathBuf`] (canonical parent rejoined with
/// the leaf file name) the caller then writes to atomically (SPEC s11.6.1 step
/// 5 via [`atomic_write`]).
pub fn validate_writable_dest(path: &Path, dialog_token: &DialogToken) -> CommandResult<PathBuf> {
    // The leaf must have a final component (a file name) and a parent dir.
    let file_name = path.file_name().ok_or_else(|| {
        CommandError::with_code(
            ErrorCode::LocalIoError,
            "destination path has no file name component",
        )
    })?;
    // 3 (early): reject a literal `..` anywhere in the requested path before we
    // touch the filesystem (defence in depth; canonicalisation below also eats
    // these, but rejecting up front gives a clear error and avoids resolving a
    // traversal at all).
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(CommandError::with_code(
            ErrorCode::LocalIoError,
            "destination path must not contain `..` segments",
        ));
    }

    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = match parent {
        Some(p) => p,
        // No parent means a bare file name with no directory; resolve against
        // the current dir is ambiguous + un-confinable, so reject.
        None => {
            return Err(CommandError::with_code(
                ErrorCode::LocalIoError,
                "destination path must include a directory",
            ))
        }
    };

    // 1: canonicalise the parent (rejecting a non-existent parent).
    let canon_parent = dunce::canonicalize(parent).map_err(|e| {
        CommandError::with_code(
            ErrorCode::LocalIoError,
            format!("destination directory does not exist or is unreadable: {e}"),
        )
    })?;

    // 2: confine to the dialog-approved root. The token root must exist (the
    // user picked it); canonicalise it the same way so the comparison is
    // symlink/UNC-stable.
    let canon_root = dunce::canonicalize(&dialog_token.0).map_err(|e| {
        CommandError::with_code(
            ErrorCode::LocalIoError,
            format!("dialog-approved root is not a valid directory: {e}"),
        )
    })?;
    if !canon_parent.starts_with(&canon_root) {
        return Err(CommandError::with_code(
            ErrorCode::LocalIoError,
            "destination is outside the dialog-approved directory",
        ));
    }

    let dest = canon_parent.join(file_name);

    // 4: reject a symlink at the leaf when it already exists (no write through a
    // symlink). `symlink_metadata` does not follow the final component.
    match std::fs::symlink_metadata(&dest) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(CommandError::with_code(
                ErrorCode::LocalIoError,
                "destination is a symlink; refusing to write through it",
            ));
        }
        // Exists as a regular file/dir, or does not exist yet: both fine.
        Ok(_) | Err(_) => {}
    }

    Ok(dest)
}

/// Atomically write `bytes` to `dest` (SPEC s11.6.1 step 5): write to a
/// sibling `<dest>.driven-tmp.<nonce>`, flush + fsync, then `rename` over the
/// final name so a crash never leaves a half-written file under the final name.
///
/// `dest` MUST already be a [`validate_writable_dest`]-confined path. A failure
/// best-effort removes the temp file before returning the error.
pub fn atomic_write(dest: &Path, bytes: &[u8]) -> CommandResult<()> {
    let parent = dest.parent().ok_or_else(|| {
        CommandError::with_code(ErrorCode::LocalIoError, "atomic write: dest has no parent")
    })?;
    // A per-write nonce from the OS clock so concurrent exports never collide.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".driven-tmp.{}.{nonce}",
        dest.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bundle")
    ));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(CommandError::with_code(
            ErrorCode::LocalIoError,
            format!("atomic write failed: {e}"),
        ));
    }
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(CommandError::with_code(
            ErrorCode::LocalIoError,
            format!("atomic rename failed: {e}"),
        ));
    }
    Ok(())
}

/// Validate a webview-supplied LOCAL READ ROOT (SPEC s11.6.1 applied to the
/// read side): `add_source.local_path` / `preview_exclusions.local_path`.
///
/// The scanner WALKS this path, so the untrusted-path rule still applies, but
/// the write-only protections (symlink-at-leaf, dialog-token confinement of a
/// not-yet-existing leaf) do not: a source root is an EXISTING directory the
/// user selected via the native dialog. We therefore (1) reject any `..`
/// component, (2) canonicalise via `dunce` (rejecting a non-existent path), and
/// (3) require the canonical target to be a directory. Returns the canonical
/// root the scanner then walks.
pub fn validate_readable_dir(path: &Path) -> CommandResult<PathBuf> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(CommandError::with_code(
            ErrorCode::LocalIoError,
            "source path must not contain `..` segments",
        ));
    }
    let canon = dunce::canonicalize(path).map_err(|e| {
        CommandError::with_code(
            ErrorCode::LocalIoError,
            format!("source folder does not exist or is unreadable: {e}"),
        )
    })?;
    let meta = std::fs::metadata(&canon).map_err(|e| {
        CommandError::with_code(
            ErrorCode::LocalIoError,
            format!("source folder is unreadable: {e}"),
        )
    })?;
    if !meta.is_dir() {
        return Err(CommandError::with_code(
            ErrorCode::LocalIoError,
            "source path is not a directory",
        ));
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_error_serialises_to_s24_shape() {
        let err = CommandError::with_code(ErrorCode::DriveRateLimited, "slow down")
            .with_retry_after_ms(1200);
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["code"], "drive.rate_limited");
        assert_eq!(v["message"], "slow down");
        assert_eq!(v["retryAfterMs"], 1200);
    }

    #[test]
    fn command_error_omits_absent_optionals() {
        let err = CommandError::with_code(ErrorCode::InternalBug, "boom");
        let v = serde_json::to_value(&err).unwrap();
        assert!(v.get("retryAfterMs").is_none());
        assert!(v.get("details").is_none());
    }

    #[test]
    fn code_from_message_prefers_more_specific_code() {
        // The daily-quota code contains the substring of nothing shorter that
        // would shadow it; the generic quota code must NOT win when the daily
        // one is present.
        let c = code_from_message("drive.daily_quota_exhausted: 403 dailyLimitExceeded");
        assert_eq!(c, Some(ErrorCode::DriveDailyQuotaExhausted));
        let c = code_from_message("auth.invalid_grant: refresh token revoked");
        assert_eq!(c, Some(ErrorCode::AuthInvalidGrant));
        assert_eq!(code_from_message("some unrelated failure"), None);
    }

    #[test]
    fn validate_writable_dest_accepts_a_file_in_the_root() {
        let dir = tempdir();
        let token = DialogToken::for_root(dir.to_string_lossy().to_string());
        let dest = dir.join("bundle.zip");
        let ok = validate_writable_dest(&dest, &token).expect("valid dest");
        // Canonical parent + the leaf name.
        assert_eq!(ok.file_name().unwrap(), std::ffi::OsStr::new("bundle.zip"));
        let canon_root = dunce::canonicalize(&dir).unwrap();
        assert!(ok.starts_with(&canon_root));
        cleanup(dir);
    }

    #[test]
    fn validate_writable_dest_rejects_parent_traversal() {
        let dir = tempdir();
        let token = DialogToken::for_root(dir.to_string_lossy().to_string());
        let dest = dir.join("..").join("escape.zip");
        let err = validate_writable_dest(&dest, &token).expect_err("traversal must be rejected");
        assert_eq!(err.code, ErrorCode::LocalIoError);
        cleanup(dir);
    }

    #[test]
    fn validate_writable_dest_rejects_outside_the_token_root() {
        let root = tempdir();
        let other = tempdir();
        let token = DialogToken::for_root(root.to_string_lossy().to_string());
        // A real, existing directory that is NOT under the token root.
        let dest = other.join("bundle.zip");
        let err = validate_writable_dest(&dest, &token).expect_err("outside-root must be rejected");
        assert_eq!(err.code, ErrorCode::LocalIoError);
        assert!(err.message.contains("outside"));
        cleanup(root);
        cleanup(other);
    }

    #[test]
    fn validate_writable_dest_rejects_nonexistent_parent() {
        let dir = tempdir();
        let token = DialogToken::for_root(dir.to_string_lossy().to_string());
        let dest = dir.join("no-such-subdir").join("bundle.zip");
        let err =
            validate_writable_dest(&dest, &token).expect_err("missing parent must be rejected");
        assert_eq!(err.code, ErrorCode::LocalIoError);
        cleanup(dir);
    }

    #[cfg(unix)]
    #[test]
    fn validate_writable_dest_rejects_symlink_at_leaf() {
        use std::os::unix::fs::symlink;
        let dir = tempdir();
        let token = DialogToken::for_root(dir.to_string_lossy().to_string());
        let target = dir.join("real-target");
        std::fs::write(&target, b"x").unwrap();
        let link = dir.join("link.zip");
        symlink(&target, &link).unwrap();
        let err = validate_writable_dest(&link, &token).expect_err("symlink leaf must be rejected");
        assert_eq!(err.code, ErrorCode::LocalIoError);
        assert!(err.message.contains("symlink"));
        cleanup(dir);
    }

    #[test]
    fn atomic_write_round_trips_and_leaves_no_temp() {
        let dir = tempdir();
        let token = DialogToken::for_root(dir.to_string_lossy().to_string());
        let dest = validate_writable_dest(&dir.join("out.bin"), &token).unwrap();
        atomic_write(&dest, b"hello driven").expect("atomic write");
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello driven");
        // No stray temp files left behind.
        let temps: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".driven-tmp."))
            .collect();
        assert!(temps.is_empty(), "atomic write must leave no temp files");
        cleanup(dir);
    }

    #[test]
    fn validate_readable_dir_accepts_existing_dir_rejects_traversal_and_files() {
        let dir = tempdir();
        // Existing dir: accepted, returned canonical.
        let canon = validate_readable_dir(&dir).expect("existing dir");
        assert_eq!(canon, dunce::canonicalize(&dir).unwrap());
        // A `..` segment is rejected before any fs touch.
        let err = validate_readable_dir(&dir.join("..").join("x")).expect_err("traversal");
        assert_eq!(err.code, ErrorCode::LocalIoError);
        // A regular file (not a dir) is rejected.
        let file = dir.join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let err = validate_readable_dir(&file).expect_err("file is not a dir");
        assert_eq!(err.code, ErrorCode::LocalIoError);
        cleanup(dir);
    }

    // --- minimal temp-dir helper (no tempfile dep in src-tauri) -------------

    /// Create a unique temp directory under the OS temp dir.
    fn tempdir() -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("driven-ipc-test-{nonce}-{:p}", &nonce));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Best-effort recursive removal of a test temp dir.
    fn cleanup(dir: PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
