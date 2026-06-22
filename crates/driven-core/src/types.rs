//! Shared core types used across the sync engine.
//!
//! Every type in this module is a contract referenced from SPEC s2 (the
//! SQLite schema), SPEC s3 (the `RemoteStore` trait), SPEC s5 (the
//! orchestrator), or SPEC s24 (the error taxonomy). Where a type mirrors a
//! schema column or a spec field, the doc comment cites the section so a
//! reader can trace it back.
//!
//! M1 phase 1 (interfaces only): types and stubs. Implementation bodies
//! land in subsequent M1 phases.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unix epoch milliseconds.
///
/// Used wherever the SPEC schema (s2) stores `INTEGER` timestamps such as
/// `created_at`, `last_synced_at`, `last_uploaded_at`, `last_verified_at`,
/// and the `pending_ops.scheduled_for` due-time. Signed so subtraction is
/// safe across the epoch and across the kind of small backwards wall jumps
/// DESIGN s18.7 explicitly tolerates.
pub type UnixMs = i64;

// -----------------------------------------------------------------------------
// Newtype IDs
// -----------------------------------------------------------------------------

macro_rules! uuid_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Generate a new random v4 id.
            pub fn new_v4() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::from_str(s).map(Self)
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }
    };
}

macro_rules! i64_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = std::num::ParseIntError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                i64::from_str(s).map(Self)
            }
        }
    };
}

uuid_newtype! {
    /// Unique id of a Google account configured in Driven (SPEC s2
    /// `accounts.id`).
    AccountId
}

uuid_newtype! {
    /// Unique id of a backup source: a `(local folder, Drive destination,
    /// account)` triple (SPEC s2 `backup_sources.id`).
    SourceId
}

uuid_newtype! {
    /// Unique id of a restore job spawned from the UI (SPEC s11.5).
    RestoreJobId
}

i64_newtype! {
    /// Activity-log row id (SPEC s2 `activity_log.id`, `INTEGER PRIMARY KEY
    /// AUTOINCREMENT`).
    ActivityId
}

i64_newtype! {
    /// Pending-op work-queue row id (SPEC s2 `pending_ops.id`, `INTEGER
    /// PRIMARY KEY AUTOINCREMENT`).
    PendingOpId
}

// -----------------------------------------------------------------------------
// RelativePath
// -----------------------------------------------------------------------------

/// A path relative to a backup source's `local_path`, in canonical form.
///
/// Invariants the constructor must enforce (validation lands in M2):
/// - Uses forward slashes `/` as the separator, never backslashes.
/// - Never starts with a leading `/`.
/// - Never contains `..` segments.
/// - Never contains the NUL byte.
/// - Is valid UTF-8.
///
/// The canonical form is portable across Windows / macOS / Linux so the
/// SQLite `file_state.relative_path` column is a stable key across
/// platforms and survives a cross-platform restore.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelativePath(String);

impl RelativePath {
    /// Returns the underlying string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for RelativePath {
    type Error = RelativePathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        use unicode_normalization::UnicodeNormalization;

        // Reject Windows-absolute / drive-relative / UNC / device paths on
        // the RAW input, BEFORE backslash normalization. Otherwise
        // `"C:\Users\me\file.txt"` would normalize to `"C:/Users/me/..."`
        // and slip past the relative checks - a restore-path-breakout risk
        // for a backup/restore tool. Order matters: these run on the raw
        // string; the leading-`/` check runs after normalization below.
        //
        // Drive-absolute / drive-relative: a second char of `:` with an
        // ascii-alphabetic first char covers `C:\x`, `C:/x`, and bare
        // `C:x`. (A manual check; no regex dependency needed.)
        {
            let bytes = value.as_bytes();
            if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
                return Err(RelativePathError::DriveOrUnc);
            }
        }
        // UNC / device prefixes on the raw input: `\\server\share`,
        // `\\?\C:\...`, and the forward-slash spelling `//server/share`.
        if value.starts_with("\\\\") || value.starts_with("//") {
            return Err(RelativePathError::DriveOrUnc);
        }

        // Normalise Windows separators to forward slashes so the
        // canonical form is portable across platforms (the doc invariant
        // above and SPEC s2 `file_state.relative_path`).
        let s = value.replace('\\', "/");
        if s.is_empty() {
            return Err(RelativePathError::Empty);
        }
        // After backslash normalization, a UNC `\\server` becomes
        // `//server`; the raw-input guard above already rejected both
        // spellings, but re-check here so any path that normalizes to a
        // `//`-prefixed form is also refused.
        if s.starts_with("//") {
            return Err(RelativePathError::DriveOrUnc);
        }
        if s.starts_with('/') {
            return Err(RelativePathError::NotRelative);
        }
        if s.contains('\0') {
            return Err(RelativePathError::NulByte);
        }
        if s.split('/').any(|seg| seg == "..") {
            return Err(RelativePathError::ParentSegment);
        }
        // NFC-normalize so byte-distinct spellings of the same logical path
        // (NFC vs NFD unicode) collapse to one `file_state` key. SPEC s24
        // local.unicode_collision depends on this canonical form. The
        // validity checks above run on the pre-normalized string because
        // NFC never introduces `/`, `..`, NUL, or a leading separator.
        let s: String = s.nfc().collect();
        Ok(Self(s))
    }
}

