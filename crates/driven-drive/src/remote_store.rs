//! The [`RemoteStore`] trait that every Drive-side backend implements.
//!
//! Mirrors SPEC s3 verbatim, modulo two corrections explicitly carried
//! over from the prior audit:
//!
//! - [`UploadBody::Stream`] uses [`futures::Stream`] of `Bytes` chunks,
//!   not `AsyncRead`. The audit flagged the AsyncRead shape as a defect
//!   because the executor produces backpressured `Bytes` from the
//!   hash-tee pipeline (SPEC s8); making the body an `AsyncRead` would
//!   force a useless framing step. [`DownloadStream`] deliberately keeps
//!   `AsyncRead` (downloads come straight off the wire and the restore
//!   sink writes them with `tokio::io::copy`).
//! - The SPEC s3 listing omits `use std::collections::HashMap;` even
//!   though `app_properties` and four trait methods use it; we add the
//!   import here so the file compiles.
//!
//! Two implementations live alongside this trait: `google::GoogleDriveStore`
//! (production) and `fake::InMemoryRemoteStore` (test fake). A shared
//! contract-test suite verifies both honour the duplicate-name-allowed,
//! 256-KiB-non-final-chunk, and `find_by_op_uuid` semantics.

use std::collections::HashMap;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------------
// Value types.
// -----------------------------------------------------------------------------

/// Which Drive corpus a request is scoped to (issue #7 Shared Drives).
///
/// Every Drive object lives either in the authenticated user's My Drive or in
/// exactly one Google Shared Drive. `supportsAllDrives=true` is sent on EVERY
/// `files.*` request regardless of context (it is harmless for My Drive); the
/// context only changes the LIST/search paths, which must additionally send
/// `corpora=drive` + `driveId` + `includeItemsFromAllDrives=true` to see
/// objects inside a Shared Drive (`corpora=user`, the My-Drive default, hides
/// them).
///
/// Derived per source at config time from the folder picker (which returns the
/// `driveId` or "my-drive") and persisted alongside the destination folder id;
/// see [`DriveContext::from_stored`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveContext {
    /// The authenticated user's My Drive (the V1 default).
    MyDrive,
    /// A specific Google Shared Drive. `drive_id` is the shared drive's id,
    /// which doubles as the file id of the drive's root folder.
    SharedDrive {
        /// The Shared Drive's `driveId`.
        drive_id: String,
    },
}

impl DriveContext {
    /// The Shared Drive id when this context targets a Shared Drive, else
    /// `None` (My Drive).
    pub fn drive_id(&self) -> Option<&str> {
        match self {
            DriveContext::MyDrive => None,
            DriveContext::SharedDrive { drive_id } => Some(drive_id.as_str()),
        }
    }

    /// Whether this context targets a Shared Drive (vs My Drive).
    pub fn is_shared_drive(&self) -> bool {
        matches!(self, DriveContext::SharedDrive { .. })
    }

    /// Builds a context from a persisted optional drive id, as stored beside a
    /// source's destination folder. `None`, an empty string, or the sentinel
    /// `"my-drive"` all decode to [`DriveContext::MyDrive`]; any other value is
    /// a Shared Drive id.
    pub fn from_stored(drive_id: Option<&str>) -> Self {
        match drive_id.map(str::trim) {
            None | Some("") | Some("my-drive") => DriveContext::MyDrive,
            Some(id) => DriveContext::SharedDrive {
                drive_id: id.to_string(),
            },
        }
    }
}

/// One Google Shared Drive root, surfaced by
/// [`RemoteStore::list_shared_drives`] for the destination picker (issue #7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedDrive {
    /// The Shared Drive's `driveId` (also the id of its root folder).
    pub id: String,
    /// Human-readable Shared Drive name for the picker UI.
    pub name: String,
}

