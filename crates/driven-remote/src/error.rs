//! The classified remote-store error every backend surfaces (SPEC s24 error
//! taxonomy).
//!
//! Historically this lived in `driven_drive::google` and was named
//! [`DriveError`] because Google Drive was the only destination. The type is
//! backend-NEUTRAL - the executor, pacer and circuit breakers downcast to it to
//! decide breaker outcomes - so it now lives here beside the trait it is thrown
//! across. The name is retained deliberately: it appears in `executor.rs`'s
//! `classify_drive_error`, in the restore command's error mapping, and in the
//! SPEC s24 dotted codes embedded in its `Display` text, and renaming it would
//! be churn with no functional gain. `driven_drive::google` re-exports it, so
//! every pre-existing `driven_drive::google::DriveError` path still resolves.

use crate::remote_store::DriveErrorClassification;
use crate::retry;

/// A classified remote-store error (SPEC s24 error taxonomy).
///
/// Carries a [`DriveErrorClassification`] (re-used from
/// [`crate::remote_store`]) so the executor / pacer / circuit-breakers can
/// decide breaker outcomes by downcasting an [`anyhow::Error`] back to this
/// type rather than string-matching the message (CODEX_NOTES: "Drive circuit
/// breaker driven by real request outcomes"). Surfaced through `anyhow` at
/// the trait boundary; recover the classification with
/// [`classification_of`].
///
/// The `Display` text deliberately EMBEDS the SPEC s24 dotted error code as a
/// literal substring (`drive.rate_limited`, `auth.invalid_grant`, ...) so the
/// M3 executor's `classify_drive_error`, which still classifies by
/// case-sensitive substring on `e.to_string()` for stores that do not emit this
/// type (the `InMemoryRemoteStore` fake), classifies a real-store error the
/// same way it classifies the fake's messages. Both paths therefore agree.
///
/// `Display`/`Error` are hand-written (not `thiserror`-derived) so the
/// `Classified` message can match on its `kind` field directly - emitting the
/// right SPEC s24 dotted code - without relying on a function-call-in-attribute
/// expansion.
#[derive(Debug)]
pub enum DriveError {
    /// A classified API/transport failure (429 / 5xx / network / auth /
    /// quota / other). The variant payload IS the pacer/breaker verdict.
    Classified {
        /// How the pacer + circuit breaker should treat this failure.
        kind: DriveErrorClassification,
        /// The underlying cause (HTTP status, transport error, parse error).
        source: anyhow::Error,
    },
    /// The configured destination folder was deleted from the remote (SPEC s24
    /// `drive.dest_folder_missing`).
    DestFolderMissing,
    /// The destination folder's sharing changed to read-only for this
    /// account (SPEC s24 `drive.dest_folder_permission_denied`).
    DestFolderPermissionDenied,
    /// A resumable upload session returned a 4xx mid-chunk; the caller must
    /// restart from offset 0 (SPEC s24 `drive.resumable_session_invalid`).
    ResumableSessionInvalid,
    /// Verification of the uploaded bytes failed: the remote's content digest
    /// did not match the bytes Driven sent (SPEC s24
    /// `drive.checksum_mismatch`).
    ///
    /// `stranded_file_id` is `Some(file_id)` ONLY when this was a CREATE whose
    /// corrupt new object was materialized remotely AND the store's best-effort
    /// `trash()` of it ALSO failed - so a live corrupt object may still exist
    /// remotely (codex C5-P1-4). The executor persists that id
    /// (`corrupt_file_id`) and KEEPS the pending op so reconcile retries the
    /// trash. `None` means the corrupt object is confirmed gone (trash
    /// succeeded), or it was an UPDATE (whose file_id is the user's
    /// pre-existing good file - never trashed), or no object materialized (a
    /// streamed-session mismatch).
    ChecksumMismatch {
        /// `Some` when a corrupt CREATE object may still be live remotely
        /// (its trash failed) and the executor must keep the op to retry it.
        stranded_file_id: Option<String>,
    },
}