impl TryFrom<&Path> for RelativePath {
    type Error = RelativePathError;

    fn try_from(value: &Path) -> Result<Self, Self::Error> {
        let s = value.to_str().ok_or(RelativePathError::NotUtf8)?;
        Self::try_from(s.to_string())
    }
}

/// Errors produced when constructing a [`RelativePath`].
#[derive(Debug, thiserror::Error)]
pub enum RelativePathError {
    /// Path is the empty string.
    #[error("path must not be empty")]
    Empty,
    /// Path is absolute or starts with a leading separator.
    #[error("path must be relative")]
    NotRelative,
    /// Path is a Windows drive-absolute / drive-relative path (e.g.
    /// `C:\x`, `C:/x`, `C:x`) or a UNC / device path (`\\server\share`,
    /// `//server/share`, `\\?\C:\...`). Rejecting these prevents a
    /// restore-path breakout on Windows (SPEC s2 `file_state.relative_path`
    /// must stay strictly relative).
    #[error("path must not be a Windows drive-absolute or UNC path")]
    DriveOrUnc,
    /// Path contains a `..` parent segment.
    #[error("path must not contain `..` segments")]
    ParentSegment,
    /// Path contains a NUL byte.
    #[error("path must not contain a NUL byte")]
    NulByte,
    /// Path is not valid UTF-8.
    #[error("path must be valid UTF-8")]
    NotUtf8,
}

// -----------------------------------------------------------------------------
// FileStateStatus
// -----------------------------------------------------------------------------

/// Status of a row in the `file_state` table (SPEC s2: TEXT column with
/// values `'synced' | 'pending' | 'corrupt' | 'locked' | 'error'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStateStatus {
    /// Latest local bytes are uploaded and verified.
    Synced,
    /// Awaiting upload; an entry in `pending_ops` should exist.
    Pending,
    /// Deep-verify detected a checksum mismatch.
    Corrupt,
    /// The file is locked (Windows sharing violation; see SPEC s24
    /// `local.file_locked`).
    Locked,
    /// Last attempt failed with a non-retryable error.
    Error,
}

// -----------------------------------------------------------------------------
// AccountState
// -----------------------------------------------------------------------------

/// Lifecycle state of an `accounts` row (SPEC s2: TEXT column with values
/// `'ok' | 'needs_reauth' | 'disabled'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    /// Normal operating state; refresh-token works.
    Ok,
    /// Refresh-token returned `invalid_grant`; user must re-consent.
    NeedsReauth,
    /// User has explicitly disabled sync for this account.
    Disabled,
}

// -----------------------------------------------------------------------------
// PauseReason
// -----------------------------------------------------------------------------

/// Reason the orchestrator is in the `Paused` state (DESIGN s5.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// User clicked "Pause" in the tray or settings UI.
    Manual,
    /// Running on battery and `skip_on_battery` is true.
    Battery,
    /// Connected to a metered network and `skip_on_metered` is true.
    Metered,
    /// No network reachability.
    Offline,
    /// A specific dependent service (Drive, OAuth, etc.) is down per the
    /// network-resilience probes (DESIGN s5.8).
    ServiceDown,
}

// -----------------------------------------------------------------------------
// Op + Plan
// -----------------------------------------------------------------------------