/// A Drive object (file or folder) as returned by list / metadata calls
/// (SPEC s3).
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    /// Drive `file_id`. Stable across renames and moves.
    pub id: String,
    /// Display name; Drive allows duplicate names within a folder, so the
    /// id is the only safe identity (SPEC s3 preamble).
    pub name: String,
    /// Parent folder ids; usually exactly one for Drive-on-My-Drive.
    pub parents: Vec<String>,
    /// Size in bytes (`None` for folders).
    pub size: Option<u64>,
    /// MD5 of the bytes actually stored on Drive; for encrypted sources
    /// this is the ciphertext MD5 (SPEC s3).
    pub md5: Option<[u8; 16]>,
    /// MIME type.
    pub mime_type: String,
    /// Last-modified time as Unix epoch ms.
    pub modified_time: i64,
    /// Whether the object is in Drive's trash.
    pub trashed: bool,
    /// Driven's canonical identity attached to every object we own:
    /// `driven.source_id`, `driven.relative_path_hash`,
    /// `driven.client_op_uuid` (SPEC s3 preamble).
    pub app_properties: HashMap<String, String>,
}

/// A resumable upload session issued by [`RemoteStore::resumable_session`]
/// (SPEC s3).
///
/// Derives `Serialize`/`Deserialize` so the executor can persist the live
/// session in `pending_ops.payload_json` and resume it byte-for-byte after
/// a process restart (DESIGN s5.4: "Resumable session URLs survive process
/// restarts: persisted in `pending_ops.payload_json`").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumableSession {
    /// The session URL Drive issues. Caller posts chunks to this URL.
    pub url: String,
    /// Wall-time the session was issued (Unix epoch ms). Driven discards
    /// sessions older than 6 days (Drive expires at 7).
    pub issued_at: i64,
    /// Total content length the session was opened for.
    pub size: u64,
    /// Whether the session is a create-new or update-existing.
    pub kind: ResumableKind,
}

/// Discriminates between a resumable create and a resumable update
/// (SPEC s3). Drive uses different endpoints for the two; POST always
/// creates a new object and would produce a duplicate if used to
/// "overwrite".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResumableKind {
    /// Create a new file under `parent_id` with `name`. The
    /// `app_properties` are attached at session-open time so they land
    /// atomically with the file (essential for the crash-safe
    /// `client_op_uuid` protocol in DESIGN s5.6).
    Create {
        /// Folder id of the destination.
        parent_id: String,
        /// Display name for the new file.
        name: String,
        /// `appProperties` to attach to the new file.
        app_properties: HashMap<String, String>,
    },
    /// Update an existing file identified by `file_id`.
    Update {
        /// Drive `file_id` of the existing file.
        file_id: String,
    },
}

/// Progress signal returned by [`RemoteStore::resume_chunk`] (SPEC s3).
#[derive(Debug)]
pub enum ResumeProgress {
    /// Upload still in progress; Drive has accepted bytes up to
    /// `received` (exclusive).
    InProgress {
        /// Total bytes Drive has accepted across all chunks so far.
        received: u64,
    },
    /// Final chunk acknowledged; upload complete. The included entry is
    /// the resulting Drive object.
    Completed(RemoteEntry),
    /// Drive returned a 4xx on the chunk; the session is dead. The
    /// caller MUST discard this session and restart from offset 0
    /// (SPEC s3 `resume_chunk` doc + s24
    /// `drive.resumable_session_invalid`).
    SessionInvalid,
}

/// A handle to a streaming download body (SPEC s3).
///
/// Reads come straight off the wire; the restore sink usually pipes
/// this into `tokio::io::copy` and a hashing tee.
pub struct DownloadStream(pub Box<dyn tokio::io::AsyncRead + Send + Unpin>);

/// Upload body for [`RemoteStore::create`] and [`RemoteStore::update`]
/// (SPEC s3).
pub enum UploadBody {
    /// In-memory body for small files (below the resumable threshold;
    /// default 4 MiB per DESIGN s11.4.3).
    Bytes(Bytes),
    /// Streaming body for the 3-stage executor pipeline (DESIGN
    /// s11.4.3). `len` is the total content length (required for the
    /// resumable upload's `Content-Length` header). `stream` yields
    /// `Bytes` chunks; each chunk handed to Drive must be a multiple of
    /// 256 KiB except the final one, and the executor accumulates
    /// pipeline-chunks to satisfy that.
    Stream {
        /// Total content length in bytes.
        len: u64,
        /// Stream of body chunks.
        stream: Box<dyn futures::Stream<Item = anyhow::Result<Bytes>> + Send + Unpin>,
    },
}

