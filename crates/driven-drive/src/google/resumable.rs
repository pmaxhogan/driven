//! The Drive resumable-upload session protocol helpers (SPEC s3, ROADMAP M4;
//! https://developers.google.com/drive/api/guides/manage-uploads#resumable).
//!
//! [`GoogleDriveStore`](super::GoogleDriveStore) composes these to open a
//! resumable session (create vs update), push 256-KiB-multiple non-final
//! chunks, and finalize. A 4xx mid-chunk kills the session
//! ([`ResumeProgress::SessionInvalid`]); the caller restarts from offset 0.
//!
//! M4 scaffold: signatures only; bodies are `todo!()`.

use bytes::Bytes;

use crate::remote_store::{RemoteEntry, ResumableKind, ResumableSession, ResumeProgress};

/// Opens a resumable upload session against Drive (SPEC s3
/// `resumable_session`).
///
/// `kind` selects the create (POST `/upload/files?uploadType=resumable`) vs
/// update (PATCH `/upload/files/{id}?uploadType=resumable`) endpoint; the
/// returned [`ResumableSession`] carries the session URL Drive issues, which
/// the executor persists in `pending_ops.payload_json` to survive restarts.
pub async fn open_session(
    http: &reqwest::Client,
    access_token: &str,
    kind: ResumableKind,
    mime: &str,
    size: u64,
) -> anyhow::Result<ResumableSession> {
    let _ = (http, access_token, kind, mime, size);
    todo!("M4 implement: open resumable session, return the session URL Drive issues")
}

/// Pushes one chunk to a resumable session (SPEC s3 `resume_chunk`).
///
/// Sends a `Content-Range: bytes <offset>-<end>/<total>` request. Non-final
/// chunks MUST be a multiple of 256 KiB; the final chunk (when `offset +
/// chunk.len() == session.size`) may be any size. A `308 Resume Incomplete`
/// returns [`ResumeProgress::InProgress`]; a `200/201` returns
/// [`ResumeProgress::Completed`]; any 4xx returns
/// [`ResumeProgress::SessionInvalid`] (SPEC s24
/// `drive.resumable_session_invalid`).
pub async fn push_chunk(
    http: &reqwest::Client,
    session: &ResumableSession,
    offset: u64,
    chunk: Bytes,
) -> anyhow::Result<ResumeProgress> {
    let _ = (http, session, offset, chunk);
    todo!("M4 implement: push chunk with Content-Range; 308/200/4xx -> InProgress/Completed/Invalid")
}

/// Queries the bytes Drive has acknowledged for a session (the
/// `Content-Range: bytes */<total>` probe) so a resumed upload knows where to
/// continue (SPEC s3, DESIGN s5.4 resume).
pub async fn query_offset(
    http: &reqwest::Client,
    session: &ResumableSession,
) -> anyhow::Result<u64> {
    let _ = (http, session);
    todo!("M4 implement: PUT empty body with Content-Range bytes */total, parse Range header")
}

/// Parses Drive's resumable completion response body into a [`RemoteEntry`]
/// (SPEC s3). Shared by [`push_chunk`]'s final-chunk path.
pub fn parse_completed_entry(body: &[u8]) -> anyhow::Result<RemoteEntry> {
    let _ = body;
    todo!("M4 implement: parse the files resource JSON into RemoteEntry")
}