/// One unit of work the planner emits for the executor (SPEC s7).
///
/// M1 phase 1 stub: only the variants used by the M1 contract surface are
/// declared. The full variant set (resume, deep-verify, etc.) lands in M2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Hash a local file and upload it (creating or updating the remote
    /// object as appropriate). SPEC s8.
    HashThenUpload {
        /// Source the file belongs to.
        source_id: SourceId,
        /// Path relative to the source's `local_path`.
        relative_path: RelativePath,
        /// Local file size in bytes, captured pre-open.
        size: u64,
    },
    /// Trash a remote object that no longer has a local counterpart
    /// (SPEC s7). Trash is preferred over hard-delete so the user can
    /// recover from a mistaken delete via the Drive web UI.
    Trash {
        /// Source the (now-missing) file belonged to.
        source_id: SourceId,
        /// Relative path the file had before it was deleted locally.
        relative_path: RelativePath,
        /// Drive `file_id` of the remote object to trash.
        drive_file_id: String,
    },
}

/// A batched list of [`Op`] values produced by the planner (SPEC s7).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Ops in planner-emitted order. The executor is free to reorder for
    /// concurrency but must preserve happens-before semantics for ops on
    /// the same `(source_id, relative_path)`.
    pub ops: Vec<Op>,
}

impl Plan {
    /// Tallies the plan into a [`PlanSummary`] for activity logging and the
    /// orchestrator's `Planning { plan: PlanSummary }` state (SPEC s5).
    ///
    /// `bytes` counts only [`Op::HashThenUpload`] sizes - trashes move no
    /// bytes. This is a pure fold over `ops`, not sync behaviour.
    pub fn summary(&self) -> PlanSummary {
        let mut summary = PlanSummary::default();
        for op in &self.ops {
            match op {
                Op::HashThenUpload { size, .. } => {
                    summary.uploads += 1;
                    summary.bytes += *size;
                }
                Op::Trash { .. } => summary.trashes += 1,
            }
        }
        summary
    }
}

/// A counts-only digest of a [`Plan`] (SPEC s5
/// `OrchestratorState::Planning { plan: PlanSummary }`).
///
/// Used for the activity-log "scan_done"/dry-run summary line and the
/// orchestrator state without carrying the full op list across the IPC
/// boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    /// Number of [`Op::HashThenUpload`] ops in the plan.
    pub uploads: usize,
    /// Number of [`Op::Trash`] ops in the plan.
    pub trashes: usize,
    /// Total bytes the upload ops will move (sum of their `size` fields).
    /// Trashes contribute nothing.
    pub bytes: u64,
}

// -----------------------------------------------------------------------------
// Scanner surface: ScanResult, LocalEntry, ScanMode, SymlinkPolicy
// -----------------------------------------------------------------------------

/// The diff a single scan of one source produces (SPEC s6).
///
/// The scanner walks the local tree, compares each file against the
/// `file_state` rows (SPEC s2) loaded for the source, and emits the set of
/// files that need uploading plus the set whose `file_state` rows have no
/// surviving local file. The planner (SPEC s7) turns this into a [`Plan`].
///
/// Paths are [`RelativePath`] (NFC-canonical, the `file_state` primary-key
/// form per DESIGN s5.2.3), not raw `PathBuf` - the SPEC s6 pseudocode
/// uses `PathBuf` only illustratively.
///
/// Note: this shape does NOT carry the walk-error / partial-success signal
/// DESIGN s5.2 step 3 requires to gate safe deletion (a permission denial
/// under a subtree must never cascade into trashing everything under it).
/// That signal's channel is unresolved at this layer - see the M2 phase-1
/// finding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanResult {
    /// Files that are new or whose `(size, mtime_ns)` (or, under
    /// [`ScanMode::DeepVerify`], whose hash) differs from the stored
    /// `file_state` row. Each becomes one [`Op::HashThenUpload`].
    pub new_or_changed: Vec<LocalEntry>,
    /// Relative paths present in `file_state` but no longer on disk. The
    /// planner trashes the ones that reached Drive and drops the rest
    /// (SPEC s7).
    pub deleted: Vec<RelativePath>,
}

/// One local file the scanner observed (SPEC s6 `LocalEntry`).
///
/// Carries exactly the cheap stat fields the fast-path diff compares
/// against the `file_state` row (DESIGN s5.2 step 2); the BLAKE3 hash is
/// computed later by the executor's `HashThenUpload`, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEntry {
    /// Path under the source's `local_path`, NFC-canonical.
    pub rel: RelativePath,
    /// File size in bytes from the entry's metadata.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch. Signed to
    /// match `file_state.mtime_ns` (SPEC s2) and tolerate pre-epoch mtimes.
    pub mtime_ns: i64,
}

