//! The production Google Drive [`RemoteStore`] backend (SPEC s3, s4, s8;
//! ROADMAP M4).
//!
//! [`GoogleDriveStore`] talks to the Drive v3 REST API over a `reqwest`
//! (rustls) client, authorized by a [`RefreshingTokenSource`] that mints
//! access tokens from the keychain-stored refresh token (SPEC s4.1). It
//! honours the Drive semantics the trait requires literally: duplicate names
//! are allowed (every lookup is by file_id), `create` is always POST and
//! `update` is always PATCH, resumable non-final chunks are a multiple of
//! 256 KiB, and `app_properties` is the canonical identity for objects
//! Driven owns.
//!
//! The submodules carry the protocol helpers [`GoogleDriveStore`] composes:
//! - [`oauth`] - the PKCE loopback flow (SPEC s4).
//! - [`token_store`] - keychain-backed refresh-token storage +
//!   [`RefreshingTokenSource`] (SPEC s4.1).
//! - [`resumable`] - the resumable-upload session protocol.
//! - [`pagination`] - the `files.list` `pageToken` loop.
//! - [`retry`] - exponential-backoff + error-classification middleware.
//!
//! M4 scaffold: every type + trait method is present and typed; the bodies
//! are `todo!()` for the implement phase.

pub mod oauth;
pub mod pagination;
pub mod resumable;
pub mod retry;
pub mod token_store;

use std::collections::HashMap;

use async_trait::async_trait;
use bytes::Bytes;

use crate::remote_store::{
    AboutInfo, DownloadStream, DriveErrorClassification, RemoteEntry, RemoteStore, ResumableKind,
    ResumableSession, ResumeProgress, UploadBody,
};

use self::token_store::RefreshingTokenSource;

/// Tracing target for the Google Drive backend.
const TARGET: &str = "driven::drive::google";

/// A classified Google Drive error (SPEC s24 error taxonomy).
///
/// Carries a [`DriveErrorClassification`] (re-used from
/// [`crate::remote_store`]) so the executor / pacer / circuit-breakers can
/// decide breaker outcomes by downcasting an [`anyhow::Error`] back to this
/// type rather than string-matching the message (CODEX_NOTES: "Drive circuit
/// breaker driven by real request outcomes"). Surfaced through `anyhow` at
/// the trait boundary; recover the classification with
/// [`classification_of`].
#[derive(thiserror::Error, Debug)]
pub enum DriveError {
    /// A classified API/transport failure (429 / 5xx / network / auth /
    /// quota / other). The variant payload IS the pacer/breaker verdict.
    #[error("drive error: {kind:?}")]
    Classified {
        /// How the pacer + circuit breaker should treat this failure.
        kind: DriveErrorClassification,
        /// The underlying cause (HTTP status, transport error, parse error).
        #[source]
        source: anyhow::Error,
    },
    /// The configured destination folder was deleted from Drive (SPEC s24
    /// `drive.dest_folder_missing`).
    #[error("drive destination folder is missing")]
    DestFolderMissing,
    /// The destination folder's sharing changed to read-only for this
    /// account (SPEC s24 `drive.dest_folder_permission_denied`).
    #[error("drive destination folder is read-only for this account")]
    DestFolderPermissionDenied,
    /// A resumable upload session returned a 4xx mid-chunk; the caller must
    /// restart from offset 0 (SPEC s24 `drive.resumable_session_invalid`).
    #[error("drive resumable session is invalid; restart required")]
    ResumableSessionInvalid,
    /// Verification of the uploaded bytes failed: Drive's `md5Checksum` did
    /// not match the bytes Driven sent (SPEC s24 `drive.checksum_mismatch`).
    #[error("drive checksum mismatch after upload")]
    ChecksumMismatch,
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
            | DriveError::ChecksumMismatch => DriveErrorClassification::Other,
        }
    }
}

/// Reads the [`DriveErrorClassification`] off an [`anyhow::Error`] the trait
/// boundary surfaced, if it originated as a [`DriveError`] (the executor
/// downcasts to decide breaker outcomes; CODEX_NOTES "Drive circuit breaker
/// driven by real request outcomes"). Returns `None` for any other error.
pub fn classification_of(err: &anyhow::Error) -> Option<DriveErrorClassification> {
    err.downcast_ref::<DriveError>().map(DriveError::classification)
}

