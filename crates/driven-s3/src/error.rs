//! Mapping S3's XML error document onto Driven's SPEC s24 classification.
//!
//! The shared [`driven_remote::retry::classify_response`] reads Google's JSON
//! `{"error":{"errors":[{"reason":..}]}}` envelope; its status-only rules (429,
//! 5xx, 401) are protocol-independent and still correct here, but S3 encodes the
//! interesting distinctions in an XML `<Error><Code>` document AND uses statuses
//! differently - most notably `503 SlowDown`, which is throttling wearing a 5xx
//! costume, and `403`, which covers both "wrong key" and "no permission".
//!
//! So this module classifies the S3 code first and falls back to the shared
//! status rules. The resulting verdict is handed to
//! [`driven_remote::DriveError::from_classified_response`], so the error the
//! executor sees is the same type, with the same SPEC s24 dotted `Display`
//! text, as one from the Drive backend.

use driven_remote::remote_store::DriveErrorClassification;
use driven_remote::{retry, DriveError};

/// Default backoff to advertise for a throttling response that carries no
/// `Retry-After` header. S3 throttling is short-lived; the retry loop grows this
/// exponentially anyway, so the floor only has to be non-zero.
const DEFAULT_THROTTLE_RETRY_AFTER_MS: u64 = 1_000;

/// Extract the `<Code>` from an S3 error document, if present.
///
/// A deliberately minimal scan rather than a full XML parse: the body may be
/// truncated, may not be XML at all (a proxy's HTML error page), and the only
/// thing needed is the code. Case-sensitive, because S3 codes are PascalCase and
/// a case-insensitive scan would match unrelated prose in an HTML body.
fn error_code(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let start = text.find("<Code>")? + "<Code>".len();
    let end = text[start..].find("</Code>")? + start;
    let code = text[start..end].trim();
    if code.is_empty() {
        None
    } else {
        Some(code.to_string())
    }
}

/// Classify an S3 HTTP response into the pacer / circuit-breaker verdict.
pub fn classify_s3_response(
    status: u16,
    body: &[u8],
    retry_after_ms: Option<u64>,
) -> DriveErrorClassification {
    let code = error_code(body);
    if let Some(code) = code.as_deref() {
        match code {
            // Throttling. `SlowDown` arrives as a 503 and `RequestTimeout` as a
            // 400, so neither would be read as rate limiting by status alone.
            "SlowDown"
            | "TooManyRequests"
            | "RequestLimitExceeded"
            | "Throttling"
            | "ThrottlingException" => {
                return DriveErrorClassification::RateLimited {
                    retry_after_ms: retry_after_ms.unwrap_or(DEFAULT_THROTTLE_RETRY_AFTER_MS),
                }
            }
            // Transient, retryable: the request never landed.
            "RequestTimeout" | "InternalError" | "ServiceUnavailable" | "OperationAborted" => {
                return DriveErrorClassification::Transient5xx
            }
            // The credential is wrong / expired / lacks permission. Mapping this
            // to AuthInvalidGrant is what moves the account to `needs_reauth`,
            // which is exactly the right prompt for an S3 destination: the user
            // must supply a working access key.
            "InvalidAccessKeyId"
            | "SignatureDoesNotMatch"
            | "AccessDenied"
            | "InvalidSecurity"
            | "ExpiredToken"
            | "TokenRefreshRequired"
            | "UnauthorizedAccess" => return DriveErrorClassification::AuthInvalidGrant,
            // The account/bucket is out of room.
            "QuotaExceeded" | "ServiceQuotaExceeded" => {
                return DriveErrorClassification::StorageQuota
            }
            // A skewed clock breaks every signature until it is fixed; retrying
            // the same request cannot help, so it is fatal-for-this-op rather
            // than transient.
            "RequestTimeTooSkewed" => return DriveErrorClassification::Other,
            _ => {}
        }
    }

    // No recognised S3 code: fall back to the shared status-only rules (429 ->
    // rate limited, 5xx -> transient, 401 -> auth, everything else -> Other),
    // which are protocol-independent.
    retry::classify_response(status, body, retry_after_ms)
}

/// Build the classified [`DriveError`] for an S3 HTTP failure.
pub fn s3_error_from_response(status: u16, body: &[u8], retry_after_ms: Option<u64>) -> DriveError {
    // Promote the two "the destination itself is gone / unreachable" cases to
    // their dedicated fatal variants, which the app surfaces with a specific
    // remedy rather than a generic transfer error.
    match error_code(body).as_deref() {
        Some("NoSuchBucket") => return DriveError::DestFolderMissing,
        // A 403 on a bucket-scoped operation with an otherwise-valid signature
        // is the S3 shape of "your destination went read-only".
        Some("AllAccessDisabled") => return DriveError::DestFolderPermissionDenied,
        _ => {}
    }
    DriveError::from_classified_response(
        classify_s3_response(status, body, retry_after_ms),
        status,
        body,
    )
}