/// How aggressively a scan decides a file changed (DESIGN s3.3, s5.2).
///
/// The two modes differ only in the change-detection predicate; both emit
/// the same [`ScanResult`] shape, so a [`ScanMode::DeepVerify`] hit lands
/// in `new_or_changed` exactly like an mtime/size change and produces one
/// [`Op::HashThenUpload`] (ROADMAP M2 "deep-verify catches bit-rot" row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    /// Default per-tick scan: a file is changed iff its `(size, mtime_ns)`
    /// differs from the stored `file_state` row. Cheap; never reads
    /// content (DESIGN s5.2 step 2 fast path).
    FastPath,
    /// Periodic re-verification (default weekly per
    /// `deep_verify_interval_secs`): re-hash every file regardless of
    /// `(size, mtime_ns)` and treat a hash mismatch against the stored
    /// `hash_blake3` as a change. Catches silent bit-rot and filesystem
    /// timestamp lies (DESIGN s3.3, s5.2 step 4).
    DeepVerify,
}

/// What the scanner does when it meets a symbolic link (DESIGN s5.2.1).
///
/// V1 ships only [`SymlinkPolicy::Skip`]: the link is not followed and the
/// link itself is not backed up, because following can walk out of the
/// configured source, can loop, and is almost never what the user
/// intended. A per-source "follow symlinks" toggle (the `Follow` variant)
/// is V2 - omitted here so V1 code can never accidentally follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymlinkPolicy {
    /// Do not follow symlinks; do not back up the link itself. V1 default
    /// and only option (DESIGN s5.2.1).
    #[default]
    Skip,
}

// -----------------------------------------------------------------------------
// ErrorCode
// -----------------------------------------------------------------------------

