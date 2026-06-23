//! `InMemoryRemoteStore` - an in-memory [`RemoteStore`] implementation
//! exercised by the contract tests (`tests/fake_contract.rs`) and by
//! every sync-engine test in the workspace (M2+).
//!
//! ## Drive quirks this fake reproduces (faithfully)
//!
//! - Duplicate names within a folder are allowed; identity is the
//!   `file_id` (UUID), not the name. Two `create()` calls with the same
//!   `(parent_id, name)` yield two distinct `file_id`s
//!   (SPEC s3 preamble).
//! - `create()` always POSTs a brand-new object; `update()` patches in
//!   place by `file_id` and preserves `app_properties` unless explicitly
//!   overwritten by the patch map (SPEC s3 `create` / `update`).
//! - `trash()` flips a per-file flag; `list_folder()` omits trashed
//!   children by default; trashing the same id twice is idempotent and
//!   trashing an unknown id is treated as success (SPEC s3 `trash` +
//!   SPEC s24 `drive.unreachable` 404-is-ok rule).
//! - Resumable sessions enforce the 256 KiB chunk-multiple rule on
//!   non-final chunks: the final chunk (offset + len == session.size)
//!   may be any size; any other chunk that is not a multiple of 256 KiB
//!   yields [`ResumeProgress::SessionInvalid`] (SPEC s3 `resume_chunk`
//!   doc). Sessions invalidated by a 4xx (modelled by the fault-injection
//!   builders in [`fault_injection`]) stay dead - every subsequent
//!   `resume_chunk` on that session returns `SessionInvalid` until the
//!   caller restarts.
//! - `app_properties` is the canonical identity for objects Driven owns;
//!   `find_by_op_uuid()` walks the parent's non-trashed children and
//!   matches `app_properties["driven.client_op_uuid"]` (SPEC s3 +
//!   DESIGN s5.6 reconciliation pass). Duplicates return the most-recent
//!   by monotonic insertion sequence (deterministic for tests) with a
//!   `tracing::warn!`.
//!
//! ## Drive quirks this fake intentionally skips ("narrow scope")
//!
//! - **Team / Shared Drives**: no `driveId`, no shared-with-me semantics.
//!   The trait does not surface them and the production
//!   `GoogleDriveStore` will (M4) confine itself to My Drive in V1.
//! - **Advanced sharing / permissions**: every object is implicitly
//!   owned by the authenticated user. No ACL evaluation, no
//!   `permissions.list`.
//! - **Pagination**: `list_folder()` returns the full child set with no
//!   cursor. The real Drive paginates via `nextPageToken`; that is a
//!   `GoogleDriveStore`-internal concern below the trait (the trait
//!   already collapses pagination behind a single `Vec`).
//! - **Google-Docs export** (`exportLinks`, native Doc/Sheet types).
//!   Driven only stores binary blobs, so this never comes up.
//! - **Real MIME sniffing**: the fake stores whatever `mime` string the
//!   caller passed and round-trips it verbatim.
//! - **Quota enforcement** is opt-in via
//!   [`InMemoryRemoteStore::with_quota_exhausted_after`] - the default
//!   behaviour is unlimited storage, which mirrors how unit tests want
//!   the fake to behave.
//!
//! The fault-injection extensions in [`fault_injection`] add the
//! per-error-class triggers the chaos harness (STRESS_HARNESS s5) and
//! the M3 executor tests rely on.

pub mod fault_injection;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, ReadBuf};
use tracing::warn;
use uuid::Uuid;

use crate::remote_store::{
    AboutInfo, DownloadStream, RemoteEntry, RemoteStore, ResumableKind, ResumableSession,
    ResumeProgress, UploadBody,
};

/// Tracing target for the fake.
const TARGET: &str = "driven::drive::fake";

/// Drive's non-final resumable chunk size in bytes (256 KiB). The trait
/// docs (SPEC s3 `resume_chunk`) require every non-final chunk to be a
/// multiple of this; the fake enforces the rule literally.
pub const CHUNK_MULTIPLE: u64 = 256 * 1024;

/// `app_properties` key Drive uses to mark folders Driven created. Used
/// by [`InMemoryRemoteStore::ensure_folder`] to disambiguate when the
/// user has manually created another folder with the same name in the
/// Drive web UI.
pub const FOLDER_MARKER_KEY: &str = "driven.folder_marker";

/// `app_properties` key Drive uses to carry the create-op UUID for the
/// crash-safe reconciliation protocol (DESIGN s5.6).
pub const CLIENT_OP_UUID_KEY: &str = "driven.client_op_uuid";

// ---------------------------------------------------------------------------
// Internal value types.
// ---------------------------------------------------------------------------

/// One file or folder in the fake's in-memory tree.
#[derive(Debug, Clone)]
struct FileEntry {
    /// Drive file_id (UUID v4). The real identity.
    file_id: String,
    /// Display name. Drive permits duplicate names within a folder.
    name: String,
    /// Parent folder id. The fake stores a single parent per object,
    /// matching how Driven uses Drive (no multi-parenting).
    parent_id: String,
    /// MIME type. `application/vnd.google-apps.folder` for folders.
    mime_type: String,
    /// File content. Empty `Vec` for folders.
    bytes: Vec<u8>,
    /// `app_properties` map (Driven's canonical identity carrier).
    app_properties: HashMap<String, String>,
    /// Last-modified time (Unix epoch ms).
    modified_time_ms: i64,
    /// Trash flag (SPEC s3 `trash`).
    trashed: bool,
    /// Monotonic insertion sequence. Used as a deterministic tiebreaker
    /// in [`find_by_op_uuid`] (higher seq = more recent), keeping the
    /// fake independent of wall-clock so concurrent tests are
    /// reproducible.
    seq: u64,
    /// Latched-bad md5. Set by [`maybe_md5_mismatch`] when the
    /// `with_md5_mismatch_after` fault trips on a write; cleared on any
    /// subsequent successful write that rewrites this entry's bytes.
    /// Every read path ([`Self::to_remote_entry`]) prefers this value
    /// over the freshly-computed md5 so the executor's checksum-mismatch
    /// retry path (SPEC s8) fires consistently across read calls.
    corrupted_md5: Option<[u8; 16]>,
    /// Content oracle (P1-B): when `Some`, this entry was written under the
    /// streaming oracle so `bytes` is EMPTY (the literal content was never
    /// retained) and the true content length + md5 live here instead. Lets a
    /// 10-50 GB upload be verified by length + digest without buffering the
    /// bytes. `None` for every normally-stored object.
    oracle: Option<OracleDigest>,
}

/// Length + md5 digest of an object's content, recorded by the streaming
/// content oracle (P1-B) instead of the literal bytes.
#[derive(Debug, Clone, Copy)]
struct OracleDigest {
    /// Exact content length in bytes.
    len: u64,
    /// md5 of the full content (computed incrementally as it streamed).
    md5: [u8; 16],
}

impl FileEntry {
    fn is_folder(&self) -> bool {
        self.mime_type == "application/vnd.google-apps.folder"
    }

