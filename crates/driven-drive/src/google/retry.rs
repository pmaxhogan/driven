//! Exponential-backoff + error-classification middleware for Drive requests
//! (SPEC s9, s24; DESIGN s5.4 / s5.8.3; ROADMAP M4).
//!
//! Every Drive request flows through [`with_retry`], which classifies the
//! response into a [`DriveErrorClassification`] and retries the transient
//! classes (429 with `Retry-After`, 5xx, network) with exponential backoff,
//! while surfacing the fatal classes (auth / quota / dest-folder) straight
//! to the caller as a [`DriveError`] carrying its classification.
//!
//! M4 scaffold: signatures only; bodies are `todo!()`.

use std::future::Future;

use crate::remote_store::DriveErrorClassification;

/// Max retries for transient (429 / 5xx / network) classes before giving up
/// (DESIGN s5.4 "5xx -> exponential backoff, max 6 retries").
pub const MAX_RETRIES: u32 = 6;

/// Classifies a Drive HTTP response into the pacer/breaker verdict (SPEC s24).
///
/// Maps `429`/`userRateLimitExceeded` -> [`DriveErrorClassification::RateLimited`]
/// (carrying the `Retry-After`), `5xx` -> [`DriveErrorClassification::Transient5xx`],
/// `401 invalid_grant` -> [`DriveErrorClassification::AuthInvalidGrant`],
/// `403 dailyLimitExceeded` -> [`DriveErrorClassification::DailyQuota`],
/// `403 storageQuotaExceeded` -> [`DriveErrorClassification::StorageQuota`],
/// and everything else -> [`DriveErrorClassification::Other`].
pub fn classify_response(status: u16, body: &[u8]) -> DriveErrorClassification {
    let _ = (status, body);
    todo!("M4 implement: classify a Drive HTTP status + body into DriveErrorClassification")
}

/// Classifies a transport-level `reqwest` error (DNS, connect, TLS, timeout)
/// as [`DriveErrorClassification::Network`] (SPEC s24 `net.*`).
pub fn classify_transport_error(err: &reqwest::Error) -> DriveErrorClassification {
    let _ = err;
    todo!("M4 implement: classify a reqwest transport error as Network")
}

/// Runs `make_request` with exponential-backoff retry for the transient
/// classes (SPEC s9, DESIGN s5.4). `make_request` is re-invoked per attempt
/// (so a fresh request is sent each retry); the fatal classes short-circuit.
///
/// Generic over the request factory + its `Output` so it wraps any Drive call
/// (metadata GET, multipart create, resumable chunk push, ...).
pub async fn with_retry<F, Fut, T>(make_request: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    let _ = (&make_request, MAX_RETRIES);
    todo!("M4 implement: exponential-backoff retry loop over the transient classes")
}