/// Stable dotted error codes surfaced across the IPC boundary (SPEC s24).
///
/// Codes are load-bearing for i18n: they are translation-bundle keys, so
/// they must never change between minor versions. New codes may be added;
/// existing codes may be deprecated but stay translatable for at least one
/// major release.
///
/// [`Display`] and [`ErrorCode::code`] both return the dotted string form
/// (e.g. `"drive.rate_limited"`); the human-readable meanings live only
/// in doc comments below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// `auth.invalid_grant` - refresh token revoked; reauth required.
    AuthInvalidGrant,
    /// `auth.consent_required` - first-time auth or scope change.
    AuthConsentRequired,
    /// `auth.network_unreachable` - couldn't reach accounts.google.com.
    AuthNetworkUnreachable,
    /// `drive.rate_limited` - 429 / userRateLimitExceeded.
    DriveRateLimited,
    /// `drive.daily_quota_exhausted` - 403 dailyLimitExceeded, paused
    /// until reset.
    DriveDailyQuotaExhausted,
    /// `drive.quota_exhausted` - 403 storageQuotaExceeded (user's Drive
    /// is full).
    DriveQuotaExhausted,
    /// `drive.upload_size_limit` - file exceeds Drive's per-file size
    /// limit.
    DriveUploadSizeLimit,
    /// `drive.checksum_mismatch` - verification failed after upload.
    DriveChecksumMismatch,
    /// `drive.unreachable` - Drive API down, unreachable, or 5xx
    /// circuit-open.
    DriveUnreachable,
    /// `drive.resumable_session_invalid` - 4xx during resumable upload;
    /// caller must restart the session.
    DriveResumableSessionInvalid,
    /// `drive.dest_folder_missing` - the configured destination folder
    /// was deleted from Drive by the user.
    DriveDestFolderMissing,
    /// `drive.dest_folder_permission_denied` - destination folder's
    /// sharing changed to read-only for this account.
    DriveDestFolderPermissionDenied,
    /// `local.file_locked` - couldn't open even with `FILE_SHARE_DELETE`
    /// (V1: locked file, VSS path failed too).
    LocalFileLocked,
    /// `local.vss_unavailable` - Driven needs elevation to use VSS but
    /// isn't elevated.
    LocalVssUnavailable,
    /// `local.file_changed_during_upload` - pre/post fstat showed file
    /// mutated mid-upload; re-queued.
    LocalFileChangedDuringUpload,
    /// `local.file_replaced_during_upload` - atomic-replace detected by
    /// inode identity check; re-queued.
    LocalFileReplacedDuringUpload,
    /// `local.io_error` - generic disk error.
    LocalIoError,
    /// `local.path_too_long` - OS path-length limit hit.
    LocalPathTooLong,
    /// `local.unicode_collision` - two distinct paths normalise to the
    /// same NFC string.
    LocalUnicodeCollision,
    /// `local.disk_full` - source filesystem out of space during a
    /// verify-style read or restore write.
    LocalDiskFull,
    /// `local.invalid_filename` - a name the local OS allowed but Drive
    /// will reject (reserved name, trailing dot/space, etc.).
    LocalInvalidFilename,
    /// `local.ads_skipped` - NTFS Alternate Data Stream encountered; main
    /// stream backed up, ADS skipped.
    LocalAdsSkipped,
    /// `net.offline` - OS reports no network connectivity.
    NetOffline,
    /// `net.no_internet` - connected but generate-204 probe fails.
    NetNoInternet,
    /// `net.dns_failed` - resolver returned no answer for a known-good
    /// domain.
    NetDnsFailed,
    /// `net.captive_portal` - captive portal detected; user action
    /// required.
    NetCaptivePortal,
    /// `net.timeout` - request exceeded its configured timeout.
    NetTimeout,
    /// `net.intermittent` - circuit-breaker tripped after N failures.
    NetIntermittent,
    /// `net.proxy_required` - 407 from HTTP proxy, proxy auth needed.
    NetProxyRequired,
    /// `update.endpoint_unreachable` - driven.maxhogan.dev/updates
    /// unreachable.
    UpdateEndpointUnreachable,
    /// `update.signature_invalid` - Tauri updater signature verification
    /// failed.
    UpdateSignatureInvalid,
    /// `crypto.key_missing` - keychain entry not found.
    CryptoKeyMissing,
    /// `crypto.decrypt_failed` - AEAD verification failed.
    CryptoDecryptFailed,
    /// `crypto.recovery_phrase_invalid` - BIP39 input failed checksum.
    CryptoRecoveryPhraseInvalid,
    /// `state.db_locked` - SQLite locked (transient).
    StateDbLocked,
    /// `state.db_corrupt` - SQLite integrity_check failed; rebuild from
    /// Drive backup advised.
    StateDbCorrupt,
    /// `state.reconcile_orphan` - startup found a remote object without a
    /// local row; adopted or cleaned.
    StateReconcileOrphan,
    /// `harness.timeout` - a stress-harness scenario exceeded its budget
    /// (chaos crate only).
    HarnessTimeout,
    /// `internal.bug` - programming error; please report.
    InternalBug,
}

