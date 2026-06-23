//! Exponential-backoff + error-classification middleware for Drive requests
//! (SPEC s9, s24; DESIGN s5.4 / s5.8.3; ROADMAP M4).
//!
//! Every Drive request flows through [`with_retry`], which classifies the
//! response into a [`DriveErrorClassification`] and retries the transient
//! classes (429 with `Retry-After`, 5xx, network) with exponential backoff,
//! while surfacing the fatal classes (auth / quota / dest-folder) straight
//! to the caller as a [`DriveError`] carrying its classification.
//!
//! Retry policy (DESIGN s5.4 "Retry semantics"):
//! - `429` / `403 userRateLimitExceeded` / `403 rateLimitExceeded` ->
//!   exponential backoff with jitter (1s, 2s, 4s, 8s, capped 60s, honour
//!   `Retry-After`), retried INDEFINITELY (the limit is recoverable).
//! - `5xx` -> exponential backoff, max [`MAX_RETRIES`] attempts.
//! - network/transport errors -> exponential backoff, max [`MAX_RETRIES`].
//! - `401 invalid_grant` / `400 invalidGrant` -> `auth.invalid_grant`, fatal
//!   (the account moves to `needs_reauth`).
//! - `403 dailyLimitExceeded` / `403 quotaExceeded` -> daily quota, fatal for
//!   this op (the pacer pauses the account until midnight Pacific).
//! - `403 storageQuotaExceeded` -> storage quota, fatal.
//! - any other `4xx` -> `Other`, fatal for this op.

use std::future::Future;
use std::time::Duration;

use crate::remote_store::DriveErrorClassification;

/// Max retries for transient (5xx / network) classes before giving up
/// (DESIGN s5.4 "5xx -> exponential backoff, max 6 retries"). Rate-limit
/// classes are NOT bounded by this - they retry indefinitely.
pub const MAX_RETRIES: u32 = 6;

/// Base backoff before the first retry (DESIGN s5.4: 1s, 2s, 4s, 8s, ...).
const BASE_BACKOFF: Duration = Duration::from_secs(1);

/// Backoff ceiling (DESIGN s5.4 "capped 60s").
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Classifies a Drive HTTP response into the pacer/breaker verdict (SPEC s24).
///
/// Maps `429`/`userRateLimitExceeded`/`rateLimitExceeded` ->
/// [`DriveErrorClassification::RateLimited`] (carrying any `Retry-After`),
/// `5xx` -> [`DriveErrorClassification::Transient5xx`],
/// `401`/`400 invalid_grant` -> [`DriveErrorClassification::AuthInvalidGrant`],
/// `403 dailyLimitExceeded`/`quotaExceeded` ->
/// [`DriveErrorClassification::DailyQuota`],
/// `403 storageQuotaExceeded` -> [`DriveErrorClassification::StorageQuota`],
/// and everything else -> [`DriveErrorClassification::Other`].
///
/// `retry_after_ms` is the `Retry-After` header value (in ms) when present;
/// the classifier folds it into the [`DriveErrorClassification::RateLimited`]
/// variant so the pacer can honour it.
pub fn classify_response(
    status: u16,
    body: &[u8],
    retry_after_ms: Option<u64>,
) -> DriveErrorClassification {
    // The `reason` field inside Drive's error JSON is the authoritative
    // discriminator for the 403 family; fall back to a body substring scan so
    // a non-JSON 403 body still classifies. Lowercased once for cheap
    // case-insensitive matching against Drive's mixed-case reasons.
    let reason = parse_error_reason(body);
    let body_lower = String::from_utf8_lossy(body).to_ascii_lowercase();
    let has = |needle: &str| {
        reason
            .as_deref()
            .map(|r| r.eq_ignore_ascii_case(needle))
            .unwrap_or(false)
            || body_lower.contains(&needle.to_ascii_lowercase())
    };

    match status {
        429 => DriveErrorClassification::RateLimited {
            retry_after_ms: retry_after_ms.unwrap_or(0),
        },
        401 => DriveErrorClassification::AuthInvalidGrant,
        403 => {
            // Order matters: the rate-limit reasons are recoverable and must
            // be distinguished from the hard daily / storage caps. Crucially,
            // `storageQuotaExceeded` CONTAINS the substring `quotaExceeded`, so
            // the storage check MUST precede the daily check - otherwise the
            // bare-`quotaExceeded` daily fallback (which also scans the body as
            // a substring) would steal a storageQuotaExceeded response and
            // mis-map a full-Drive error to DailyQuota (SPEC s24
            // drive.quota_exhausted vs drive.daily_quota_exhausted).
            if has("userRateLimitExceeded") || has("rateLimitExceeded") {
                DriveErrorClassification::RateLimited {
                    retry_after_ms: retry_after_ms.unwrap_or(0),
                }
            } else if has("storageQuotaExceeded") {
                DriveErrorClassification::StorageQuota
            } else if has("dailyLimitExceeded") || has("quotaExceeded") {
                DriveErrorClassification::DailyQuota
            } else if has("invalid_grant") || has("invalidGrant") {
                DriveErrorClassification::AuthInvalidGrant
            } else {
                DriveErrorClassification::Other
            }
        }
        400 => {
            if has("invalid_grant") || has("invalidGrant") {
                DriveErrorClassification::AuthInvalidGrant
            } else {
                DriveErrorClassification::Other
            }
        }
        s if (500..=599).contains(&s) => DriveErrorClassification::Transient5xx,
        _ => DriveErrorClassification::Other,
    }
}