/// The production Google Drive [`RemoteStore`] (SPEC s3, ROADMAP M4).
///
/// Holds the authorized HTTP client (a `reqwest` rustls client wrapped by the
/// [`retry`] middleware), the [`RefreshingTokenSource`] (SPEC s4.1), and the
/// Drive root the store operates relative to. Cheap to clone-by-`Arc`
/// internally; the orchestrator holds it behind `Arc<dyn RemoteStore>`.
pub struct GoogleDriveStore {
    http: reqwest::Client,
    tokens: RefreshingTokenSource,
}

impl GoogleDriveStore {
    /// Builds a [`GoogleDriveStore`] from an authorized HTTP client and a
    /// [`RefreshingTokenSource`] (SPEC s4.1). The token source mints access
    /// tokens on demand from the keychain-stored refresh token; the HTTP
    /// client is the `reqwest` (rustls) client all Drive traffic flows over.
    pub fn new(http: reqwest::Client, tokens: RefreshingTokenSource) -> Self {
        let _ = TARGET;
        Self { http, tokens }
    }
}

#[async_trait]
impl RemoteStore for GoogleDriveStore {
    async fn ensure_folder(&self, parent_id: &str, name: &str) -> anyhow::Result<RemoteEntry> {
        let _ = (&self.http, &self.tokens, parent_id, name);
        todo!("M4 implement: ensure_folder (files.list by name under parent, dedup, create if none)")
    }

    async fn list_folder(&self, folder_id: &str) -> anyhow::Result<Vec<RemoteEntry>> {
        let _ = folder_id;
        todo!("M4 implement: list_folder via the files.list pageToken loop (pagination.rs)")
    }

    async fn create(
        &self,
        parent_id: &str,
        name: &str,
        mime: &str,
        body: UploadBody,
        app_properties: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        let _ = (parent_id, name, mime, body, app_properties);
        todo!("M4 implement: create (multipart POST for small bodies, resumable for streams)")
    }

    async fn update(
        &self,
        file_id: &str,
        body: UploadBody,
        app_properties_patch: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        let _ = (file_id, body, app_properties_patch);
        todo!("M4 implement: update (PATCH files metadata + upload files content by id)")
    }

    async fn resumable_session(
        &self,
        kind: ResumableKind,
        mime: &str,
        size: u64,
    ) -> anyhow::Result<ResumableSession> {
        let _ = (kind, mime, size);
        todo!("M4 implement: open a resumable upload session (resumable.rs)")
    }

    async fn resume_chunk(
        &self,
        session: &ResumableSession,
        offset: u64,
        chunk: Bytes,
    ) -> anyhow::Result<ResumeProgress> {
        let _ = (session, offset, chunk);
        todo!("M4 implement: push one resumable chunk (256 KiB non-final rule, 4xx -> SessionInvalid)")
    }

    async fn trash(&self, file_id: &str) -> anyhow::Result<()> {
        let _ = file_id;
        todo!("M4 implement: trash (PATCH trashed=true; 404 -> Ok, idempotent)")
    }

    async fn metadata(&self, file_id: &str) -> anyhow::Result<RemoteEntry> {
        let _ = file_id;
        todo!("M4 implement: GET files by id with field selection -> RemoteEntry")
    }

    async fn download(&self, file_id: &str) -> anyhow::Result<DownloadStream> {
        let _ = file_id;
        todo!("M4 implement: GET files by id with alt=media -> streaming DownloadStream")
    }

    async fn find_by_op_uuid(
        &self,
        parent_id: &str,
        op_uuid: &str,
    ) -> anyhow::Result<Option<RemoteEntry>> {
        let _ = (parent_id, op_uuid);
        todo!("M4 implement: files.list with appProperties has {{ driven.client_op_uuid }} query")
    }

    async fn about(&self) -> anyhow::Result<AboutInfo> {
        todo!("M4 implement: GET /about?fields=storageQuota -> AboutInfo")
    }
}