impl ErrorCode {
    /// Returns the stable dotted code string used as the i18n key and as
    /// the JSON `code` field at the IPC boundary (SPEC s24).
    pub fn code(self) -> &'static str {
        match self {
            ErrorCode::AuthInvalidGrant => "auth.invalid_grant",
            ErrorCode::AuthConsentRequired => "auth.consent_required",
            ErrorCode::AuthNetworkUnreachable => "auth.network_unreachable",
            ErrorCode::DriveRateLimited => "drive.rate_limited",
            ErrorCode::DriveDailyQuotaExhausted => "drive.daily_quota_exhausted",
            ErrorCode::DriveQuotaExhausted => "drive.quota_exhausted",
            ErrorCode::DriveUploadSizeLimit => "drive.upload_size_limit",
            ErrorCode::DriveChecksumMismatch => "drive.checksum_mismatch",
            ErrorCode::DriveUnreachable => "drive.unreachable",
            ErrorCode::DriveResumableSessionInvalid => "drive.resumable_session_invalid",
            ErrorCode::DriveDestFolderMissing => "drive.dest_folder_missing",
            ErrorCode::DriveDestFolderPermissionDenied => "drive.dest_folder_permission_denied",
            ErrorCode::LocalFileLocked => "local.file_locked",
            ErrorCode::LocalVssUnavailable => "local.vss_unavailable",
            ErrorCode::LocalFileChangedDuringUpload => "local.file_changed_during_upload",
            ErrorCode::LocalFileReplacedDuringUpload => "local.file_replaced_during_upload",
            ErrorCode::LocalIoError => "local.io_error",
            ErrorCode::LocalPathTooLong => "local.path_too_long",
            ErrorCode::LocalUnicodeCollision => "local.unicode_collision",
            ErrorCode::LocalDiskFull => "local.disk_full",
            ErrorCode::LocalInvalidFilename => "local.invalid_filename",
            ErrorCode::LocalAdsSkipped => "local.ads_skipped",
            ErrorCode::NetOffline => "net.offline",
            ErrorCode::NetNoInternet => "net.no_internet",
            ErrorCode::NetDnsFailed => "net.dns_failed",
            ErrorCode::NetCaptivePortal => "net.captive_portal",
            ErrorCode::NetTimeout => "net.timeout",
            ErrorCode::NetIntermittent => "net.intermittent",
            ErrorCode::NetProxyRequired => "net.proxy_required",
            ErrorCode::UpdateEndpointUnreachable => "update.endpoint_unreachable",
            ErrorCode::UpdateSignatureInvalid => "update.signature_invalid",
            ErrorCode::CryptoKeyMissing => "crypto.key_missing",
            ErrorCode::CryptoDecryptFailed => "crypto.decrypt_failed",
            ErrorCode::CryptoRecoveryPhraseInvalid => "crypto.recovery_phrase_invalid",
            ErrorCode::StateDbLocked => "state.db_locked",
            ErrorCode::StateDbCorrupt => "state.db_corrupt",
            ErrorCode::StateReconcileOrphan => "state.reconcile_orphan",
            ErrorCode::HarnessTimeout => "harness.timeout",
            ErrorCode::InternalBug => "internal.bug",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_accepts_canonical_forms() {
        for s in ["a/b.txt", "deeply/nested/file", "file.txt"] {
            let rp = RelativePath::try_from(s.to_string()).expect("happy");
            assert_eq!(rp.as_str(), s);
        }
    }

    #[test]
    fn relative_path_normalises_backslashes() {
        let rp = RelativePath::try_from(r"a\b\c.txt".to_string()).unwrap();
        assert_eq!(rp.as_str(), "a/b/c.txt");
    }

    #[test]
    fn relative_path_rejects_empty() {
        assert!(matches!(
            RelativePath::try_from(String::new()),
            Err(RelativePathError::Empty)
        ));
    }

    #[test]
    fn relative_path_rejects_absolute() {
        assert!(matches!(
            RelativePath::try_from("/etc/passwd".to_string()),
            Err(RelativePathError::NotRelative)
        ));
    }

    #[test]
    fn relative_path_rejects_parent_segment() {
        assert!(matches!(
            RelativePath::try_from("a/../b".to_string()),
            Err(RelativePathError::ParentSegment)
        ));
        assert!(matches!(
            RelativePath::try_from("..".to_string()),
            Err(RelativePathError::ParentSegment)
        ));
        // A leading "." is fine; a segment that just contains ".." is not.
        assert!(RelativePath::try_from("a/..b/c".to_string()).is_ok());
    }

    #[test]
    fn relative_path_rejects_windows_absolute_and_unc() {
        // Drive-absolute / drive-relative (checked on raw input, BEFORE
        // backslash normalization would mask `C:\` as `C:/`).
        for s in [r"C:\Users\me", "C:/Users/me", "C:x", r"d:\file"] {
            assert!(
                matches!(
                    RelativePath::try_from(s.to_string()),
                    Err(RelativePathError::DriveOrUnc)
                ),
                "expected DriveOrUnc for {s:?}"
            );
        }
        // UNC / device prefixes in both spellings.
        for s in [r"\\server\share\f", "//server/share/f"] {
            assert!(
                matches!(
                    RelativePath::try_from(s.to_string()),
                    Err(RelativePathError::DriveOrUnc)
                ),
                "expected DriveOrUnc for {s:?}"
            );
        }
        // Ordinary relative paths still accepted.
        for s in ["a/b.txt", "deep/nested/file"] {
            assert!(
                RelativePath::try_from(s.to_string()).is_ok(),
                "expected Ok for {s:?}"
            );
        }
    }

    #[test]
    fn relative_path_rejects_nul_byte() {
        assert!(matches!(
            RelativePath::try_from("a\0b".to_string()),
            Err(RelativePathError::NulByte)
        ));
    }

    #[test]
    fn relative_path_from_path_round_trips() {
        let rp: RelativePath = std::path::Path::new("a/b.txt").try_into().unwrap();
        assert_eq!(rp.as_str(), "a/b.txt");
    }
}