/// Lightweight quota / account info (SPEC s3 `about`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AboutInfo {
    /// Total storage quota in bytes; `None` for unlimited (Workspace).
    pub limit: Option<u64>,
    /// Total bytes consumed across Drive, Gmail, and Photos.
    pub usage: u64,
    /// Bytes consumed by Drive content (non-trash).
    pub usage_in_drive: u64,
    /// Bytes consumed by Drive's trash.
    pub usage_in_drive_trash: u64,
}

/// Classification of a Drive error, used by the pacer (SPEC s9) and the
/// circuit-breakers (DESIGN s5.8.3). Declared here so the M1 phase 2
/// implementations can populate it without a forward dep on M3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveErrorClassification {
    /// 429 / `userRateLimitExceeded`. The pacer halves its ceiling and
    /// sleeps for the recommended interval.
    RateLimited {
        /// Recommended retry-after delay in milliseconds.
        retry_after_ms: u64,
    },
    /// 5xx response from Drive; transient and worth retrying with
    /// exponential backoff.
    Transient5xx,
    /// Lower-level network failure (DNS, connect, TLS, timeout).
    Network,
    /// 401 `invalid_grant` on token refresh. The account moves to
    /// `needs_reauth`.
    AuthInvalidGrant,
    /// 403 `dailyLimitExceeded`. Driven pauses until the daily Drive
    /// quota window resets.
    DailyQuota,
    /// 403 `storageQuotaExceeded`. The user's Drive is full; Driven
    /// pauses and surfaces the error.
    StorageQuota,
    /// Anything else; treated as fatal-for-this-op.
    Other,
}

// -----------------------------------------------------------------------------
// The trait.
// -----------------------------------------------------------------------------

/// Drive-side storage contract (SPEC s3).
///
/// Implementations must honour these Drive semantics literally:
/// - Drive allows duplicate names within the same folder; every lookup
///   is by `file_id`, never by name alone.
/// - Updating an existing object is its own API path (`update`); using
///   `create` to "overwrite" yields a duplicate.
/// - Non-final chunks in a resumable upload must be a multiple of
///   256 KiB. Final chunks may be any size.
/// - `app_properties` is the canonical identity for objects Driven owns.
#[async_trait]
pub trait RemoteStore: Send + Sync {
    /// Ensures a child folder with the given name exists under
    /// `parent_id` *uniquely*. Searches by name; if multiple matches
    /// exist, picks the one with our
    /// `app_properties["driven.folder_marker"]` if present, else the
    /// oldest non-trashed one and logs a warning. Creates if none.
    ///
    /// `drive_context` scopes the search: [`DriveContext::MyDrive`] confines it
    /// to My Drive; [`DriveContext::SharedDrive`] scopes both the search and the
    /// folder create to that Shared Drive (issue #7).
    async fn ensure_folder(
        &self,
        parent_id: &str,
        name: &str,
        drive_context: &DriveContext,
    ) -> anyhow::Result<RemoteEntry>;

    /// Lists every direct child of `folder_id`. `drive_context` scopes the
    /// listing: a [`DriveContext::SharedDrive`] lists children inside that
    /// Shared Drive (issue #7).
    async fn list_folder(
        &self,
        folder_id: &str,
        drive_context: &DriveContext,
    ) -> anyhow::Result<Vec<RemoteEntry>>;

    /// Enumerates the Shared Drives the authenticated account can access
    /// (Drive `drives.list`), for the destination picker to show Shared Drive
    /// roots beside My Drive (issue #7).
    ///
    /// The default returns an empty list, so a backend with no Shared Drive
    /// notion (or that opts out) simply offers My Drive only. The production
    /// `GoogleDriveStore` overrides it with a real `drives.list` call.
    async fn list_shared_drives(&self) -> anyhow::Result<Vec<SharedDrive>> {
        Ok(Vec::new())
    }