    fn md5(&self) -> Option<[u8; 16]> {
        if self.is_folder() {
            return None;
        }
        // Oracle-stored entries carry the true md5 computed while the content
        // streamed; the literal bytes are absent so recompute is impossible.
        if let Some(d) = &self.oracle {
            return Some(d.md5);
        }
        use md5::{Digest, Md5};
        let mut hasher = Md5::new();
        hasher.update(&self.bytes);
        let out = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&out);
        Some(bytes)
    }

    /// The content length in bytes (oracle length when oracle-stored, else the
    /// retained byte count). `None` for folders.
    fn content_len(&self) -> Option<u64> {
        if self.is_folder() {
            return None;
        }
        Some(
            self.oracle
                .map(|d| d.len)
                .unwrap_or(self.bytes.len() as u64),
        )
    }

    /// Build a [`RemoteEntry`] from this entry. The md5 returned is
    /// `corrupted_md5` if the md5-mismatch fault has latched on this
    /// entry, otherwise the freshly-computed md5 of the stored bytes (or the
    /// oracle md5 when the bytes were not retained).
    fn to_remote_entry(&self) -> RemoteEntry {
        let size = if self.is_folder() {
            None
        } else {
            self.content_len()
        };
        RemoteEntry {
            id: self.file_id.clone(),
            name: self.name.clone(),
            parents: vec![self.parent_id.clone()],
            size,
            md5: self.corrupted_md5.or_else(|| self.md5()),
            mime_type: self.mime_type.clone(),
            modified_time: self.modified_time_ms,
            trashed: self.trashed,
            app_properties: self.app_properties.clone(),
        }
    }
}

/// Bytes a resumable session has accepted so far.
///
/// Normal mode keeps the literal bytes (committed verbatim on the final
/// chunk). Oracle mode (P1-B) keeps only a running length + md5 hasher so a
/// 10-50 GB resumable upload never buffers the whole content - the huge-file
/// rows verify by length + digest instead of byte round-trip.
enum Received {
    /// Literal accepted bytes, grown one chunk at a time.
    Bytes(Vec<u8>),
    /// Streaming digest: accepted length + an incremental md5 hasher.
    Digest { len: u64, hasher: md5::Md5 },
}

impl std::fmt::Debug for Received {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Received::Bytes(b) => f.debug_struct("Bytes").field("len", &b.len()).finish(),
            Received::Digest { len, .. } => f.debug_struct("Digest").field("len", len).finish(),
        }
    }
}

impl Received {
    /// Bytes accepted so far (the resumable offset bookkeeping).
    fn len(&self) -> u64 {
        match self {
            Received::Bytes(b) => b.len() as u64,
            Received::Digest { len, .. } => *len,
        }
    }

    /// Accept one validated chunk: append bytes (normal) or fold them into the
    /// length + md5 digest (oracle), never buffering in oracle mode.
    fn accept(&mut self, chunk: &[u8]) {
        use md5::Digest;
        match self {
            Received::Bytes(b) => b.extend_from_slice(chunk),
            Received::Digest { len, hasher } => {
                hasher.update(chunk);
                *len += chunk.len() as u64;
            }
        }
    }

    /// Finalize the committed content into `(bytes, oracle, len)` for storing
    /// in a [`FileEntry`]: literal bytes yield `(bytes, None, bytes.len())`; an
    /// oracle digest yields `(empty, Some(digest), len)` so the huge-file rows
    /// commit by length+md5 without retaining the content.
    fn finish(self) -> (Vec<u8>, Option<OracleDigest>, u64) {
        use md5::Digest;
        match self {
            Received::Bytes(b) => {
                let len = b.len() as u64;
                (b, None, len)
            }
            Received::Digest { len, hasher } => {
                let mut md5 = [0u8; 16];
                md5.copy_from_slice(&hasher.finalize());
                (Vec::new(), Some(OracleDigest { len, md5 }), len)
            }
        }
    }
}

/// State of an open resumable upload session.
///
/// The fake stores received content; on the final chunk it commits it as the
/// underlying file's content (create) or replaces the existing file's content
/// (update). Mid-session 4xx (the fault injectors flip `invalid = true`)
/// leaves the session marked dead - every subsequent `resume_chunk` returns
/// [`ResumeProgress::SessionInvalid`] regardless of fault counters, matching
/// SPEC s3's "session is dead" rule.
#[derive(Debug)]
struct ResumableSessionState {
    /// Total content length the session was opened for.
    size: u64,
    /// Content received so far (literal bytes, or a streaming oracle digest).
    received: Received,
    /// MIME type the session was opened for.
    mime: String,
    /// Create-or-update target metadata.
    kind: ResumableKind,
    /// Wall-time the session was issued (Unix epoch ms). Surfaces in
    /// the corresponding [`ResumableSession`] for the 6-day-window
    /// check the production code is expected to apply. Retained on the
    /// fake's session state (not just on the public handle) so a future
    /// session-expiry fault injector can read it without an API break.
    #[allow(dead_code)]
    issued_at: i64,
    /// Whether the session has been invalidated by a 4xx (real or
    /// fault-injected) or a non-256-KiB-multiple non-final chunk.
    invalid: bool,
    /// Per-session chunk countdown: when > 0 each accepted chunk
    /// decrements; when the decrement hits zero the session
    /// invalidates. `u64::MAX` = "never trip", so a session opened
    /// without an arming call is unaffected by another session's
    /// budget (the C.2 fix the task brief calls out).
    session_invalidated_after_chunks: u64,
}

impl ResumableSessionState {
    /// Mark the session dead AND release its received-bytes buffer.
    ///
    /// An invalidated session can never commit, so retaining the partial
    /// upload only leaks memory (a 1 GiB transfer that 4xx'd mid-stream
    /// would otherwise pin ~1 GiB until the session is dropped). We keep
    /// only the lightweight `invalid` tombstone so future `resume_chunk`
    /// calls still return [`ResumeProgress::SessionInvalid`].
    fn invalidate(&mut self) {
        self.invalid = true;
        self.received = Received::Bytes(Vec::new());
    }
}

/// The mutable core of the fake.
#[derive(Debug)]
struct Inner {
    /// All objects (files and folders), keyed by `file_id`. Flat-by-id
    /// because Drive's identity is the id - the parent index is just a
    /// derived filter.
    objects: HashMap<String, FileEntry>,
    /// Open resumable sessions keyed by their session URL.
    sessions: HashMap<String, ResumableSessionState>,
    /// Cumulative count of resumable sessions EVER opened (monotonic; never
    /// decremented when a session completes/invalidates). Lets a test assert
    /// that an upload did NOT take the resumable path at all - e.g. a VSS
    /// snapshot read, which is forced non-resumable (P1-B). Distinct from
    /// `sessions.len()` (currently-open) which is 0 once an upload finishes.
    resumable_sessions_opened: u64,
    /// Monotonic insertion counter (assigned to every new
    /// [`FileEntry`]).
    seq: u64,
    /// Wall-time the fake last advanced to (Unix epoch ms). Real
    /// production code derives this from a `Clock`; in the fake we
    /// simply tick by 1ms per state mutation so two consecutive
    /// creates have ordered `modified_time` values without needing a
    /// `Clock` injection.
    now_ms: i64,
    /// Total bytes currently held in object content (folders excluded).
    /// Tracked here so [`fault_injection::with_quota_exhausted_after`]
    /// can trip cheaply.
    bytes_stored: u64,
    /// Quota information surfaced by `about()`. The default is a
    /// generous `Some(1 << 50)` limit (1 PiB) to keep tests realistic
    /// without becoming a quota-enforcement engine.
    about_limit: Option<u64>,
    /// FIFO pool of `file_id`s freed by `trash()` while the
    /// `fileid_recycle` fault is latched. The next `create` pops from the
    /// front instead of minting a fresh UUID, reproducing genuine Drive
    /// file_id recycling (STRESS_HARNESS s3.7 `drive-fileid-recycled`).
    /// Empty (and unused) unless the fault is armed.
    recycle_pool: std::collections::VecDeque<String>,
}

