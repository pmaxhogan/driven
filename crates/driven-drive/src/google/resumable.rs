//! The Drive resumable-upload session protocol helpers (SPEC s3, ROADMAP M4;
//! https://developers.google.com/drive/api/guides/manage-uploads#resumable).
//!
//! [`GoogleDriveStore`](super::GoogleDriveStore) composes these to open a
//! resumable session (create vs update), push 256-KiB-multiple non-final
//! chunks, and finalize. Error handling per chunk (DESIGN s5.4):
//! - a 4xx (other than 308) kills the session
//!   ([`ResumeProgress::SessionInvalid`]); the caller restarts from offset 0
//!   (NEVER resume the old session URL after a 4xx).
//! - a 5xx / transport error is TRANSIENT at the session level: the caller's
//!   retry wrapper re-queries the SAME session offset ([`query_offset`]) and
//!   re-pushes with bounded exponential backoff (it does NOT restart from 0).
//! - a 308 with no acknowledged `Range` re-queries the confirmed offset rather
//!   than assuming the chunk landed, so a crash never persists a false offset.
//!
//! Protocol (Drive resumable upload):
//! 1. `POST /upload/drive/v3/files?uploadType=resumable` (create) or
//!    `PATCH /upload/drive/v3/files/{id}?uploadType=resumable` (update) with
//!    the JSON metadata body; Drive returns the session URL in the `Location`
//!    header.
//! 2. `PUT <session-url>` with `Content-Range: bytes <start>-<end>/<total>`
//!    per chunk. A `308 Resume Incomplete` carries a `Range: bytes=0-<n>`
//!    header naming the last byte Drive holds; `200`/`201` carries the
//!    completed `files` resource JSON.

use bytes::Bytes;

use super::{DriveError, DriveFile};
use crate::remote_store::{RemoteEntry, ResumableKind, ResumableSession, ResumeProgress};

/// Drive's non-final resumable chunk-size unit in bytes (256 KiB). Every
/// non-final chunk's length must be a multiple of this (SPEC s3
/// `resume_chunk`).
pub const CHUNK_MULTIPLE: u64 = 256 * 1024;

/// The wire chunk size Driven sends per `PUT` (DESIGN s11.4.3 "the HTTP wire
/// chunk (sized to be a multiple of 256 KiB per Drive's spec, default
/// 4 MiB)"). A multiple of [`CHUNK_MULTIPLE`].
pub const CHUNK_BYTES: u64 = 4 * 1024 * 1024;

