//! The Drive resumable-upload session protocol helpers (SPEC s3, ROADMAP M4;
//! https://developers.google.com/drive/api/guides/manage-uploads#resumable).
//!
//! [`GoogleDriveStore`](super::GoogleDriveStore) composes these to open a
//! resumable session (create vs update), push 256-KiB-multiple non-final
//! chunks, and finalize. A 4xx mid-chunk kills the session
//! ([`ResumeProgress::SessionInvalid`]); the caller restarts from offset 0
//! (DESIGN s5.4: NEVER resume the old session URL after a 4xx).
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
        .query(&[("uploadType", "resumable")])
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
/// chunk.len() == session.size`) may be any size. A `308 Resume Incomplete`
/// returns [`ResumeProgress::InProgress`]; a `200/201` returns
/// [`ResumeProgress::Completed`]; any 4xx returns
/// [`ResumeProgress::SessionInvalid`] (SPEC s24
/// `drive.resumable_session_invalid`; DESIGN s5.4 "any 4xx other than 308
/// terminates the session").
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
        let received = parse_range_end(&resp)
            .map(|end| end + 1)
            .unwrap_or(offset + len);
        return Ok(ResumeProgress::InProgress { received });
    }
    if (200..300).contains(&status) {
        let body = resp.bytes().await.map_err(DriveError::from_transport)?;
        let entry = parse_completed_entry(&body)?;
        return Ok(ResumeProgress::Completed(entry));
    }
    // 5xx and network are transient at the SESSION level: the chunk push can
    // be retried against the SAME session by re-querying the offset. We map
    // 5xx to a classified error so `with_retry` (when the caller wraps the
    // push) backs off; the resumable upload loop in mod.rs re-attempts. But a
    // 4xx (400/401/403/404/410) is fatal to the session: SessionInvalid.
    if (500..=599).contains(&status) {
        let retry_after = super::parse_retry_after(&resp);
        let body = resp.bytes().await.map_err(DriveError::from_transport)?;
        return Err(anyhow::Error::new(DriveError::from_response(
            status,
            &body,
            retry_after,
        )));
    }
    // Any 4xx (including 410 Gone) kills the session (DESIGN s5.4).
    Ok(ResumeProgress::SessionInvalid)
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
/// is a Drive `files` resource (the [`super::pagination::FILE_FIELDS`]
/// projection is NOT applied to the resumable response, so we parse whatever
/// fields Drive returns and tolerate missing ones).
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
    fn chunk_bytes_is_a_multiple_of_chunk_multiple() {
        assert_eq!(CHUNK_BYTES % CHUNK_MULTIPLE, 0);
        const { assert!(CHUNK_BYTES >= CHUNK_MULTIPLE) };
    }
}