/// Whether an error chain came from a definitive "the object is not there"
/// response (a 404, or an S3 `NoSuchKey`).
///
/// [`DriveError::from_classified_response`] embeds the literal
/// `drive HTTP <status>` plus the (truncated) body in the source chain, so both
/// signals are recoverable from an `anyhow::Error` that has crossed the trait
/// boundary. Used to make `trash` / `delete_permanent` idempotent.
pub fn is_not_found(err: &anyhow::Error) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(err.as_ref());
    while let Some(c) = cause {
        let msg = c.to_string();
        if msg.contains("drive HTTP 404") || msg.contains("<Code>NoSuchKey</Code>") {
            return true;
        }
        cause = c.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xml(code: &str) -> Vec<u8> {
        format!("<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>m</Message></Error>")
            .into_bytes()
    }

    #[test]
    fn slow_down_is_rate_limiting_despite_its_503_status() {
        // The single most important case: S3 throttles with a 503, which the
        // status-only rules would call a transient 5xx and give up on after
        // MAX_RETRIES. Rate limiting retries indefinitely, which is what a
        // throttled backup needs.
        assert_eq!(
            classify_s3_response(503, &xml("SlowDown"), None),
            DriveErrorClassification::RateLimited {
                retry_after_ms: DEFAULT_THROTTLE_RETRY_AFTER_MS
            }
        );
        assert_eq!(
            classify_s3_response(503, &xml("SlowDown"), Some(2_500)),
            DriveErrorClassification::RateLimited {
                retry_after_ms: 2_500
            }
        );
    }

    #[test]
    fn request_timeout_is_transient_despite_its_400_status() {
        assert_eq!(
            classify_s3_response(400, &xml("RequestTimeout"), None),
            DriveErrorClassification::Transient5xx
        );
    }

    #[test]
    fn credential_failures_map_to_auth_invalid_grant() {
        for code in [
            "InvalidAccessKeyId",
            "SignatureDoesNotMatch",
            "AccessDenied",
            "ExpiredToken",
        ] {
            assert_eq!(
                classify_s3_response(403, &xml(code), None),
                DriveErrorClassification::AuthInvalidGrant,
                "{code} must move the account to needs_reauth"
            );
        }
    }

    #[test]
    fn quota_exhaustion_maps_to_storage_quota() {
        assert_eq!(
            classify_s3_response(400, &xml("QuotaExceeded"), None),
            DriveErrorClassification::StorageQuota
        );
    }

    #[test]
    fn clock_skew_is_fatal_not_transient() {
        assert_eq!(
            classify_s3_response(403, &xml("RequestTimeTooSkewed"), None),
            DriveErrorClassification::Other
        );
    }

    #[test]
    fn unrecognised_bodies_fall_back_to_the_shared_status_rules() {
        assert_eq!(
            classify_s3_response(429, b"", None),
            DriveErrorClassification::RateLimited { retry_after_ms: 0 }
        );
        assert_eq!(
            classify_s3_response(500, b"<html>gateway</html>", None),
            DriveErrorClassification::Transient5xx
        );
        assert_eq!(
            classify_s3_response(404, &xml("NoSuchKey"), None),
            DriveErrorClassification::Other
        );
    }

    #[test]
    fn missing_bucket_and_disabled_access_get_dedicated_variants() {
        assert!(matches!(
            s3_error_from_response(404, &xml("NoSuchBucket"), None),
            DriveError::DestFolderMissing
        ));
        assert!(matches!(
            s3_error_from_response(403, &xml("AllAccessDisabled"), None),
            DriveError::DestFolderPermissionDenied
        ));
    }

    #[test]
    fn not_found_is_detected_from_status_and_from_the_s3_code() {
        let by_status = anyhow::Error::new(s3_error_from_response(404, b"", None));
        assert!(is_not_found(&by_status));

        // MinIO answers a delete of a missing key with 204, but a HEAD with 404
        // and no body; R2 includes the code. Both must read as "already gone".
        let by_code = anyhow::Error::new(s3_error_from_response(400, &xml("NoSuchKey"), None));
        assert!(is_not_found(&by_code));

        let other = anyhow::Error::new(s3_error_from_response(500, b"boom", None));
        assert!(!is_not_found(&other));
        assert!(!is_not_found(&anyhow::anyhow!("a plain error")));
    }

    #[test]
    fn error_code_extraction_tolerates_junk() {
        assert_eq!(error_code(&xml("SlowDown")).as_deref(), Some("SlowDown"));
        assert_eq!(error_code(b"").as_deref(), None);
        assert_eq!(error_code(b"<Error><Code></Code>").as_deref(), None);
        assert_eq!(error_code(b"<html>Code</html>").as_deref(), None);
        // A truncated document (the body is capped at 512 chars downstream).
        assert_eq!(error_code(b"<Error><Code>SlowDo").as_deref(), None);
    }
}