/// Opens a resumable upload session against Drive (SPEC s3
/// `resumable_session`).
///
/// `kind` selects the create (POST `/upload/drive/v3/files?uploadType=resumable`)
/// vs update (PATCH `/upload/drive/v3/files/{id}?uploadType=resumable`)
/// endpoint; the returned [`ResumableSession`] carries the session URL Drive
/// issues (the `Location` header), which the executor persists in
/// `pending_ops.payload_json` to survive restarts.
pub async fn open_session(
    http: &reqwest::Client,
    access_token: &str,
    kind: ResumableKind,
    mime: &str,
    size: u64,
) -> anyhow::Result<ResumableSession> {
    let (url, method, metadata) = match &kind {
        ResumableKind::Create {
            parent_id,
            name,
            app_properties,
        } => {
            let mut meta = serde_json::Map::new();
            meta.insert("name".to_string(), serde_json::json!(name));
            meta.insert("parents".to_string(), serde_json::json!([parent_id]));
            if !app_properties.is_empty() {
                meta.insert(
                    "appProperties".to_string(),
                    serde_json::json!(app_properties),
                );
            }
            (
                format!("{}/files", super::DRIVE_UPLOAD_BASE),
                reqwest::Method::POST,
                serde_json::Value::Object(meta),
            )
        }
        ResumableKind::Update { file_id } => (
            format!("{}/files/{}", super::DRIVE_UPLOAD_BASE, file_id),
            reqwest::Method::PATCH,
            // Drive's resumable update endpoint rejects appProperties in the
            // session metadata; the caller applies them via a follow-up
            // metadata PATCH (see GoogleDriveStore::apply_props_patch).
            serde_json::Value::Object(serde_json::Map::new()),
        ),
    };

    let metadata_body = super::json_body(&metadata)?;
    let resp = http
        .request(method, &url)
        // `fields=` selects the projection of the COMPLETION response (the
        // 200/201 files resource Drive returns when the final chunk lands).
        // Without it Drive returns its default projection, which OMITS
        // `md5Checksum` - so the executor's post-upload verify_md5 sees a
        // missing `entry.md5` and fails verification on EVERY resumable upload
        // (codex C-P1-1). Requesting FILE_FIELDS makes the completion body
        // carry md5 (a belt-and-suspenders metadata fetch in
        // resumable_upload_bytes covers the rare case Drive still omits it).
        .query(&[
            ("uploadType", "resumable"),
            ("fields", super::pagination::FILE_FIELDS),
        ])
        .bearer_auth(access_token)
        // `X-Upload-Content-Length` / `-Type` let Drive validate the declared
        // size + type up front (Drive resumable protocol).
        .header("X-Upload-Content-Type", mime)
        .header("X-Upload-Content-Length", size.to_string())
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/json; charset=UTF-8",
        )
        // Session open/commit is a control-plane request, NOT a byte transfer:
        // cap it with the dedicated resumable-control total timeout (DESIGN
        // s5.8.4) rather than inheriting the streaming profile's no-overall-cap.
        .timeout(super::RESUMABLE_CTRL_TOTAL_TIMEOUT)
        .body(metadata_body)
        .send()
        .await
        .map_err(DriveError::from_transport)?;

    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let retry_after = super::parse_retry_after(&resp);
        let body = resp.bytes().await.map_err(DriveError::from_transport)?;
        return Err(anyhow::Error::new(DriveError::from_response(
            status,
            &body,
            retry_after,
        )));
    }

    let session_url = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            anyhow::Error::new(DriveError::Classified {
                kind: crate::remote_store::DriveErrorClassification::Other,
                source: anyhow::anyhow!(
                    "drive: resumable session response missing Location header"
                ),
            })
        })?;

    Ok(ResumableSession {
        url: session_url,
        issued_at: now_unix_ms(),
        size,
        kind,
    })
}