impl Inner {
    fn next_seq(&mut self) -> u64 {
        self.seq = self.seq.saturating_add(1);
        self.seq
    }

    fn tick(&mut self) -> i64 {
        self.now_ms = self.now_ms.saturating_add(1);
        self.now_ms
    }

    /// Allocate a `file_id` for a new FILE object. Pops a recycled id from
    /// [`Inner::recycle_pool`] (FIFO) when the `fileid_recycle` fault has
    /// queued one; otherwise mints a fresh UUID. Folders never recycle -
    /// `ensure_folder` always mints fresh so the marker-folder identity
    /// stays stable.
    fn alloc_file_id(&mut self) -> String {
        self.recycle_pool
            .pop_front()
            .unwrap_or_else(|| Uuid::new_v4().to_string())
    }

    fn children(&self, parent_id: &str) -> Vec<&FileEntry> {
        self.objects
            .values()
            .filter(|e| e.parent_id == parent_id)
            .collect()
    }

    fn ensure_object(&self, file_id: &str) -> anyhow::Result<&FileEntry> {
        self.objects
            .get(file_id)
            .ok_or_else(|| anyhow::anyhow!("fake: no object with file_id {file_id}"))
    }

    /// Validate that `parent_id` names an existing, LIVE FOLDER. Real
    /// Drive rejects a create / ensure_folder whose parent is a file, does
    /// not exist, OR is trashed (a trashed folder cannot receive new
    /// children - the write fails as "parent not found"). The fake mirrors
    /// that so tests cannot construct an impossible Drive state and so the
    /// production dest-folder-deleted path (STRESS_HARNESS + M4) is not
    /// masked by the fake silently accepting a trashed parent. Returns an
    /// `Err` when the parent is missing, is a non-folder object, OR is a
    /// trashed folder (the trashed case is reported the same as missing).
    fn ensure_folder_parent(&self, parent_id: &str) -> anyhow::Result<()> {
        match self.objects.get(parent_id) {
            None => anyhow::bail!("fake: parent folder {parent_id} does not exist"),
            Some(e) if !e.is_folder() => {
                anyhow::bail!("fake: parent {parent_id} is not a folder")
            }
            // A trashed folder is treated as missing: real Drive will not
            // create a child under a trashed parent.
            Some(e) if e.trashed => {
                anyhow::bail!("fake: parent folder {parent_id} is trashed (treated as missing)")
            }
            Some(_) => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// The fake itself.
// ---------------------------------------------------------------------------

/// In-memory [`RemoteStore`] implementation (M1 phase 2B).
///
/// Construct with [`InMemoryRemoteStore::new`]. The fake starts empty
/// except for a synthetic "root" folder whose id is exposed by
/// [`InMemoryRemoteStore::root_id`] - call sites use that id as the
/// `parent_id` argument to [`RemoteStore::ensure_folder`] /
/// [`RemoteStore::create`] when emulating "uploads into the My Drive
/// root".
///
/// Cloning the fake clones the [`Arc`] - both handles see the same
/// underlying state, matching how `GoogleDriveStore` will hand the same
/// `RemoteStore` to multiple uploader workers.
#[derive(Debug, Clone)]
pub struct InMemoryRemoteStore {
    inner: Arc<Mutex<Inner>>,
    root_id: String,
    /// Counters and flags driving the fault-injection extensions
    /// (STRESS_HARNESS s5). The wrapping `Arc` lets the builder methods
    /// keep the configuration mutable while the cloned `RemoteStore`
    /// trait object sees the same state.
    pub(crate) faults: Arc<Faults>,
}

/// Fault-injection counters / latches.
///
/// Each counter starts at `u64::MAX` (= "never trip") and is set by the
/// `with_*_after` builders. The per-request hook in each trait method
/// decrements its counter; on transition from > 0 to 0 the next
/// request returns the matching error class.
///
/// Atomic so the fake remains lock-free on the fault path - the only
/// reason to grab the inner mutex is to actually mutate object state.
#[derive(Debug)]
pub(crate) struct Faults {
    pub(crate) rate_limit_after: AtomicU64,
    pub(crate) five_xx_after: AtomicU64,
    pub(crate) invalid_grant_after: AtomicU64,
    pub(crate) network_drop_after: AtomicU64,
    pub(crate) session_invalidated_after_chunks: AtomicU64,
    pub(crate) md5_mismatch_after: AtomicU64,
    pub(crate) quota_exhausted_after_bytes: AtomicU64,
    /// Trip a `403 dailyLimitExceeded` after this many WriteTarget requests
    /// (STRESS_HARNESS s3.7 `daily-quota-exhausted`, P1-F). Distinct from the
    /// per-10-minute `rate_limit_after` and the total-bytes
    /// `quota_exhausted_after_bytes`: a daily-limit trip pauses the account
    /// until midnight Pacific. LATCHES once tripped (the daily window stays
    /// closed for the rest of the run), so every subsequent write keeps
    /// returning the daily-limit error - modelling a quota that does not reset
    /// within a single harness run. `u64::MAX` (the default) never trips. Set
    /// by [`fault_injection::with_daily_quota_after`].
    pub(crate) daily_quota_after: AtomicU64,
    /// Latched true once `daily_quota_after` trips, so the daily window stays
    /// closed for every later write in the run.
    pub(crate) daily_quota_latched: std::sync::atomic::AtomicBool,
    /// Artificial per-request latency in nanoseconds, injected by
    /// [`fault_injection::with_slow_responses`]. `0` (the default) means
    /// no delay. Every trait method awaits this before doing its work
    /// (DESIGN s5.8.1 "lossy: +500ms latency"); the sleep happens in
    /// `maybe_delay` BEFORE the inner mutex is acquired so no guard is
    /// held across the `.await`.
    pub(crate) response_delay_nanos: AtomicU64,
    /// Latched once tripped (auth.invalid_grant is "stay-broken").
    pub(crate) invalid_grant_latched: std::sync::atomic::AtomicBool,
    /// Dest-folder states are latched on construction by the builder.
    pub(crate) dest_folder_missing: std::sync::atomic::AtomicBool,
    pub(crate) dest_folder_readonly: std::sync::atomic::AtomicBool,
    /// When true, `find_by_op_uuid()` surfaces trashed children
    /// alongside live ones, modelling the "trashed remote object whose
    /// `file_state` row has not yet been reconciled" case (DESIGN s5.6).
    /// Not actual Drive file_id recycling - that would need a separate
    /// id-pool flag and is out of M1 scope.
    pub(crate) trashed_visible_in_find: std::sync::atomic::AtomicBool,
    /// When true, `trash()` pushes the trashed object's `file_id` into the
    /// inner recycle pool and the next `create` (direct or resumable-commit)
    /// pops it instead of minting a fresh UUID, modelling genuine Drive
    /// file_id recycling (STRESS_HARNESS s3.7 `drive-fileid-recycled`). Set
    /// by [`fault_injection::with_fileid_recycle`].
    pub(crate) fileid_recycle: std::sync::atomic::AtomicBool,
    /// When true, write paths (`create`, `update`, resumable `resume_chunk`
    /// commit) do NOT retain the literal uploaded bytes - they record only the
    /// content LENGTH and a streaming md5 digest of the bytes as they pass
    /// through. This is the streaming/sparse content oracle (STRESS_HARNESS
    /// s3.2 `huge-file-10gb` / `huge-file-50gb-mid-run-crash`, P1-B): a 10-50 GB
    /// upload can be verified by length + md5 without ever buffering tens of
    /// gigabytes in a `Vec<u8>` and OOMing. An oracle-stored object's
    /// [`RemoteEntry::size`] / [`RemoteEntry::md5`] are exact; [`download`]
    /// cannot reproduce the bytes (it errors), so the oracle is for
    /// length+digest assertions, not byte-streaming round-trips. Default off:
    /// every other scenario keeps storing literal bytes. Set by
    /// [`fault_injection::with_content_oracle`].
    pub(crate) content_oracle: std::sync::atomic::AtomicBool,
}

impl Default for Faults {
    fn default() -> Self {
        use std::sync::atomic::AtomicBool;
        Self {
            rate_limit_after: AtomicU64::new(u64::MAX),
            five_xx_after: AtomicU64::new(u64::MAX),
            invalid_grant_after: AtomicU64::new(u64::MAX),
            network_drop_after: AtomicU64::new(u64::MAX),
            session_invalidated_after_chunks: AtomicU64::new(u64::MAX),
            md5_mismatch_after: AtomicU64::new(u64::MAX),
            quota_exhausted_after_bytes: AtomicU64::new(u64::MAX),
            daily_quota_after: AtomicU64::new(u64::MAX),
            daily_quota_latched: AtomicBool::new(false),
            response_delay_nanos: AtomicU64::new(0),
            invalid_grant_latched: AtomicBool::new(false),
            dest_folder_missing: AtomicBool::new(false),
            dest_folder_readonly: AtomicBool::new(false),
            trashed_visible_in_find: AtomicBool::new(false),
            fileid_recycle: AtomicBool::new(false),
            content_oracle: AtomicBool::new(false),
        }
    }
}

impl InMemoryRemoteStore {
    /// Creates an empty fake with a synthetic root folder. The root's
    /// id is what callers pass as `parent_id` when emulating "uploads
    /// to My Drive root".
    pub fn new() -> Self {
        let root_id = Uuid::new_v4().to_string();
        let root = FileEntry {
            file_id: root_id.clone(),
            name: "My Drive".to_string(),
            parent_id: String::new(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            bytes: Vec::new(),
            app_properties: HashMap::new(),
            modified_time_ms: 0,
            trashed: false,
            seq: 0,
            corrupted_md5: None,
            oracle: None,
        };
        let inner = Inner {
            objects: HashMap::from([(root_id.clone(), root)]),
            sessions: HashMap::new(),
            resumable_sessions_opened: 0,
            seq: 0,
            now_ms: 0,
            bytes_stored: 0,
            about_limit: Some(1u64 << 50),
            recycle_pool: std::collections::VecDeque::new(),
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
            root_id,
            faults: Arc::new(Faults::default()),
        }
    }

    /// Returns the synthetic root folder id. Tests pass this as
    /// `parent_id` when emulating uploads into My Drive root.
    pub fn root_id(&self) -> &str {
        &self.root_id
    }

    /// Internal test hook: list every child of `parent_id` including
    /// trashed objects. Mirrors the SPEC s3 contract-test bullet for
    /// "trash + list-with-trashed flag" (ROADMAP M1 acceptance).
    /// The trait method itself cannot take the flag, so this is a
    /// fake-only inherent accessor.
    pub fn list_folder_with_trashed(&self, folder_id: &str) -> Vec<RemoteEntry> {
        let guard = self.inner.lock();
        guard
            .children(folder_id)
            .into_iter()
            .map(|e| e.to_remote_entry())
            .collect()
    }

    /// Internal test hook: count open resumable sessions. Used by the
    /// contract suite to assert that completed / invalidated sessions
    /// are released.
    pub fn open_session_count(&self) -> usize {
        self.inner.lock().sessions.len()
    }

    /// Internal test hook: cumulative count of resumable sessions EVER opened
    /// (monotonic). Lets a test assert an upload never used the resumable path
    /// - e.g. a VSS snapshot read forced non-resumable (P1-B).
    pub fn resumable_sessions_opened(&self) -> u64 {
        self.inner.lock().resumable_sessions_opened
    }

    /// Internal test hook: total bytes of content currently stored in
    /// non-trashed object content.
    pub fn bytes_stored(&self) -> u64 {
        self.inner.lock().bytes_stored
    }

    /// Arm the session-invalidated-after-N-chunks fault on a specific,
    /// already-open session. After `n_chunks` more accepted chunks
    /// the session invalidates with
    /// [`ResumeProgress::SessionInvalid`]. Returns `false` if the
    /// session URL is unknown.
    ///
    /// Counterpart to the construction-time builder
    /// [`InMemoryRemoteStore::with_session_invalidated_after`]: the
    /// builder arms the NEXT session opened; this method arms THIS
    /// session specifically. Useful for tests that need to open
    /// multiple sessions and only fault one of them (the C.2 test
    /// case).
    pub fn arm_session_invalidated_after(&self, url: &str, n_chunks: u32) -> bool {
        let mut guard = self.inner.lock();
        if let Some(state) = guard.sessions.get_mut(url) {
            state.session_invalidated_after_chunks = u64::from(n_chunks) + 1;
            true
        } else {
            false
        }
    }

    // ---------------------------------------------------------------
    // Internal helpers.
    // ---------------------------------------------------------------

    /// Runs the per-request fault checks that every trait method shares
    /// (network drop, 5xx-after, rate limit, invalid_grant, dest-folder
    /// states). On a hit, returns the appropriate error.
    ///
    /// `path_kind` is a coarse classifier so dest-folder-missing /
    /// readonly only trip for create / update calls and not for, say,
    /// `about()` calls.
    ///
    /// Async because it first awaits any injected per-request latency
    /// ([`fault_injection::with_slow_responses`]). The sleep runs here -
    /// the single insertion point at the top of every trait method, BEFORE
    /// any `self.inner.lock()` - so the `.await` never spans a held
    /// `parking_lot` guard (DESIGN s5.8.1).
    /// Whether the streaming content oracle (P1-B) is armed: write paths keep
    /// only a length+md5 digest instead of buffering the literal bytes.
    fn oracle_on(&self) -> bool {
        self.faults.content_oracle.load(Ordering::Acquire)
    }

    async fn check_request_faults(&self, path_kind: RequestKind) -> anyhow::Result<()> {
        self.maybe_delay().await;
        // auth.invalid_grant is latched: once tripped it stays broken.
        if self.faults.invalid_grant_latched.load(Ordering::Acquire) {
            anyhow::bail!("fake: auth.invalid_grant (latched)");
        }
        if decrement_to_zero(&self.faults.invalid_grant_after) {
            self.faults
                .invalid_grant_latched
                .store(true, Ordering::Release);
            anyhow::bail!("fake: auth.invalid_grant");
        }
        if decrement_to_zero(&self.faults.network_drop_after) {
            anyhow::bail!("fake: net.intermittent (network drop)");
        }
        if decrement_to_zero(&self.faults.five_xx_after) {
            anyhow::bail!("fake: drive.unreachable (5xx)");
        }
        if decrement_to_zero(&self.faults.rate_limit_after) {
            anyhow::bail!("fake: drive.rate_limited");
        }
        // drive.daily_quota_exhausted (403 dailyLimitExceeded, P1-F): trips on
        // a WriteTarget call and LATCHES so the daily window stays closed for
        // the rest of the run. The message carries "daily" + "dailyLimitExceeded"
        // so the executor's `classify_drive_error` maps it to
        // `DriveError::DailyQuota` -> `ErrorCode::DriveDailyQuotaExhausted` and
        // the pacer pauses the account until midnight Pacific.
        if matches!(path_kind, RequestKind::WriteTarget) {
            if self.faults.daily_quota_latched.load(Ordering::Acquire) {
                anyhow::bail!(
                    "fake: drive.daily_quota_exhausted (403 dailyLimitExceeded, latched)"
                );
            }
            if decrement_to_zero(&self.faults.daily_quota_after) {
                self.faults
                    .daily_quota_latched
                    .store(true, Ordering::Release);
                anyhow::bail!("fake: drive.daily_quota_exhausted (403 dailyLimitExceeded)");
            }
        }
        if matches!(path_kind, RequestKind::WriteTarget)
            && self.faults.dest_folder_missing.load(Ordering::Acquire)
        {
            anyhow::bail!("fake: drive.dest_folder_missing");
        }
        if matches!(path_kind, RequestKind::WriteTarget)
            && self.faults.dest_folder_readonly.load(Ordering::Acquire)
        {
            anyhow::bail!("fake: drive.dest_folder_permission_denied");
        }
        Ok(())
    }

    /// Await the injected per-request latency, if any. Reads the delay,
    /// drops nothing (no guard is held here), then `tokio::time::sleep`s.
    /// A `0` delay (the default) is a no-op fast path. Used by
    /// [`fault_injection::with_slow_responses`] to model DESIGN s5.8.1's
    /// "lossy: +500ms latency".
    async fn maybe_delay(&self) {
        let nanos = self.faults.response_delay_nanos.load(Ordering::Acquire);
        if nanos != 0 {
            tokio::time::sleep(std::time::Duration::from_nanos(nanos)).await;
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum RequestKind {
    /// Read-only API call (list, metadata, download, about,
    /// find_by_op_uuid).
    Read,
    /// Mutating call against the destination folder (create, update,
    /// ensure_folder, resumable_session, resume_chunk). dest-folder
    /// missing / readonly faults trip here.
    WriteTarget,
}

/// Atomically decrement `counter` if it is non-zero and not `u64::MAX`.
/// Returns `true` iff the decrement crossed from 1 to 0 (the "trip"
/// edge). `u64::MAX` means "never trip" and is left alone.
fn decrement_to_zero(counter: &AtomicU64) -> bool {
    loop {
        let cur = counter.load(Ordering::Acquire);
        if cur == u64::MAX || cur == 0 {
            return false;
        }
        let next = cur - 1;
        if counter
            .compare_exchange(cur, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return next == 0;
        }
    }
}

impl Default for InMemoryRemoteStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// The trait impl.
// ---------------------------------------------------------------------------

#[async_trait]
impl RemoteStore for InMemoryRemoteStore {
    async fn ensure_folder(&self, parent_id: &str, name: &str) -> anyhow::Result<RemoteEntry> {
        self.check_request_faults(RequestKind::WriteTarget).await?;
        let mut guard = self.inner.lock();
        // Parent must be an existing FOLDER (not a file, not missing).
        guard.ensure_folder_parent(parent_id)?;

        // SPEC s3 `ensure_folder`: search by name; if multiple matches,
        // pick the one with our folder marker; else the oldest non-
        // trashed and log a warning. Create if none.
        let mut matches: Vec<&FileEntry> = guard
            .children(parent_id)
            .into_iter()
            .filter(|e| e.is_folder() && !e.trashed && e.name == name)
            .collect();

        if let Some(marked) = matches
            .iter()
            .find(|e| e.app_properties.contains_key(FOLDER_MARKER_KEY))
            .copied()
        {
            return Ok(marked.to_remote_entry());
        }

        if matches.len() > 1 {
            warn!(
                target: TARGET,
                parent_id = %parent_id,
                name = %name,
                duplicates = matches.len(),
                "ensure_folder found multiple non-marker matches; picking oldest"
            );
        }
        if !matches.is_empty() {
            matches.sort_by_key(|e| e.seq);
            return Ok(matches[0].to_remote_entry());
        }
        drop(matches);

        // Create.
        let file_id = Uuid::new_v4().to_string();
        let seq = guard.next_seq();
        let modified_time_ms = guard.tick();
        let mut app_properties = HashMap::new();
        app_properties.insert(FOLDER_MARKER_KEY.to_string(), "1".to_string());
        let entry = FileEntry {
            file_id: file_id.clone(),
            name: name.to_string(),
            parent_id: parent_id.to_string(),
            mime_type: "application/vnd.google-apps.folder".to_string(),
            bytes: Vec::new(),
            app_properties,
            modified_time_ms,
            trashed: false,
            seq,
            corrupted_md5: None,
            oracle: None,
        };
        let out = entry.to_remote_entry();
        guard.objects.insert(file_id, entry);
        Ok(out)
    }

    async fn list_folder(&self, folder_id: &str) -> anyhow::Result<Vec<RemoteEntry>> {
        self.check_request_faults(RequestKind::Read).await?;
        let guard = self.inner.lock();
        if !guard.objects.contains_key(folder_id) {
            anyhow::bail!("fake: folder {folder_id} does not exist");
        }
        Ok(guard
            .children(folder_id)
            .into_iter()
            .filter(|e| !e.trashed)
            .map(|e| e.to_remote_entry())
            .collect())
    }

    async fn create(
        &self,
        parent_id: &str,
        name: &str,
        mime: &str,
        body: UploadBody,
        app_properties: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        self.check_request_faults(RequestKind::WriteTarget).await?;
        // Drain stream BEFORE locking (advisor finding #3): never hold
        // a parking_lot::Mutex guard across an .await. Under the content
        // oracle (P1-B) the bytes are digested, not buffered.
        let body = collect_body(body, self.oracle_on()).await?;
        let content_len = body.len();

        // Quota check.
        if let Some(remaining) =
            try_consume_bytes(&self.faults.quota_exhausted_after_bytes, content_len)
        {
            anyhow::bail!(
                "fake: drive.quota_exhausted (over budget by {}B)",
                content_len - remaining
            );
        }

        let (bytes, oracle) = body_into_entry_fields(body);
        let mut guard = self.inner.lock();
        // Parent must be an existing FOLDER (not a file, not missing).
        guard.ensure_folder_parent(parent_id)?;
        // Reuse a recycled id if the fileid_recycle fault has queued one
        // (STRESS_HARNESS s3.7); otherwise mint fresh.
        let file_id = guard.alloc_file_id();
        let seq = guard.next_seq();
        let modified_time_ms = guard.tick();
        let mut entry = FileEntry {
            file_id: file_id.clone(),
            name: name.to_string(),
            parent_id: parent_id.to_string(),
            mime_type: mime.to_string(),
            bytes,
            app_properties,
            modified_time_ms,
            trashed: false,
            seq,
            corrupted_md5: None,
            oracle,
        };
        guard.bytes_stored = guard.bytes_stored.saturating_add(content_len);
        // Latch the md5 fault on the entry itself so every subsequent
        // read (metadata, list_folder, find_by_op_uuid, ...) returns the
        // bad md5 until the file is rewritten.
        maybe_latch_md5_mismatch(&self.faults, &mut entry);
        let out = entry.to_remote_entry();
        guard.objects.insert(file_id, entry);
        Ok(out)
    }

    async fn update(
        &self,
        file_id: &str,
        body: UploadBody,
        app_properties_patch: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        self.check_request_faults(RequestKind::WriteTarget).await?;
        let body = collect_body(body, self.oracle_on()).await?;

        let new_len = body.len();
        let (bytes, oracle) = body_into_entry_fields(body);
        let mut guard = self.inner.lock();
        let new_now = guard.tick();
        let entry = guard
            .objects
            .get_mut(file_id)
            .ok_or_else(|| anyhow::anyhow!("fake: no object with file_id {file_id}"))?;
        let old_len = entry.content_len().unwrap_or(0);
        entry.bytes = bytes;
        entry.oracle = oracle;
        for (k, v) in app_properties_patch {
            entry.app_properties.insert(k, v);
        }
        entry.modified_time_ms = new_now;
        // Re-upload clears any prior md5-mismatch latch on this entry
        // (the executor's checksum-retry path is supposed to recover
        // from a transient corruption by re-uploading the bytes).
        entry.corrupted_md5 = None;
        maybe_latch_md5_mismatch(&self.faults, entry);
        // Build the RemoteEntry *before* touching `guard.bytes_stored`
        // - NLL ends the &mut FileEntry borrow at last use, allowing
        // the subsequent reborrow of `guard`.
        let out = entry.to_remote_entry();
        if new_len >= old_len {
            guard.bytes_stored = guard.bytes_stored.saturating_add(new_len - old_len);
        } else {
            guard.bytes_stored = guard.bytes_stored.saturating_sub(old_len - new_len);
        }
        Ok(out)
    }

    async fn resumable_session(
        &self,
        kind: ResumableKind,
        mime: &str,
        size: u64,
    ) -> anyhow::Result<ResumableSession> {
        self.check_request_faults(RequestKind::WriteTarget).await?;
        let url = format!("memory://fake/resumable/{}", Uuid::new_v4());

        // Validate the target up front (matches Drive's "open the
        // session, validate identity, return session URL"). For
        // `Update`, the file_id must exist; for `Create`, the parent
        // must exist.
        let mut guard = self.inner.lock();
        match &kind {
            ResumableKind::Create { parent_id, .. } => {
                // Parent must be an existing FOLDER (not a file, not missing).
                guard.ensure_folder_parent(parent_id)?;
            }
            ResumableKind::Update { file_id } => {
                guard.ensure_object(file_id)?;
            }
        }
        let issued_at = guard.tick();
        // Bind the global `session_invalidated_after_chunks` counter to
        // this session if it has been armed via
        // [`fault_injection::with_session_invalidated_after`]. The
        // global is reset to `u64::MAX` so subsequently-opened sessions
        // are unaffected, satisfying the C.2 "B does not consume A's
        // budget" requirement. Use a swap so concurrent
        // resumable_session calls race for the binding deterministically
        // (whoever wins the swap gets the budget).
        let armed = self
            .faults
            .session_invalidated_after_chunks
            .swap(u64::MAX, Ordering::AcqRel);
        guard.resumable_sessions_opened = guard.resumable_sessions_opened.saturating_add(1);
        guard.sessions.insert(
            url.clone(),
            ResumableSessionState {
                size,
                // Do NOT preallocate `size`: a 1 GiB declared upload must not
                // commit 1 GiB up front (M3's memory-ceiling acceptance test
                // would otherwise fail inside the fake). Grow per chunk. Under
                // the content oracle (P1-B) accepted chunks fold into a
                // length+md5 digest, never buffered, so a 10-50 GB resumable
                // upload verifies by length+digest without OOMing.
                received: if self.oracle_on() {
                    Received::Digest {
                        len: 0,
                        hasher: <md5::Md5 as md5::Digest>::new(),
                    }
                } else {
                    Received::Bytes(Vec::new())
                },
                mime: mime.to_string(),
                kind: clone_kind(&kind),
                issued_at,
                invalid: false,
                session_invalidated_after_chunks: armed,
            },
        );
        Ok(ResumableSession {
            url,
            issued_at,
            size,
            kind,
        })
    }

    async fn resume_chunk(
        &self,
        session: &ResumableSession,
        offset: u64,
        chunk: Bytes,
    ) -> anyhow::Result<ResumeProgress> {
        self.check_request_faults(RequestKind::WriteTarget).await?;

        let mut guard = self.inner.lock();
        let state = guard
            .sessions
            .get_mut(&session.url)
            .ok_or_else(|| anyhow::anyhow!("fake: unknown session {}", session.url))?;

        if state.invalid {
            return Ok(ResumeProgress::SessionInvalid);
        }

        if offset != state.received.len() {
            // Drive responds with 308 + Range header to request the
            // bytes it actually has. The trait does not surface that
            // back-channel; treat as a session-invalidating client bug.
            state.invalidate();
            return Ok(ResumeProgress::SessionInvalid);
        }
        if offset + chunk.len() as u64 > state.size {
            state.invalidate();
            return Ok(ResumeProgress::SessionInvalid);
        }

        let is_final = offset + chunk.len() as u64 == state.size;
        if !is_final && (chunk.len() as u64) % CHUNK_MULTIPLE != 0 {
            // SPEC s3 `resume_chunk`: non-final chunks must be a
            // multiple of 256 KiB. The real Drive returns 400 here; the
            // trait surfaces that as `SessionInvalid` (advisor finding
            // #4 reconciliation: keep the contract test portable to the
            // real GoogleDriveStore in M4).
            state.invalidate();
            return Ok(ResumeProgress::SessionInvalid);
        }

        // Per-session chunk-invalidation budget. Decrement only AFTER
        // chunk-validity checks pass so a malformed chunk does not
        // burn budget on the wrong session. Trip when the counter
        // reaches zero (C.2 fix).
        if state.session_invalidated_after_chunks != u64::MAX
            && state.session_invalidated_after_chunks > 0
        {
            state.session_invalidated_after_chunks -= 1;
            if state.session_invalidated_after_chunks == 0 {
                state.invalidate();
                return Ok(ResumeProgress::SessionInvalid);
            }
        }

        state.received.accept(&chunk);
        if !is_final {
            return Ok(ResumeProgress::InProgress {
                received: state.received.len(),
            });
        }

        // Final chunk: commit. Take ownership of the session state out
        // of the map so the mutable borrow ends cleanly before we
        // mutate other fields of `guard` (objects, seq, now_ms).
        // `expect` is the SPEC s0 "trivially-unreachable" carve-out:
        // the same lock guarded the `get_mut` above, so the entry is
        // still present.
        let removed = guard
            .sessions
            .remove(&session.url)
            .expect("session existed under the same lock");
        let (received_bytes, received_oracle, received_len) = removed.received.finish();
        let mime = removed.mime;
        let kind = removed.kind;

        // Quota check (charged at commit time, mirroring how Drive only
        // bills you for the persisted bytes).
        if let Some(remaining) =
            try_consume_bytes(&self.faults.quota_exhausted_after_bytes, received_len)
        {
            anyhow::bail!(
                "fake: drive.quota_exhausted (over budget by {}B)",
                received_len - remaining
            );
        }

        let entry = match kind {
            ResumableKind::Create {
                parent_id,
                name,
                app_properties,
            } => {
                // Parent must still be an existing FOLDER (not a file,
                // not missing) at commit time.
                if guard.ensure_folder_parent(&parent_id).is_err() {
                    anyhow::bail!(
                        "fake: parent folder {parent_id} missing or not a folder mid-upload"
                    );
                }
                // Reuse a recycled id if armed (STRESS_HARNESS s3.7).
                let file_id = guard.alloc_file_id();
                let seq = guard.next_seq();
                let modified_time_ms = guard.tick();
                let mut entry = FileEntry {
                    file_id: file_id.clone(),
                    name,
                    parent_id,
                    mime_type: mime,
                    bytes: received_bytes,
                    app_properties,
                    modified_time_ms,
                    trashed: false,
                    seq,
                    corrupted_md5: None,
                    oracle: received_oracle,
                };
                guard.bytes_stored = guard.bytes_stored.saturating_add(received_len);
                maybe_latch_md5_mismatch(&self.faults, &mut entry);
                let out = entry.to_remote_entry();
                guard.objects.insert(file_id, entry);
                out
            }
            ResumableKind::Update { file_id } => {
                let new_len = received_len;
                let new_now = guard.tick();
                let entry = guard.objects.get_mut(&file_id).ok_or_else(|| {
                    anyhow::anyhow!("fake: file {file_id} disappeared mid-upload")
                })?;
                let old_len = entry.content_len().unwrap_or(0);
                entry.bytes = received_bytes;
                entry.oracle = received_oracle;
                entry.modified_time_ms = new_now;
                // Re-upload clears any prior md5 latch (see `update`).
                entry.corrupted_md5 = None;
                maybe_latch_md5_mismatch(&self.faults, entry);
                // Build the RemoteEntry *before* reborrowing `guard`
                // for `bytes_stored`. NLL ends the &mut FileEntry
                // borrow at the last use of `entry`.
                let out = entry.to_remote_entry();
                if new_len >= old_len {
                    guard.bytes_stored = guard.bytes_stored.saturating_add(new_len - old_len);
                } else {
                    guard.bytes_stored = guard.bytes_stored.saturating_sub(old_len - new_len);
                }
                out
            }
        };
        Ok(ResumeProgress::Completed(entry))
    }

    async fn trash(&self, file_id: &str) -> anyhow::Result<()> {
        self.check_request_faults(RequestKind::WriteTarget).await?;
        // When file_id recycling is armed, a trash REMOVES the object (as if
        // the user emptied it from trash) and queues its id for reuse by the
        // next `create`, reproducing genuine Drive id recycling
        // (STRESS_HARNESS s3.7 `drive-fileid-recycled`). Otherwise trash is
        // the normal soft-delete: flip the `trashed` flag in place.
        let recycle = self.faults.fileid_recycle.load(Ordering::Acquire);
        let mut guard = self.inner.lock();
        if recycle {
            let freed = match guard.objects.remove(file_id) {
                Some(entry) => {
                    let was = if entry.trashed {
                        0
                    } else {
                        entry.bytes.len() as u64
                    };
                    // Only files recycle their id (folders never recycle).
                    if !entry.is_folder() {
                        guard.recycle_pool.push_back(entry.file_id);
                    }
                    was
                }
                // 404 is treated as success (SPEC s3 `trash`).
                None => 0,
            };
            guard.bytes_stored = guard.bytes_stored.saturating_sub(freed);
            return Ok(());
        }
        let freed = match guard.objects.get_mut(file_id) {
            // Idempotent: trashing an already-trashed file succeeds.
            Some(entry) => {
                if entry.trashed {
                    0
                } else {
                    let was = entry.bytes.len() as u64;
                    entry.trashed = true;
                    was
                }
            }
            // 404 is treated as success (SPEC s3 `trash`).
            None => 0,
        };
        guard.bytes_stored = guard.bytes_stored.saturating_sub(freed);
        Ok(())
    }

    async fn metadata(&self, file_id: &str) -> anyhow::Result<RemoteEntry> {
        self.check_request_faults(RequestKind::Read).await?;
        let guard = self.inner.lock();
        let entry = guard.ensure_object(file_id)?;
        // Md5 mismatch (if any) is latched on the entry at write time -
        // see [`maybe_latch_md5_mismatch`]. Reads just round-trip
        // [`FileEntry::corrupted_md5`].
        Ok(entry.to_remote_entry())
    }

    async fn download(&self, file_id: &str) -> anyhow::Result<DownloadStream> {
        self.check_request_faults(RequestKind::Read).await?;
        let guard = self.inner.lock();
        let entry = guard.ensure_object(file_id)?;
        if entry.is_folder() {
            anyhow::bail!("fake: cannot download folder {file_id}");
        }
        if entry.oracle.is_some() {
            // Oracle-stored object (P1-B): the literal bytes were never
            // retained, only the length + md5. The huge-file rows verify by
            // length+digest, not byte round-trip; error rather than hand back
            // empty bytes that would masquerade as valid content.
            anyhow::bail!(
                "fake: object {file_id} was stored under the content oracle; its bytes are not retained (length+md5 only)"
            );
        }
        let bytes = entry.bytes.clone();
        drop(guard);
        Ok(DownloadStream(Box::new(InMemoryReader::new(bytes))))
    }

    async fn find_by_op_uuid(
        &self,
        parent_id: &str,
        op_uuid: &str,
    ) -> anyhow::Result<Option<RemoteEntry>> {
        self.check_request_faults(RequestKind::Read).await?;
        let include_trashed = self.faults.trashed_visible_in_find.load(Ordering::Acquire);
        let guard = self.inner.lock();
        let mut matches: Vec<&FileEntry> = guard
            .children(parent_id)
            .into_iter()
            .filter(|e| {
                if e.trashed && !include_trashed {
                    return false;
                }
                e.app_properties
                    .get(CLIENT_OP_UUID_KEY)
                    .map(|v| v == op_uuid)
                    .unwrap_or(false)
            })
            .collect();
        if matches.is_empty() {
            return Ok(None);
        }
        if matches.len() > 1 {
            warn!(
                target: TARGET,
                parent_id = %parent_id,
                op_uuid = %op_uuid,
                duplicates = matches.len(),
                "find_by_op_uuid found multiple matches; returning most-recent by seq"
            );
        }
        // Most-recent by monotonic seq (deterministic for tests).
        matches.sort_by_key(|e| std::cmp::Reverse(e.seq));
        Ok(Some(matches[0].to_remote_entry()))
    }

    async fn about(&self) -> anyhow::Result<AboutInfo> {
        // Deliberately bypasses `check_request_faults` for the
        // dest-folder bits but still honours rate-limit / 5xx /
        // network-drop / invalid-grant (which would also kill an
        // `about` call against real Drive).
        self.check_request_faults(RequestKind::Read).await?;
        let guard = self.inner.lock();
        let trash_bytes: u64 = guard
            .objects
            .values()
            .filter(|e| e.trashed)
            .map(|e| e.bytes.len() as u64)
            .sum();
        Ok(AboutInfo {
            limit: guard.about_limit,
            usage: guard.bytes_stored.saturating_add(trash_bytes),
            usage_in_drive: guard.bytes_stored,
            usage_in_drive_trash: trash_bytes,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers shared by the trait methods.
// ---------------------------------------------------------------------------

/// The content collected from an [`UploadBody`]: either the literal bytes
/// (normal mode) or a streaming length+md5 digest (oracle mode, P1-B - so a
/// 10-50 GB upload never buffers the whole content in a `Vec<u8>`).
enum CollectedBody {
    /// The full literal bytes were retained.
    Bytes(Vec<u8>),
    /// Only the length + md5 digest were retained (content oracle).
    Digest(OracleDigest),
}

impl CollectedBody {
    /// The content length in bytes, regardless of mode.
    fn len(&self) -> u64 {
        match self {
            CollectedBody::Bytes(b) => b.len() as u64,
            CollectedBody::Digest(d) => d.len,
        }
    }
}

/// Drain an [`UploadBody`]. In normal mode the bytes are buffered into a
/// `Vec<u8>`; when `oracle` is true the bytes are STREAMED through an md5
/// hasher and only the length + digest are kept (P1-B), so a multi-gigabyte
/// upload never holds the whole content in memory. Done before any lock is
/// acquired so we never hold a parking_lot guard across an `.await` (advisor
/// finding #3).
async fn collect_body(body: UploadBody, oracle: bool) -> anyhow::Result<CollectedBody> {
    use md5::{Digest, Md5};
    match body {
        UploadBody::Bytes(b) => {
            if oracle {
                let mut hasher = Md5::new();
                hasher.update(&b);
                let mut md5 = [0u8; 16];
                md5.copy_from_slice(&hasher.finalize());
                Ok(CollectedBody::Digest(OracleDigest {
                    len: b.len() as u64,
                    md5,
                }))
            } else {
                Ok(CollectedBody::Bytes(b.to_vec()))
            }
        }
        UploadBody::Stream { len, mut stream } => {
            if oracle {
                // Stream through an md5 hasher + byte counter; never buffer the
                // content. This is what lets the huge-file rows verify by
                // length+digest without OOMing on 10-50 GB.
                let mut hasher = Md5::new();
                let mut count: u64 = 0;
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    hasher.update(&chunk);
                    count += chunk.len() as u64;
                }
                if count != len {
                    anyhow::bail!("fake: stream length mismatch: declared {len}, got {count}");
                }
                let mut md5 = [0u8; 16];
                md5.copy_from_slice(&hasher.finalize());
                Ok(CollectedBody::Digest(OracleDigest { len, md5 }))
            } else {
                // Do NOT preallocate `len` here - a large declared upload would
                // commit that memory up front. Let the buffer grow per chunk.
                let mut buf = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk?;
                    buf.extend_from_slice(&chunk);
                }
                // The fake is the test oracle for a backup tool: a truncated (or
                // over-long) stream must NOT be silently accepted with a valid
                // MD5 of the wrong bytes. Reject any length mismatch against the
                // declared `len` so a corrupt/incomplete backup can never pass
                // here when it would fail in production.
                let actual = buf.len() as u64;
                if actual != len {
                    anyhow::bail!("fake: stream length mismatch: declared {len}, got {actual}");
                }
                Ok(CollectedBody::Bytes(buf))
            }
        }
    }
}

/// Split a [`CollectedBody`] into `(bytes, oracle)` for storing in a
/// [`FileEntry`]: a literal body yields the bytes + `oracle: None`; a digest
/// body yields an empty byte vec + the digest.
fn body_into_entry_fields(body: CollectedBody) -> (Vec<u8>, Option<OracleDigest>) {
    match body {
        CollectedBody::Bytes(b) => (b, None),
        CollectedBody::Digest(d) => (Vec::new(), Some(d)),
    }
}

/// `ResumableKind` doesn't derive `Clone` (it carries owned strings
/// the caller may need to move). The fake needs to stash a copy in
/// the session state and surface a freshly-cloned one back on the
/// returned [`ResumableSession`]. Hand-rolled here rather than
/// derived to avoid changing the public type for a fake-internal
/// concern.
fn clone_kind(kind: &ResumableKind) -> ResumableKind {
    match kind {
        ResumableKind::Create {
            parent_id,
            name,
            app_properties,
        } => ResumableKind::Create {
            parent_id: parent_id.clone(),
            name: name.clone(),
            app_properties: app_properties.clone(),
        },
        ResumableKind::Update { file_id } => ResumableKind::Update {
            file_id: file_id.clone(),
        },
    }
}

/// Try to consume `n_bytes` from the quota counter. Returns
/// `Some(remaining_before_charge)` iff the consumption would exceed
/// the budget (the request is rejected), `None` if there was budget
/// and the counter has been decremented.
///
/// `u64::MAX` means "unlimited" and always succeeds without
/// decrementing.
fn try_consume_bytes(counter: &AtomicU64, n_bytes: u64) -> Option<u64> {
    loop {
        let cur = counter.load(Ordering::Acquire);
        if cur == u64::MAX {
            return None;
        }
        if n_bytes > cur {
            return Some(cur);
        }
        let next = cur - n_bytes;
        if counter
            .compare_exchange(cur, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return None;
        }
    }
}

/// Latch the md5-mismatch fault on `entry` if the `with_md5_mismatch_after`
/// counter trips on this request.
///
/// The real file content is unaffected; the entry's `corrupted_md5`
/// holds a deliberately-wrong md5 (every bit flipped vs the truth) so
/// every subsequent read path that calls
/// [`FileEntry::to_remote_entry`] keeps returning the wrong value
/// until the entry is rewritten (`update` / final-chunk commit). This
/// matches the SPEC s8 "md5 mismatch -> retry" path: the executor will
/// see the bad md5 on metadata / list_folder / find_by_op_uuid until
/// it re-uploads the file.
fn maybe_latch_md5_mismatch(faults: &Faults, entry: &mut FileEntry) {
    if decrement_to_zero(&faults.md5_mismatch_after) {
        let mut wrong = entry.md5().unwrap_or([0u8; 16]);
        for b in &mut wrong {
            *b ^= 0xff;
        }
        entry.corrupted_md5 = Some(wrong);
    }
}

// ---------------------------------------------------------------------------
// DownloadStream backing.
// ---------------------------------------------------------------------------

/// `tokio::io::AsyncRead` adapter over an owned `Vec<u8>`. We can't
/// hand back a `tokio::io::ReaderStream`-style adapter directly
/// because `DownloadStream` boxes `dyn AsyncRead + Send + Unpin`;
/// `std::io::Cursor<Vec<u8>>` implements all three but `tokio`'s
/// blanket `AsyncRead for T: std::io::Read` isn't quite that, so we
/// hand-roll the trivial impl.
struct InMemoryReader {
    buf: Vec<u8>,
    pos: usize,
}

impl InMemoryReader {
    fn new(buf: Vec<u8>) -> Self {
        Self { buf, pos: 0 }
    }
}

impl AsyncRead for InMemoryReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let remaining = this.buf.len().saturating_sub(this.pos);
        if remaining == 0 {
            return Poll::Ready(Ok(()));
        }
        let to_copy = remaining.min(buf.remaining());
        buf.put_slice(&this.buf[this.pos..this.pos + to_copy]);
        this.pos += to_copy;
        Poll::Ready(Ok(()))
    }
}