impl std::fmt::Display for DriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriveError::Classified { kind, .. } => write!(f, "{}", classified_message(kind)),
            DriveError::DestFolderMissing => {
                write!(f, "drive.dest_folder_missing: destination folder is missing")
            }
            DriveError::DestFolderPermissionDenied => write!(
                f,
                "drive.dest_folder_permission_denied: destination folder is read-only for this account"
            ),
            DriveError::ResumableSessionInvalid => write!(
                f,
                "drive.resumable_session_invalid: resumable session is invalid; restart required"
            ),
            DriveError::ChecksumMismatch { stranded_file_id } => match stranded_file_id {
                Some(id) => write!(
                    f,
                    "drive.checksum_mismatch: md5 mismatch after upload; corrupt object {id} could not be trashed"
                ),
                None => write!(f, "drive.checksum_mismatch: md5 mismatch after upload"),
            },
        }
    }
}

impl std::error::Error for DriveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // The `anyhow::Error` source is surfaced as the error chain so the
            // `error_is_not_found` helper (and any caller) can walk to the
            // `drive HTTP <status>` cause. `anyhow::Error` impls
            // `AsRef<dyn Error + Send + Sync>`; coerce off the auto traits to
            // the `source()` return type.
            DriveError::Classified { source, .. } => {
                let std_err: &(dyn std::error::Error + Send + Sync + 'static) = source.as_ref();
                Some(std_err)
            }
            _ => None,
        }
    }
}

/// Builds the `Display` text for a [`DriveError::Classified`], embedding the
/// SPEC s24 dotted code so BOTH the downcast path ([`classification_of`]) and
/// the M3 string-substring matcher (`executor.rs::classify_drive_error`)
/// agree on the class. The matcher tests `daily` before `quota_exhausted`, so
/// the daily-quota code must contain `daily` (it does:
/// `drive.daily_quota_exhausted`).
fn classified_message(kind: &DriveErrorClassification) -> String {
    match kind {
        DriveErrorClassification::RateLimited { retry_after_ms } => {
            format!("drive.rate_limited (retry_after_ms={retry_after_ms})")
        }
        DriveErrorClassification::Transient5xx => {
            "drive.unreachable: transient 5xx from Drive".to_string()
        }
        DriveErrorClassification::Network => {
            "net.intermittent: drive request network/transport error".to_string()
        }
        DriveErrorClassification::AuthInvalidGrant => {
            "auth.invalid_grant: refresh token revoked; reauth required".to_string()
        }
        DriveErrorClassification::DailyQuota => {
            "drive.daily_quota_exhausted: 403 dailyLimitExceeded".to_string()
        }
        DriveErrorClassification::StorageQuota => {
            "drive.quota_exhausted: 403 storageQuotaExceeded".to_string()
        }
        DriveErrorClassification::Other => {
            "drive.unreachable: unclassified Drive error".to_string()
        }
    }
}

impl DriveError {
    /// The [`DriveErrorClassification`] this error implies, for the pacer +
    /// circuit breaker. Non-[`DriveError::Classified`] variants map to their
    /// natural class ([`DriveErrorClassification::Other`] for the fatal
    /// dest-folder / checksum / session-invalid cases).
    pub fn classification(&self) -> DriveErrorClassification {
        match self {
            DriveError::Classified { kind, .. } => kind.clone(),
            DriveError::DestFolderMissing
            | DriveError::DestFolderPermissionDenied
            | DriveError::ResumableSessionInvalid
            | DriveError::ChecksumMismatch { .. } => DriveErrorClassification::Other,
        }
    }

    /// Builds a classified error from an HTTP status + body, mapping the
    /// status/reason to the SPEC s24 class via [`retry::classify_response`].
    /// The dest-folder-missing / permission-denied 404/403 cases against the
    /// destination folder are promoted to their dedicated fatal variants by
    /// the caller (which knows it was a write against the dest folder).
    pub fn from_response(status: u16, body: &[u8], retry_after_ms: Option<u64>) -> Self {
        Self::from_classified_response(
            retry::classify_response(status, body, retry_after_ms),
            status,
            body,
        )
    }