/// Pushes one chunk to a resumable session (SPEC s3 `resume_chunk`).
///
/// Sends a `Content-Range: bytes <offset>-<end>/<total>` request. Non-final
/// chunks MUST be a multiple of 256 KiB; the final chunk (when `offset +
/// chunk.len() == session.size`) may be any size.
///
/// Return mapping:
/// - `308 Resume Incomplete` -> [`ResumeProgress::InProgress`]. Drive's `Range:
///   bytes=0-<n>` header names the last byte it acknowledges, so `received =
///   n + 1`. If the 308 carries NO `Range` header we do NOT assume the chunk
///   was accepted (assuming `offset + len` would persist a FALSE acked offset
///   and corrupt resume state after a crash - codex C-P1-3); instead we
///   re-`PUT bytes */<total>` to ask Drive for the confirmed offset and return
///   only that acknowledged count.
/// - `200`/`201` -> [`ResumeProgress::Completed`].
/// - `401` / `403` / `429` (and any non-session-dead status) -> the body is
///   read and classified via [`DriveError::from_response`] and the TYPED error
///   is returned (codex R-P1-2). This surfaces `403 storageQuotaExceeded` ->
///   `drive.quota_exhausted`, `403 dailyLimitExceeded` ->
///   `drive.daily_quota_exhausted`, `401 invalid_grant` -> `auth.invalid_grant`,
///   and `429 rateLimitExceeded` -> `drive.rate_limited`, instead of hiding
///   them behind `SessionInvalid` (which would loop the session restart forever
///   on a quota error and never let the breaker see the typed failure).
/// - `400` / `404` / `410` (a genuinely session-dead 4xx on the session URL) ->
///   [`ResumeProgress::SessionInvalid`] (SPEC s24 `drive.resumable_session_invalid`;
///   DESIGN s5.4 "any 4xx other than 308 terminates the session"). The caller
///   restarts from offset 0.
/// - 5xx / transport -> a classified [`DriveError`] so the caller's retry
///   wrapper backs off and re-queries the SAME session.
pub async fn push_chunk(
    http: &reqwest::Client,
    access_token: &str,
    session: &ResumableSession,
    offset: u64,
    chunk: Bytes,
) -> anyhow::Result<ResumeProgress> {
    let total = session.size;
    let len = chunk.len() as u64;
    let content_range = if total == 0 {
        // A zero-byte upload: Drive accepts `bytes */0`.
        "bytes */0".to_string()
    } else if len == 0 {
        // Probe / empty trailing chunk against a known total.
        format!("bytes */{total}")
    } else {
        let end = offset + len - 1;
        format!("bytes {offset}-{end}/{total}")
    };

    let resp = http
        .put(&session.url)
        .bearer_auth(access_token)
        .header(reqwest::header::CONTENT_RANGE, &content_range)
        .body(chunk)
        .send()
        .await
        .map_err(DriveError::from_transport)?;

    let status = resp.status().as_u16();
    // Drive signals "more bytes needed" with 308 Resume Incomplete.
    if status == 308 {
        match parse_range_end(&resp) {
            // Drive acknowledged bytes 0..=end: the next offset is end + 1.
            Some(end) => return Ok(ResumeProgress::InProgress { received: end + 1 }),
            // 308 with NO acknowledged Range: do NOT assume `offset + len` was
            // accepted (codex C-P1-3 - that false offset persisted across a
            // crash corrupts resume state). Ask Drive for the confirmed offset
            // and report only what it actually holds.
            None => {
                let confirmed = query_offset(http, access_token, session).await?;
                return Ok(ResumeProgress::InProgress {
                    received: confirmed,
                });
            }
        }
    }
    if (200..300).contains(&status) {
        let body = resp.bytes().await.map_err(DriveError::from_transport)?;
        let entry = parse_completed_entry(&body)?;
        return Ok(ResumeProgress::Completed(entry));
    }
    // 5xx and network are transient at the SESSION level: the chunk push can
    // be retried against the SAME session by re-querying the offset. We map
    // 5xx to a classified error so `with_retry` (when the caller wraps the
    // push) backs off; the resumable upload loop in mod.rs re-attempts.
    // codex R-P1-2: 401/403/429 (and 5xx) are NOT a dead session - read the
    // body and return the TYPED classified error so quota/auth/rate surface
    // their stable codes; only a genuinely session-dead 4xx (400/404/410)
    // collapses to SessionInvalid (the caller restarts from offset 0).
    match chunk_status_outcome(status) {
        ChunkStatusOutcome::SessionInvalid => Ok(ResumeProgress::SessionInvalid),
        ChunkStatusOutcome::Typed => {
            let retry_after = super::parse_retry_after(&resp);
            let body = resp.bytes().await.map_err(DriveError::from_transport)?;
            Err(anyhow::Error::new(DriveError::from_response(
                status,
                &body,
                retry_after,
            )))
        }
    }
}

/// How a non-2xx / non-308 chunk-PUT status is handled (codex R-P1-2). Split
/// out as a pure function so the session-dead vs typed-error decision is
/// unit-testable without standing up an HTTP server (the wire path needs a
/// real `reqwest::Response`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkStatusOutcome {
    /// The session URL is dead (400 bad-request / 404 / 410 Gone): restart
    /// from offset 0 ([`ResumeProgress::SessionInvalid`]).
    SessionInvalid,
    /// Read the body and return the typed classified error (5xx transient,
    /// 401 auth, 403 quota/rate, 429 rate, or any other status). NEVER hidden
    /// behind SessionInvalid - a quota error there would loop the restart
    /// forever and the breaker would never see the typed failure.
    Typed,
}