/// Pulls the first `errors[].reason` (or top-level `error.status`) out of a
/// Drive error JSON body, if it parses. Returns `None` for a non-JSON body
/// (the caller then falls back to a substring scan).
fn parse_error_reason(body: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    // Classic Drive error shape: { "error": { "errors": [ { "reason": ".." } ], "status": ".." } }
    let error = v.get("error")?;
    if let Some(reason) = error
        .get("errors")
        .and_then(|e| e.as_array())
        .and_then(|a| a.first())
        .and_then(|e0| e0.get("reason"))
        .and_then(|r| r.as_str())
    {
        return Some(reason.to_string());
    }
    // OAuth token-endpoint shape: { "error": "invalid_grant", ... }
    if let Some(s) = error.as_str() {
        return Some(s.to_string());
    }
    error
        .get("status")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
}

/// Classifies a transport-level `reqwest` error (DNS, connect, TLS, timeout)
/// as [`DriveErrorClassification::Network`] (SPEC s24 `net.*`).
pub fn classify_transport_error(err: &reqwest::Error) -> DriveErrorClassification {
    // Every transport-level failure (connect refused, TLS handshake, DNS, idle
    // RST, request timeout) is a recoverable network class; the breaker +
    // backoff absorb it. We do not sub-divide here because the trait boundary
    // surfaces one `Network` verdict (the network-probe topology owns the
    // finer DNS/captive distinctions, DESIGN s5.8).
    let _ = err;
    DriveErrorClassification::Network
}

/// Whether a classification is transient (worth retrying). Rate-limited and
/// network/5xx are transient; auth / quota / other are fatal for the op.
fn is_transient(class: &DriveErrorClassification) -> bool {
    matches!(
        class,
        DriveErrorClassification::RateLimited { .. }
            | DriveErrorClassification::Transient5xx
            | DriveErrorClassification::Network
    )
}

/// The backoff for retry attempt `attempt` (0-based), with full jitter,
/// honouring a `Retry-After` hint for the rate-limit class.
///
/// `2^attempt` seconds (1s, 2s, 4s, 8s, ...) capped at [`MAX_BACKOFF`], then
/// "full jitter" (a uniform sample in `[0, computed]`) to avoid thundering-herd
/// alignment (DESIGN s5.4 "with jitter"). A `Retry-After` floor (the server's
/// explicit hint) is applied on top so we never retry sooner than Drive asked.
fn backoff_for(attempt: u32, retry_after_ms: Option<u64>) -> Duration {
    let exp = BASE_BACKOFF
        .checked_mul(1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX))
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF);
    let jittered = full_jitter(exp);
    match retry_after_ms {
        Some(ms) if ms > 0 => jittered.max(Duration::from_millis(ms)),
        _ => jittered,
    }
}

/// Full-jitter sample in `[0, d]` using a cheap, dependency-free RNG seeded
/// from the wall clock + the duration. We do not need cryptographic jitter -
/// only enough spread to de-correlate concurrent retriers.
fn full_jitter(d: Duration) -> Duration {
    let span_ms = d.as_millis() as u64;
    if span_ms == 0 {
        return Duration::ZERO;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.as_nanos() as u64)
        .unwrap_or(0)
        ^ span_ms.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // SplitMix64 step - one multiply/xor is plenty for jitter.
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    Duration::from_millis(z % (span_ms + 1))
}

// The outcome the retry loop needs from one `make_request` attempt: either a
// success value, or a classified failure the loop decides to retry or
// surface. `make_request` returns an `anyhow::Error` whose root cause, when a
// retryable class, MUST be downcastable via `retry_after_of` /
// `classification_in` so the loop can read the verdict.
//
// We keep `with_retry` generic over `anyhow::Result<T>` (the trait boundary's
// currency) and read the classification back off the error, rather than
// forcing every caller into a bespoke result type.

/// Reads the [`DriveErrorClassification`] off an error if it carries one (a
/// [`crate::google::DriveError`]). Returns `None` for an un-classified error
/// (which the loop then treats as fatal - we never retry a verdict we cannot
/// read, to avoid hammering on a genuine bug).
fn classification_in(err: &anyhow::Error) -> Option<DriveErrorClassification> {
    crate::google::classification_of(err)
}