    /// Builds a classified error from an ALREADY-classified HTTP response.
    ///
    /// Backends whose error envelope is not Google's JSON shape (the S3 XML
    /// `<Error><Code>` document, for instance) classify with their own mapper
    /// and hand the verdict here, so the `source` chain and the SPEC s24
    /// `Display` text stay identical across backends.
    pub fn from_classified_response(
        kind: DriveErrorClassification,
        status: u16,
        body: &[u8],
    ) -> Self {
        DriveError::Classified {
            kind,
            source: anyhow::anyhow!(
                "drive HTTP {status}: {}",
                String::from_utf8_lossy(body)
                    .chars()
                    .take(512)
                    .collect::<String>()
            ),
        }
    }

    /// Builds a classified transport-error from a `reqwest::Error`.
    pub fn from_transport(err: reqwest::Error) -> Self {
        let kind = retry::classify_transport_error(&err);
        DriveError::Classified {
            kind,
            source: anyhow::Error::new(err),
        }
    }
}

/// Reads the [`DriveErrorClassification`] off an [`anyhow::Error`] the trait
/// boundary surfaced, if it originated as a [`DriveError`] (the executor
/// downcasts to decide breaker outcomes; CODEX_NOTES "Drive circuit breaker
/// driven by real request outcomes"). Returns `None` for any other error.
pub fn classification_of(err: &anyhow::Error) -> Option<DriveErrorClassification> {
    err.downcast_ref::<DriveError>()
        .map(DriveError::classification)
}

/// R1-P2-1: classify a MID-STREAM download READ error surfaced as a
/// [`std::io::Error`]. A streaming download reader wraps a transport failure
/// that occurs WHILE the body is being read via
/// `std::io::Error::other(reqwest_error)` - so the real cause (a network drop /
/// timeout / connection reset mid-body) is preserved as the io error's inner
/// source rather than as a classified [`DriveError`]. The restore sink must NOT
/// report such a failure as `local.io_error` (the DISK is fine; the
/// remote/network failed). This walks the io error's source chain looking for
/// either a [`DriveError`] (if a future path wraps one) or a raw
/// `reqwest::Error`, and returns its [`DriveErrorClassification`]; `None` if the
/// io error is a genuine LOCAL disk error with no remote/network cause (the
/// caller then keeps the local classification).
#[must_use]
pub fn classify_stream_read_error(err: &std::io::Error) -> Option<DriveErrorClassification> {
    // The reader wraps the cause with `io::Error::other(e)`, so the inner error is
    // reachable via `get_ref()` (and any deeper cause via its `source()` chain).
    let mut cause: Option<&(dyn std::error::Error + 'static)> = err.get_ref().map(|e| e as _);
    while let Some(c) = cause {
        if let Some(drive) = c.downcast_ref::<DriveError>() {
            return Some(drive.classification());
        }
        if let Some(req) = c.downcast_ref::<reqwest::Error>() {
            return Some(retry::classify_transport_error(req));
        }
        cause = c.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_of_round_trips() {
        let e = anyhow::Error::new(DriveError::Classified {
            kind: DriveErrorClassification::Transient5xx,
            source: anyhow::anyhow!("boom"),
        });
        assert_eq!(
            classification_of(&e),
            Some(DriveErrorClassification::Transient5xx)
        );
        assert_eq!(classification_of(&anyhow::anyhow!("plain")), None);
    }

    #[test]
    fn from_classified_response_keeps_the_caller_verdict() {
        let e = DriveError::from_classified_response(
            DriveErrorClassification::RateLimited {
                retry_after_ms: 1500,
            },
            503,
            b"<Error><Code>SlowDown</Code></Error>",
        );
        assert_eq!(
            e.classification(),
            DriveErrorClassification::RateLimited {
                retry_after_ms: 1500
            }
        );
        // The SPEC s24 dotted code must be embedded in Display for the
        // substring matcher.
        assert!(e.to_string().contains("drive.rate_limited"));
        // The body is preserved (truncated) in the source chain.
        let src = std::error::Error::source(&e).map(|s| s.to_string());
        assert!(src.unwrap_or_default().contains("SlowDown"));
    }

    #[test]
    fn fatal_variants_classify_as_other() {
        for e in [
            DriveError::DestFolderMissing,
            DriveError::DestFolderPermissionDenied,
            DriveError::ResumableSessionInvalid,
            DriveError::ChecksumMismatch {
                stranded_file_id: None,
            },
        ] {
            assert_eq!(e.classification(), DriveErrorClassification::Other);
        }
    }
}