/// Decides how a chunk-PUT status (already known not to be 2xx or 308) is
/// handled. 400/404/410 are session-dead; everything else (401/403/429/5xx and
/// any unexpected status) is read + classified into a typed error.
fn chunk_status_outcome(status: u16) -> ChunkStatusOutcome {
    match status {
        400 | 404 | 410 => ChunkStatusOutcome::SessionInvalid,
        _ => ChunkStatusOutcome::Typed,
    }
}

/// Queries the bytes Drive has acknowledged for a session (the
/// `Content-Range: bytes */<total>` probe) so a resumed upload knows where to
/// continue (SPEC s3, DESIGN s5.4 resume). Returns the count of bytes Drive
/// holds (the next offset to send from). A `200/201` means the upload already
/// completed (returns `total`); a 4xx surfaces as a session-invalid error.
pub async fn query_offset(
    http: &reqwest::Client,
    access_token: &str,
    session: &ResumableSession,
) -> anyhow::Result<u64> {
    let resp = http
        .put(&session.url)
        .bearer_auth(access_token)
        .header(
            reqwest::header::CONTENT_RANGE,
            format!("bytes */{}", session.size),
        )
        .body(Bytes::new())
        .send()
        .await
        .map_err(DriveError::from_transport)?;
    let status = resp.status().as_u16();
    if status == 308 {
        return Ok(parse_range_end(&resp).map(|end| end + 1).unwrap_or(0));
    }
    if (200..300).contains(&status) {
        // Already complete.
        return Ok(session.size);
    }
    Err(anyhow::Error::new(DriveError::ResumableSessionInvalid))
}

/// Parses Drive's resumable completion response body into a [`RemoteEntry`]
/// (SPEC s3). Shared by [`push_chunk`]'s final-chunk path. The completion body
/// is a Drive `files` resource projected by the `fields=` query param
/// [`open_session`] set on the session URL ([`super::pagination::FILE_FIELDS`],
/// which includes `md5Checksum` so post-upload verification has the digest -
/// codex C-P1-1). We still parse leniently and tolerate missing fields in case
/// Drive ignores the projection.
pub fn parse_completed_entry(body: &[u8]) -> anyhow::Result<RemoteEntry> {
    let file: DriveFile = serde_json::from_slice(body).map_err(|e| {
        anyhow::anyhow!("drive: failed to parse resumable completion response: {e}")
    })?;
    Ok(file.into_remote_entry())
}

/// Parses the `Range: bytes=0-<end>` header Drive sends with a 308 response,
/// returning `<end>` (the last byte index Drive holds). `None` if absent.
fn parse_range_end(resp: &reqwest::Response) -> Option<u64> {
    let range = resp
        .headers()
        .get(reqwest::header::RANGE)
        .and_then(|v| v.to_str().ok())?;
    parse_range_end_str(range)
}

/// Pure parse of a `Range: bytes=0-<end>` header value into `<end>` (the last
/// byte index Drive acknowledges). `None` for an absent / malformed value so
/// the caller (codex C-P1-3) re-queries the confirmed offset rather than
/// inventing one. Split out from [`parse_range_end`] so it is unit-testable
/// without constructing a `reqwest::Response`.
fn parse_range_end_str(range: &str) -> Option<u64> {
    // Shape: "bytes=0-262143".
    let after_eq = range.split('=').nth(1).unwrap_or(range);
    let end = after_eq.split('-').nth(1)?;
    end.trim().parse::<u64>().ok()
}