/// Runs `make_request` with exponential-backoff retry for the transient
/// classes (SPEC s9, DESIGN s5.4). `make_request` is re-invoked per attempt
/// (so a fresh request is sent each retry); the fatal classes short-circuit.
///
/// Generic over the request factory + its `Output` so it wraps any Drive call
/// (metadata GET, multipart create, resumable chunk push, ...). The error the
/// factory returns must be a [`crate::google::DriveError`] (so its class is
/// readable); a non-classified error is treated as fatal and returned as-is.
///
/// Rate-limit failures retry indefinitely (DESIGN s5.4); 5xx / network
/// failures retry up to [`MAX_RETRIES`] then surface the last error.
pub async fn with_retry<F, Fut, T>(make_request: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let mut transient_attempt: u32 = 0;
    loop {
        match make_request().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let class = match classification_in(&e) {
                    Some(c) => c,
                    // Unreadable verdict -> fatal. Surface as-is rather than
                    // risk an unbounded retry of a programming error.
                    None => return Err(e),
                };
                if !is_transient(&class) {
                    return Err(e);
                }
                match class {
                    DriveErrorClassification::RateLimited { retry_after_ms } => {
                        // Recoverable; retry indefinitely. `transient_attempt`
                        // still climbs (capped inside `backoff_for`) so the
                        // backoff grows, but we never give up.
                        let delay = backoff_for(transient_attempt, Some(retry_after_ms));
                        transient_attempt = transient_attempt.saturating_add(1);
                        tokio::time::sleep(delay).await;
                    }
                    DriveErrorClassification::Transient5xx | DriveErrorClassification::Network => {
                        if transient_attempt >= MAX_RETRIES {
                            return Err(e);
                        }
                        let delay = backoff_for(transient_attempt, None);
                        transient_attempt = transient_attempt.saturating_add(1);
                        tokio::time::sleep(delay).await;
                    }
                    // Unreachable: filtered by `is_transient` above.
                    _ => return Err(e),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use crate::google::DriveError;

    #[test]
    fn classify_429_is_rate_limited_with_retry_after() {
        let c = classify_response(429, b"", Some(1200));
        assert_eq!(
            c,
            DriveErrorClassification::RateLimited {
                retry_after_ms: 1200
            }
        );
    }

    #[test]
    fn classify_403_user_rate_limit_is_rate_limited() {
        let body = br#"{"error":{"errors":[{"reason":"userRateLimitExceeded"}],"code":403}}"#;
        let c = classify_response(403, body, None);
        assert_eq!(
            c,
            DriveErrorClassification::RateLimited { retry_after_ms: 0 }
        );
    }

    #[test]
    fn classify_403_daily_is_daily_quota_not_storage() {
        let body = br#"{"error":{"errors":[{"reason":"dailyLimitExceeded"}],"code":403}}"#;
        assert_eq!(
            classify_response(403, body, None),
            DriveErrorClassification::DailyQuota
        );
    }

    #[test]
    fn classify_403_storage_quota() {
        let body = br#"{"error":{"errors":[{"reason":"storageQuotaExceeded"}],"code":403}}"#;
        assert_eq!(
            classify_response(403, body, None),
            DriveErrorClassification::StorageQuota
        );
    }

    #[test]
    fn classify_401_is_invalid_grant() {
        assert_eq!(
            classify_response(401, b"", None),
            DriveErrorClassification::AuthInvalidGrant
        );
    }

    #[test]
    fn classify_400_invalid_grant_body() {
        let body = br#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#;
        assert_eq!(
            classify_response(400, body, None),
            DriveErrorClassification::AuthInvalidGrant
        );
    }

    #[test]
    fn classify_5xx_is_transient() {
        for s in [500u16, 502, 503, 504] {
            assert_eq!(
                classify_response(s, b"", None),
                DriveErrorClassification::Transient5xx
            );
        }
    }

    #[test]
    fn classify_404_is_other() {
        assert_eq!(
            classify_response(404, b"", None),
            DriveErrorClassification::Other
        );
    }

    #[tokio::test(start_paused = true)]
    async fn with_retry_succeeds_after_transient_5xx() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let out: anyhow::Result<u32> = with_retry(|| {
            let calls = calls2.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(anyhow::Error::new(DriveError::Classified {
                        kind: DriveErrorClassification::Transient5xx,
                        source: anyhow::anyhow!("simulated 503"),
                    }))
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn with_retry_gives_up_after_max_5xx() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let out: anyhow::Result<u32> = with_retry(|| {
            let calls = calls2.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::Error::new(DriveError::Classified {
                    kind: DriveErrorClassification::Transient5xx,
                    source: anyhow::anyhow!("always 503"),
                }))
            }
        })
        .await;
        assert!(out.is_err());
        // initial attempt + MAX_RETRIES retries.
        assert_eq!(calls.load(Ordering::SeqCst), MAX_RETRIES + 1);
    }

    #[tokio::test(start_paused = true)]
    async fn with_retry_does_not_retry_fatal() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let out: anyhow::Result<u32> = with_retry(|| {
            let calls = calls2.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::Error::new(DriveError::Classified {
                    kind: DriveErrorClassification::AuthInvalidGrant,
                    source: anyhow::anyhow!("invalid_grant"),
                }))
            }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "fatal class is not retried"
        );
    }
}