    /// Creates a new file under `parent_id`. Always POST. The caller is
    /// responsible for ensuring no `file_state.drive_file_id` exists for
    /// this `(source_id, relative_path)` already; otherwise this call
    /// creates a duplicate (Drive's documented behaviour).
    async fn create(
        &self,
        parent_id: &str,
        name: &str,
        mime: &str,
        body: UploadBody,
        app_properties: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry>;

    /// Updates an existing file by `file_id` (PATCH). Use for any change
    /// to a file Driven has already uploaded. The caller MUST carry the
    /// `file_id` from `file_state.drive_file_id` rather than re-resolve
    /// by name (Drive permits duplicate names).
    async fn update(
        &self,
        file_id: &str,
        body: UploadBody,
        app_properties_patch: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry>;

    /// Opens a resumable upload session. `kind` chooses create vs update.
    async fn resumable_session(
        &self,
        kind: ResumableKind,
        mime: &str,
        size: u64,
    ) -> anyhow::Result<ResumableSession>;

    /// Pushes one chunk to a resumable session.
    ///
    /// Non-final chunks MUST be a multiple of 256 KiB. The final chunk
    /// (when `offset + chunk.len() == session.size`) may be any size,
    /// including a non-multiple. On a 4xx response the session is dead;
    /// this method returns [`ResumeProgress::SessionInvalid`] and the
    /// caller must discard the session and re-create from offset 0.
    async fn resume_chunk(
        &self,
        session: &ResumableSession,
        offset: u64,
        chunk: Bytes,
    ) -> anyhow::Result<ResumeProgress>;

    /// Moves an object to trash. Idempotent: trashing an already-trashed
    /// file succeeds, and a 404 from Drive is treated as success
    /// (already-gone is the desired state).
    async fn trash(&self, file_id: &str) -> anyhow::Result<()>;

    /// PERMANENTLY deletes an object (issue #36 version-store count-cap prune).
    ///
    /// Unlike [`trash`](Self::trash) this bypasses the trash and frees the
    /// storage immediately. Idempotent: a 404 (already gone) is treated as
    /// success. The caller MUST only ever pass an id it has proven is no longer
    /// a live pointer (a superseded version whose `file_state` row has already
    /// been flipped to a different object) - the store does not re-check.
    ///
    /// The default SAFELY degrades to [`trash`](Self::trash) (a soft-delete
    /// instead of a hard-delete) so a store that does not implement a permanent
    /// delete never over-deletes; the production `GoogleDriveStore` and the test
    /// `InMemoryRemoteStore` override it with a real hard-delete, and the
    /// `BreakerReportingStore` wrapper delegates to its inner store.
    async fn delete_permanent(&self, file_id: &str) -> anyhow::Result<()> {
        self.trash(file_id).await
    }

    /// Returns metadata for one Drive object by id.
    async fn metadata(&self, file_id: &str) -> anyhow::Result<RemoteEntry>;

    /// Opens a download stream for one Drive object by id.
    async fn download(&self, file_id: &str) -> anyhow::Result<DownloadStream>;

    /// Finds an object Driven previously created under `parent_id` whose
    /// `app_properties["driven.client_op_uuid"]` matches `op_uuid`.
    ///
    /// Used by the reconciliation pass (DESIGN s5.6) after a crash: when
    /// the create-intent was recorded in `pending_ops` but the resulting
    /// `file_id` never made it into `file_state`, this lookup finds the
    /// orphaned remote object so Driven can adopt it instead of creating
    /// a duplicate. If multiple matches exist (an astronomical-but-not-
    /// impossible duplicate after a bug), implementations return the
    /// most-recent and log a warning.
    async fn find_by_op_uuid(
        &self,
        parent_id: &str,
        op_uuid: &str,
        drive_context: &DriveContext,
    ) -> anyhow::Result<Option<RemoteEntry>>;

    /// Returns quota / about info for the authenticated account. Cheap
    /// enough to call for the "x of y used" UI display.
    async fn about(&self) -> anyhow::Result<AboutInfo>;
}