/// Wall-clock Unix epoch ms (for the session's `issued_at`; Driven discards
/// sessions older than 6 days, DESIGN s5.4).
fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_completed_entry_reads_files_resource() {
        let body = br#"{
            "id": "abc123",
            "name": "big.bin",
            "parents": ["root"],
            "size": "524305",
            "md5Checksum": "0123456789abcdef0123456789abcdef",
            "mimeType": "application/octet-stream",
            "trashed": false,
            "appProperties": {"driven.source_id": "s1"}
        }"#;
        let entry = parse_completed_entry(body).unwrap();
        assert_eq!(entry.id, "abc123");
        assert_eq!(entry.name, "big.bin");
        assert_eq!(entry.size, Some(524_305));
        assert_eq!(
            entry
                .app_properties
                .get("driven.source_id")
                .map(String::as_str),
            Some("s1")
        );
    }

    #[test]
    fn parse_range_end_str_reads_acked_end() {
        // The canonical 308 Range header.
        assert_eq!(parse_range_end_str("bytes=0-262143"), Some(262_143));
        // Whitespace tolerated.
        assert_eq!(parse_range_end_str("bytes=0-100 "), Some(100));
    }

    #[test]
    fn parse_range_end_str_none_on_malformed_or_absent() {
        // C-P1-3: a missing / malformed Range must NOT yield a fabricated
        // offset - it returns None so push_chunk re-queries the confirmed
        // offset instead of persisting a false acked count.
        assert_eq!(parse_range_end_str(""), None);
        assert_eq!(parse_range_end_str("bytes=*/0"), None);
        assert_eq!(parse_range_end_str("garbage"), None);
        assert_eq!(parse_range_end_str("bytes=0-notanumber"), None);
    }

    #[test]
    fn chunk_bytes_is_a_multiple_of_chunk_multiple() {
        assert_eq!(CHUNK_BYTES % CHUNK_MULTIPLE, 0);
        const { assert!(CHUNK_BYTES >= CHUNK_MULTIPLE) };
    }

    #[test]
    fn chunk_status_outcome_session_dead_only_for_400_404_410() {
        // R-P1-2: only a genuinely session-dead 4xx restarts the session.
        for s in [400u16, 404, 410] {
            assert_eq!(
                chunk_status_outcome(s),
                ChunkStatusOutcome::SessionInvalid,
                "status {s} must be session-dead"
            );
        }
    }

    #[test]
    fn chunk_status_outcome_auth_quota_rate_are_typed_not_session_invalid() {
        // R-P1-2: 401 invalid_grant, 403 storageQuota/dailyLimit, 429 rate
        // limit must surface as TYPED errors, NOT be hidden behind
        // SessionInvalid (which would loop the session restart on a quota
        // error and never let the breaker see the failure). 5xx and any other
        // unexpected status are also read + classified.
        for s in [401u16, 403, 429, 500, 502, 503, 418] {
            assert_eq!(
                chunk_status_outcome(s),
                ChunkStatusOutcome::Typed,
                "status {s} must be read + classified (typed), not session-dead"
            );
        }
    }

    #[test]
    fn typed_statuses_classify_to_their_stable_codes() {
        use crate::remote_store::DriveErrorClassification;
        // The typed branch hands the body to DriveError::from_response; confirm
        // the auth/quota/rate bodies map to the SPEC s24 classes the executor
        // needs (R-P1-2 / R-P1-3 surface these on a streamed large upload).
        let invalid_grant = br#"{"error":"invalid_grant"}"#;
        assert!(matches!(
            DriveError::from_response(401, invalid_grant, None).classification(),
            DriveErrorClassification::AuthInvalidGrant
        ));
        let storage = br#"{"error":{"errors":[{"reason":"storageQuotaExceeded"}],"code":403}}"#;
        assert!(matches!(
            DriveError::from_response(403, storage, None).classification(),
            DriveErrorClassification::StorageQuota
        ));
        let daily = br#"{"error":{"errors":[{"reason":"dailyLimitExceeded"}],"code":403}}"#;
        assert!(matches!(
            DriveError::from_response(403, daily, None).classification(),
            DriveErrorClassification::DailyQuota
        ));
        assert!(matches!(
            DriveError::from_response(429, b"", None).classification(),
            DriveErrorClassification::RateLimited { .. }
        ));
    }
}
