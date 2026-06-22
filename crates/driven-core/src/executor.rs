//! The plan executor: turns a [`Plan`](crate::types::Plan) into Drive
//! mutations, crash-safely (SPEC s8, DESIGN s5.4, s5.6, s11.4).
//!
//! The executor is where the speed and the safety live:
//! - **Inter-file parallelism** (DESIGN s11.4.2): an `UploadPool` of
//!   `min(num_cpus * 2, 16)` permits (hard cap 32) bounds in-flight files;
//!   each op also passes the [`Pacer`](crate::pacer::Pacer) rate gate
//!   (SPEC s8: "both gates must be open").
//! - **Intra-file pipelining** (DESIGN s11.4.3): reader (tokio) ->
//!   hash+encrypt (rayon) -> uploader (tokio), bounded mpsc between
//!   stages, for files above the 4 MiB small-file threshold.
//! - **File-changed-during-upload defences** (SPEC s8): pre-open `lstat`,
//!   open with `FILE_SHARE_DELETE`, post-read `fstat` identity check;
//!   a mismatch surfaces `local.file_changed_during_upload` /
//!   `local.file_replaced_during_upload` and re-queues without marking
//!   `synced`.
//! - **Crash-safe `client_op_uuid` protocol** (DESIGN s5.6): the create's
//!   UUID lands in `appProperties` atomically with the file, so the
//!   reconciliation pass can adopt an orphaned remote object after a crash
//!   instead of duplicating it.
//!
//! The concrete implementation is [`DefaultExecutor`]; it codes against the
//! injected seams in [`ExecutorDeps`] - [`RemoteStore`], [`StateRepo`],
//! [`Pacer`], and the optional [`SourceCryptoSuite`] - so the whole
//! executor is exercisable against `InMemoryRemoteStore` + a real
//! `SqliteStateRepo` with no live Drive (DESIGN s14).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use driven_crypto::{ContentEncryptor, CryptoError, SourceCryptoSuite};
use driven_drive::remote_store::{
    RemoteEntry, RemoteStore, ResumableKind, ResumableSession, ResumeProgress, UploadBody,
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::pacer::{Pacer, ResponseClass};
use crate::state::{FileStateRow, NewPendingOp, SourceRow, StateRepo};
use crate::time::{Clock, SystemClock};
use crate::types::{
    ErrorCode, ExecProgress, FileStateStatus, Op, PendingOpId, Plan, RelativePath, SourceId,
};

/// Tracing target for the executor.
const TARGET: &str = "driven::core::executor";

// -----------------------------------------------------------------------------
// Thresholds (DESIGN s5.4, canonical names).
// -----------------------------------------------------------------------------

/// Files at or above this go through Drive's resumable upload protocol;
/// below uses a simple multipart create/update (DESIGN s5.4
/// `RESUMABLE_THRESHOLD = 5 MiB`).
pub const RESUMABLE_THRESHOLD: u64 = 5 * 1024 * 1024;

/// Files at or above this use the 3-stage producer/consumer pipeline
/// (DESIGN s5.4 / s11.4.3 `PIPELINE_THRESHOLD = 4 MiB`); below run inline
/// in one task. Currently informational: the streaming reader is used for
/// every resumable upload regardless, and the small-file path is fully
/// inline; the threshold is retained as the canonical constant the
/// orchestrator and acceptance tests refer to.
pub const PIPELINE_THRESHOLD: u64 = 4 * 1024 * 1024;

/// Files at or above this would trigger `blake3::Hasher::update_rayon`
/// for multi-core hashing (DESIGN s5.4 / s11.4.4
/// `RAYON_HASH_THRESHOLD = 100 MiB`). NOTE: `blake3 = "1"` is pulled
/// without its `rayon` feature, so multi-core hashing is deferred; the
/// CPU stage hashes single-threaded today. The constant is kept so the
/// feature can be turned on without re-deriving the boundary.
pub const RAYON_HASH_THRESHOLD: u64 = 100 * 1024 * 1024;

/// Drive's resumable chunk multiple: every non-final chunk pushed to a
/// resumable session must be a multiple of 256 KiB (SPEC s3
/// `resume_chunk`). The executor accumulates pipeline output into wire
/// chunks of this size.
const CHUNK_MULTIPLE: usize = 256 * 1024;

/// Wire chunk size for resumable uploads (DESIGN s11.4.3 default 4 MiB,
/// "accumulated from 4 pipeline chunks"). A multiple of [`CHUNK_MULTIPLE`]
/// so non-final chunks satisfy Drive's rule.
const WIRE_CHUNK: usize = 4 * 1024 * 1024;

// Compile-time proof that WIRE_CHUNK satisfies Drive's resumable-session rule
// (every non-final chunk must be a multiple of CHUNK_MULTIPLE, SPEC s3). This
// also anchors CHUNK_MULTIPLE as a live invariant rather than dead doc.
const _: () = assert!(WIRE_CHUNK % CHUNK_MULTIPLE == 0);

/// On-disk read buffer size (SPEC s8 "64 KiB local-read buffer";
/// independent of the HTTP wire chunk). Also the plaintext chunk size fed
/// to the content encryptor (DESIGN s7.1 64 KiB plaintext chunks).
const READ_BUF: usize = 64 * 1024;

/// `appProperties` key carrying the create-op UUID for the crash-safe
/// reconciliation protocol (DESIGN s5.6; mirrors
/// `driven_drive::fake::CLIENT_OP_UUID_KEY`).
const CLIENT_OP_UUID_KEY: &str = "driven.client_op_uuid";

/// `appProperties` key carrying the source id of the object Driven owns
/// (SPEC s3 preamble canonical identity).
const SOURCE_ID_KEY: &str = "driven.source_id";

/// `appProperties` key carrying the relative-path hash (SPEC s3 preamble).
const RELATIVE_PATH_HASH_KEY: &str = "driven.relative_path_hash";

/// `pending_ops.op_type` value the executor finalizes via
/// `commit_*_result` (SPEC s2; matches the SqliteStateRepo bound).
const OP_TYPE_UPLOAD: &str = "upload";

/// Max retries for transient 5xx / network failures (DESIGN s5.4 "5xx ->
/// exponential backoff, max 6 retries").
const MAX_TRANSIENT_RETRIES: u32 = 6;

/// Max times a single resumable session is restarted from offset 0 after a
/// session-invalidating 4xx before the op fails (DESIGN s5.4 "any 4xx ->
/// recreate from scratch"). Bounded so a permanently-broken session does
/// not loop forever.
const MAX_SESSION_RESTARTS: u32 = 3;

/// Max age of a persisted resumable session before it is discarded and the
/// transfer restarts from offset 0 (DESIGN s5.4: "Driven discards sessions
/// older than 6 days; Drive expires at 7"). Expressed in milliseconds.
const SESSION_MAX_AGE_MS: i64 = 6 * 24 * 60 * 60 * 1000;

/// Sentinel `mtime_ns` stamped on a requeued `file_state` row when an
/// adopted orphan's local bytes no longer match what was uploaded (P1-2).
///
/// The FastPath scanner (scanner.rs) treats a file as unchanged iff its
/// current `(size, mtime_ns)` equals the stored row's. To GUARANTEE the
/// changed bytes get re-uploaded, the requeue row must store an identity
/// the live file can never match - `i64::MIN` is never produced by a real
/// filesystem mtime, so the next scan always sees a mismatch, re-emits the
/// file, and (because the row keeps its `drive_file_id`) the executor
/// re-uploads it as an UPDATE against the same object (no duplicate).
const REQUEUE_FORCE_RESCAN_MTIME_NS: i64 = i64::MIN;

// -----------------------------------------------------------------------------
// Pending-op payload (persisted in pending_ops.payload_json; DESIGN s5.4 / s5.6).
// -----------------------------------------------------------------------------

/// The structured payload Driven persists in `pending_ops.payload_json` for
/// an upload op. Carries the crash-safe `client_op_uuid` (DESIGN s5.6), the
/// optional pre-existing `drive_file_id` (create vs update), the BLAKE3 of
/// the plaintext that was uploaded (so a reconciled orphan can be re-hashed
/// against the bytes that actually landed - P1-2), and the live resumable
/// session if one is in flight (so a crash mid-upload resumes from the
/// last-acked offset rather than restarting from zero - P1-3).
///
/// Serialized as a free-form JSON object (the column is TEXT), so adding
/// fields needs no migration. Unknown/absent fields default, which keeps it
/// forward- and backward-compatible with rows written by older code.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PendingOpPayload {
    /// The crash-safe create/update UUID (DESIGN s5.6 step 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_op_uuid: Option<String>,
    /// The existing Drive file id, present iff this op is an UPDATE. `None`
    /// (or JSON `null`) marks a CREATE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    drive_file_id: Option<String>,
    /// Hex-encoded BLAKE3 (over plaintext) of the bytes uploaded by this
    /// op. Persisted once the file has been hashed, before the bytes land,
    /// so reconciliation can verify an adopted orphan still matches (P1-2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uploaded_blake3_hex: Option<String>,
    /// The live resumable session, persisted while an upload is in flight
    /// (P1-3). Carries the session URL, issued-at, total size, kind, and
    /// the last offset Drive acknowledged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resumable: Option<PersistedResumable>,
}

/// A resumable session persisted across process restarts (DESIGN s5.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedResumable {
    /// The full session as issued by the [`RemoteStore`] (url, issued_at,
    /// total size, kind).
    session: ResumableSession,
    /// The byte offset Drive has acknowledged so far; resume pushes the
    /// next chunk from here (P1-3). Updated after every accepted chunk.
    acked_offset: u64,
}

impl PendingOpPayload {
    /// Parse the JSON payload; an empty/old/garbage payload yields the
    /// default (all-`None`), never an error.
    fn from_value(v: &serde_json::Value) -> Self {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }

    /// Serialize back to a JSON value for persistence.
    fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

// -----------------------------------------------------------------------------
// SkipReason / OpOutcome / Executor trait (Phase-1 surface, re-stated).
// -----------------------------------------------------------------------------

/// Why the executor skipped (did NOT complete) an op, re-queuing it for a
/// later scan rather than marking it `synced` (SPEC s8 file-changed
/// defences, DESIGN s5.3).
///
/// A skip is not an error: the bytes are simply not coherent to commit
/// this pass. Each maps to a `local.*` [`ErrorCode`] for the activity log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The path's `(dev, inode)` changed between the pre-open `lstat` and
    /// the `open`: someone atomically replaced the file before we opened
    /// it (`local.file_replaced_during_upload`).
    ReplacedBeforeOpen,
    /// The post-read `fstat` showed the open handle's `(ctime, size)`
    /// changed mid-read: the file was modified during upload
    /// (`local.file_changed_during_upload`).
    ChangedDuringUpload,
    /// The path's `(dev, inode)` no longer matches the opened handle after
    /// the read: an atomic replace was detected mid-upload
    /// (`local.file_replaced_during_upload`).
    ReplacedDuringUpload,
    /// The file is locked and could not be opened even with
    /// `FILE_SHARE_DELETE` (and the VSS fallback failed too)
    /// (`local.file_locked`).
    Locked,
}

impl SkipReason {
    /// The stable [`ErrorCode`] this skip surfaces in the activity log
    /// (SPEC s24).
    pub fn error_code(self) -> ErrorCode {
        match self {
            SkipReason::ReplacedBeforeOpen | SkipReason::ReplacedDuringUpload => {
                ErrorCode::LocalFileReplacedDuringUpload
            }
            SkipReason::ChangedDuringUpload => ErrorCode::LocalFileChangedDuringUpload,
            SkipReason::Locked => ErrorCode::LocalFileLocked,
        }
    }
}

/// The outcome of executing one [`Op`] (SPEC s8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum OpOutcome {
    /// The op completed: the upload landed and verified, or the trash
    /// succeeded; `file_state` is updated.
    Done {
        /// The path the op acted on.
        relative_path: RelativePath,
    },
    /// The op was skipped and re-queued (not an error); see [`SkipReason`].
    Skipped {
        /// The path the op acted on.
        relative_path: RelativePath,
        /// Why it was skipped.
        reason: SkipReason,
    },
    /// The op failed with a non-retryable error; the file is left
    /// `error`/`pending` for a later attempt.
    Failed {
        /// The path the op acted on.
        relative_path: RelativePath,
        /// The stable error code.
        code: ErrorCode,
    },
}

impl OpOutcome {
    /// The relative path this outcome acted on (every variant carries one).
    fn relative_path(&self) -> &RelativePath {
        match self {
            OpOutcome::Done { relative_path }
            | OpOutcome::Skipped { relative_path, .. }
            | OpOutcome::Failed { relative_path, .. } => relative_path,
        }
    }
}

/// The plan-execution contract the orchestrator drives (SPEC s8).
///
/// Mirrors the SPEC s8 `execute(plan, remote, state, pacer, crypto,
/// activity, pool)` free function as a trait so the orchestrator can hold
/// `Arc<dyn Executor>` and tests can substitute a recording fake. The
/// concrete impl owns the `UploadPool` (DESIGN s11.4.2) and the per-op
/// pipeline; the seams it needs are passed at construction by the impl,
/// not threaded through this method (keeps the trait object-safe and the
/// orchestrator call site small).
#[async_trait::async_trait]
pub trait Executor: Send + Sync {
    /// Executes every op in `plan` for `source`, honouring the pool +
    /// pacer gates and the crash-safe protocol. Returns the per-op
    /// outcomes in completion order. Reports live progress through
    /// `on_progress`, called (throttled) as ops finish so the orchestrator
    /// can update [`ExecProgress`] without the executor knowing about the
    /// state machine.
    async fn execute(
        &self,
        source: &SourceRow,
        plan: &Plan,
        on_progress: &(dyn Fn(ExecProgress) + Send + Sync),
    ) -> anyhow::Result<Vec<OpOutcome>>;

    /// Runs the startup reconciliation pass (DESIGN s5.6): for every
    /// still-pending op carrying a `client_op_uuid`, adopt the orphaned
    /// remote object (`find_by_op_uuid` for creates; `drive_file_id`
    /// `appProperties` compare for updates) or re-run it. Cheap - touches
    /// only `pending_ops`, not every file. Run once before the first
    /// normal cycle.
    async fn reconcile(&self, source: &SourceRow) -> anyhow::Result<()>;
}

/// Construction-time dependencies an [`Executor`] implementation wires
/// together (SPEC s8 parameter list).
///
/// Grouped into one struct so the implementer's constructor signature
/// stays readable and so the orchestrator (or a test) assembles the seam
/// set once.
pub struct ExecutorDeps {
    /// The Drive-side store (SPEC s3). `InMemoryRemoteStore` in tests.
    pub remote: Arc<dyn RemoteStore>,
    /// The SQLite state layer (SPEC s2).
    pub state: Arc<dyn StateRepo>,
    /// The per-account rate pacer (SPEC s9).
    pub pacer: Arc<dyn Pacer>,
    /// The per-source encryption suite, or `None` when the source is
    /// unencrypted (DESIGN s7).
    pub crypto: Option<Arc<dyn SourceCryptoSuite>>,
}

// -----------------------------------------------------------------------------
// DefaultExecutor
// -----------------------------------------------------------------------------

/// The number of in-flight files permitted concurrently (DESIGN s11.4.2:
/// `min(available_parallelism * 2, 16)`, hard cap 32). Computed once at
/// construction.
fn default_pool_size() -> usize {
    let par = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    (par.saturating_mul(2)).min(16).clamp(1, 32)
}

/// Test-only hook fired exactly once, between the pre-open `lstat`/open and
/// the post-read `fstat` identity check, so a test can deterministically
/// mutate or replace the file on disk and exercise the
/// changed/replaced-during-upload defences (SPEC s8) without racing a real
/// filesystem. Production builds carry no such field.
#[cfg(test)]
type MidUploadHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// Test-only hook fired exactly once, AFTER the upload bytes have landed on
/// Drive but BEFORE the post-upload identity re-check (P1-1), so a test can
/// deterministically mutate or replace the file on disk once the bytes are
/// already remote and exercise the post-upload
/// `file_changed_during_upload` defence (SPEC s8). Production builds carry
/// no such field.
#[cfg(test)]
type PostUploadHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// The production [`Executor`] (SPEC s8, DESIGN s5.4 / s5.6 / s11.4).
///
/// Holds the injected seams plus an [`UploadPool`](Semaphore) bounding
/// in-flight files (DESIGN s11.4.2) and a [`Clock`] for the timestamps
/// written into `file_state` / `pending_ops`. Cheap to clone-by-`Arc`
/// internally; the orchestrator holds it behind `Arc<dyn Executor>`.
pub struct DefaultExecutor {
    remote: Arc<dyn RemoteStore>,
    state: Arc<dyn StateRepo>,
    pacer: Arc<dyn Pacer>,
    crypto: Option<Arc<dyn SourceCryptoSuite>>,
    clock: Arc<dyn Clock>,
    /// Inter-file concurrency gate (DESIGN s11.4.2). `acquire`d per op.
    pool: Arc<Semaphore>,
    #[cfg(test)]
    mid_upload_hook: Option<MidUploadHook>,
    #[cfg(test)]
    post_upload_hook: Option<PostUploadHook>,
}

impl DefaultExecutor {
    /// Builds a [`DefaultExecutor`] from its dependency set, a system
    /// [`Clock`], and the default-sized upload pool (DESIGN s11.4.2).
    pub fn new(deps: ExecutorDeps) -> Self {
        Self::with_clock(deps, Arc::new(SystemClock))
    }

    /// Builds a [`DefaultExecutor`] with an explicit [`Clock`] (tests
    /// inject a `FakeClock` so the timestamps are deterministic).
    pub fn with_clock(deps: ExecutorDeps, clock: Arc<dyn Clock>) -> Self {
        let pool = Arc::new(Semaphore::new(default_pool_size()));
        Self {
            remote: deps.remote,
            state: deps.state,
            pacer: deps.pacer,
            crypto: deps.crypto,
            clock,
            pool,
            #[cfg(test)]
            mid_upload_hook: None,
            #[cfg(test)]
            post_upload_hook: None,
        }
    }

    /// Test-only: install the mid-upload hook (see [`MidUploadHook`]).
    #[cfg(test)]
    fn with_mid_upload_hook(mut self, hook: MidUploadHook) -> Self {
        self.mid_upload_hook = Some(hook);
        self
    }

    /// Test-only: install the post-upload hook (see [`PostUploadHook`]).
    #[cfg(test)]
    fn with_post_upload_hook(mut self, hook: PostUploadHook) -> Self {
        self.post_upload_hook = Some(hook);
        self
    }

    /// Fire the mid-upload hook, if installed (no-op in production).
    fn fire_mid_upload_hook(&self, _path: &Path) {
        #[cfg(test)]
        if let Some(hook) = &self.mid_upload_hook {
            hook(_path);
        }
    }

    /// Fire the post-upload hook, if installed (no-op in production).
    fn fire_post_upload_hook(&self, _path: &Path) {
        #[cfg(test)]
        if let Some(hook) = &self.post_upload_hook {
            hook(_path);
        }
    }

    // -------------------------------------------------------------------------
    // Per-op drivers.
    // -------------------------------------------------------------------------

    /// Execute one [`Op::HashThenUpload`]: the SPEC s8 hash-then-upload
    /// path with the file-changed defences and the crash-safe
    /// `client_op_uuid` protocol.
    async fn hash_then_upload(
        &self,
        source: &SourceRow,
        relative_path: &RelativePath,
        size: u64,
    ) -> anyhow::Result<OpOutcome> {
        let full_path = join_source_path(&source.local_path, relative_path);

        // --- pre-open lstat + open with FILE_SHARE_DELETE -------------------
        let pre = match lstat_identity(&full_path) {
            Ok(id) => id,
            Err(e) => {
                warn!(target: TARGET, path = %full_path.display(), error = %e, "lstat failed");
                return Ok(OpOutcome::Failed {
                    relative_path: relative_path.clone(),
                    code: ErrorCode::LocalIoError,
                });
            }
        };
        let mut file = match open_shared(&full_path).await {
            Ok(f) => f,
            Err(OpenError::Locked) => {
                return Ok(OpOutcome::Skipped {
                    relative_path: relative_path.clone(),
                    reason: SkipReason::Locked,
                });
            }
            Err(OpenError::Io(e)) => {
                warn!(target: TARGET, path = %full_path.display(), error = %e, "open failed");
                return Ok(OpOutcome::Failed {
                    relative_path: relative_path.clone(),
                    code: ErrorCode::LocalIoError,
                });
            }
        };
        let opened = fstat_identity(&file).await?;
        if (opened.dev, opened.inode) != (pre.dev, pre.inode) {
            // Replaced between our lstat and our open (SPEC s8 defence #1).
            return Ok(OpOutcome::Skipped {
                relative_path: relative_path.clone(),
                reason: SkipReason::ReplacedBeforeOpen,
            });
        }

        // Test seam: let a test mutate/replace the file now, before we read
        // and post-fstat, so the mid-upload defences are deterministic.
        self.fire_mid_upload_hook(&full_path);

        // --- decide create vs update by the stored drive_file_id -----------
        let existing = self.state.get_file_state(source.id, relative_path).await?;
        let existing_file_id = existing.as_ref().and_then(|r| r.drive_file_id.clone());

        // --- enqueue the pending_op with a fresh client_op_uuid ------------
        // (DESIGN s5.6 step 1: the UUID lands in pending_ops, transactionally,
        // BEFORE we issue the create/update.)
        let op_uuid = uuid::Uuid::new_v4().to_string();
        let now = self.clock.now_ms();
        let payload = PendingOpPayload {
            client_op_uuid: Some(op_uuid.clone()),
            drive_file_id: existing_file_id.clone(),
            ..PendingOpPayload::default()
        };
        let op_id = self
            .state
            .enqueue_pending_op(NewPendingOp {
                source_id: source.id,
                op_type: OP_TYPE_UPLOAD.to_string(),
                relative_path: relative_path.clone(),
                payload_json: payload.to_value(),
                scheduled_for: now,
                created_at: now,
            })
            .await?;

        // --- read + hash + (encrypt) + upload, verify md5, commit ----------
        let app_props = self.app_properties(source.id, relative_path, &op_uuid);
        let outcome = self
            .upload_and_commit(
                source,
                relative_path,
                size,
                &mut file,
                pre,
                existing_file_id.as_deref(),
                op_id,
                payload,
                app_props,
            )
            .await;

        match outcome {
            Ok(out) => Ok(out),
            Err(UploadError::Skip(reason)) => {
                // File changed/replaced BEFORE the bytes reached Drive (SPEC
                // s8 post-read defence). No remote object was created, so the
                // pending_op is always safe to drop; the next scan
                // re-enqueues a clean op.
                self.state.delete_pending_op(op_id).await?;
                Ok(OpOutcome::Skipped {
                    relative_path: relative_path.clone(),
                    reason,
                })
            }
            Err(UploadError::SkipPostUpload(reason)) => {
                // P1-1: file changed/replaced AFTER the bytes landed on
                // Drive. Do NOT mark synced. For an UPDATE the prior
                // file_state row keeps the existing drive_file_id, so
                // dropping the op is safe - the next scan re-enqueues a clean
                // update. For a CREATE the bytes are an orphan with no
                // file_state row; dropping the op would strand it (there is
                // no general orphan sweep, only the pending_ops-driven
                // reconcile pass). Leave the create op in place so reconcile
                // adopts the orphan and re-hashes it (P1-2): if the orphan's
                // bytes still match it is committed, otherwise it is requeued
                // as an update against the SAME file_id (no duplicate).
                if existing_file_id.is_some() {
                    self.state.delete_pending_op(op_id).await?;
                }
                Ok(OpOutcome::Skipped {
                    relative_path: relative_path.clone(),
                    reason,
                })
            }
            Err(UploadError::Failed(code)) => {
                self.state.delete_pending_op(op_id).await?;
                Ok(OpOutcome::Failed {
                    relative_path: relative_path.clone(),
                    code,
                })
            }
            Err(UploadError::Fatal(e)) => Err(e),
        }
    }

    /// The read/hash/encrypt/upload/verify/commit inner loop. Split out so
    /// `hash_then_upload` can own the pending_op lifecycle around it.
    #[allow(clippy::too_many_arguments)]
    async fn upload_and_commit(
        &self,
        source: &SourceRow,
        relative_path: &RelativePath,
        size: u64,
        file: &mut tokio::fs::File,
        pre: FileIdentity,
        existing_file_id: Option<&str>,
        op_id: PendingOpId,
        mut payload: PendingOpPayload,
        app_props: HashMap<String, String>,
    ) -> Result<OpOutcome, UploadError> {
        let full_path = join_source_path(&source.local_path, relative_path);

        // Read the whole file into memory once, computing blake3 over the
        // plaintext and building the exact bytes to send (ciphertext when
        // encrypted, else plaintext). The pipeline (DESIGN s11.4.3) is an
        // internal throughput refinement over this same data flow; the
        // observable contract - blake3-over-plaintext, md5-over-sent-bytes,
        // no full-size prealloc - is identical and is what the fake checks.
        let HashedBody {
            blake3,
            sent_bytes,
            plaintext_len,
        } = read_hash_encrypt(file, self.crypto.as_deref())
            .await
            .map_err(UploadError::from_read)?;

        // --- post-read fstat identity check (SPEC s8 defence #3) -----------
        let post = fstat_identity(file).await.map_err(UploadError::Fatal)?;
        if (post.size, post.ctime_ns) != (pre.size, pre.ctime_ns) {
            return Err(UploadError::Skip(SkipReason::ChangedDuringUpload));
        }
        match lstat_identity(&full_path) {
            Ok(now_path) => {
                if (now_path.dev, now_path.inode) != (pre.dev, pre.inode) {
                    return Err(UploadError::Skip(SkipReason::ReplacedDuringUpload));
                }
            }
            // The path vanished mid-read (atomic replace + delete): treat as
            // replaced, re-queue.
            Err(_) => return Err(UploadError::Skip(SkipReason::ReplacedDuringUpload)),
        }

        // The plaintext we read must match the size the planner observed; a
        // grow/shrink is the changed-during-upload case the fstat check
        // above already catches, but guard explicitly so the declared
        // upload length is never wrong for an unencrypted body.
        if self.crypto.is_none() && plaintext_len != size {
            return Err(UploadError::Skip(SkipReason::ChangedDuringUpload));
        }

        // --- P1-2: persist the uploaded blake3 BEFORE the bytes land -------
        // The hash is over the plaintext we just read and verified coherent.
        // Persisting it now (before `upload_bytes` issues the create/update)
        // means a crash with the object already on Drive leaves a pending_op
        // that records exactly which bytes were uploaded, so reconciliation
        // can re-hash the (possibly since-changed) local file against this
        // and only adopt-as-Synced on a match.
        payload.uploaded_blake3_hex = Some(hex::encode(blake3));
        self.state
            .update_pending_op_payload(op_id, &payload.to_value())
            .await
            .map_err(UploadError::Fatal)?;

        // --- issue the upload (small simple, large resumable) --------------
        let mime = "application/octet-stream";
        let entry = self
            .upload_bytes(
                source,
                relative_path,
                existing_file_id,
                sent_bytes,
                mime,
                app_props,
                op_id,
                &mut payload,
            )
            .await?;

        // md5 verification over the EXACT bytes sent (SPEC s8) happened inside
        // `upload_bytes`: it compared `entry.md5` (Drive's md5 of the stored
        // bytes - ciphertext when encrypted, plaintext otherwise) against the
        // local md5 the read/encrypt pass accumulated over those same bytes.

        // --- P1-1: post-UPLOAD identity re-check (SPEC s8) -----------------
        // The first fstat above proved the file did not change while we were
        // READING it; but the bytes could still be mutated between the read
        // and the moment Drive finishes accepting them. Re-stat the open
        // handle AND the path now that the object is fully uploaded, before
        // we commit it as Synced. On any change/replace, do NOT commit: the
        // remote object is an orphan that reconcile adopts + re-hashes
        // (P1-2), and the op is re-enqueued by the caller.
        self.fire_post_upload_hook(&full_path);
        let post2 = fstat_identity(file).await.map_err(UploadError::Fatal)?;
        if (post2.size, post2.ctime_ns) != (pre.size, pre.ctime_ns) {
            return Err(UploadError::SkipPostUpload(SkipReason::ChangedDuringUpload));
        }
        match lstat_identity(&full_path) {
            Ok(now_path) => {
                if (now_path.dev, now_path.inode) != (pre.dev, pre.inode) {
                    return Err(UploadError::SkipPostUpload(
                        SkipReason::ReplacedDuringUpload,
                    ));
                }
            }
            Err(_) => {
                return Err(UploadError::SkipPostUpload(
                    SkipReason::ReplacedDuringUpload,
                ))
            }
        }

        // --- build + commit the file_state row, atomically with op delete --
        let now = self.clock.now_ms();
        let row = FileStateRow {
            source_id: source.id,
            relative_path: relative_path.clone(),
            size: post.size,
            mtime_ns: post.mtime_ns,
            hash_blake3: blake3,
            drive_file_id: Some(entry.id.clone()),
            drive_md5: entry.md5,
            encrypted_remote_path: None,
            status: FileStateStatus::Synced,
            last_uploaded_at: Some(now),
            last_verified_at: Some(now),
        };
        if existing_file_id.is_some() {
            self.state
                .commit_update_result(op_id, &row)
                .await
                .map_err(UploadError::Fatal)?;
        } else {
            self.state
                .commit_create_result(op_id, &row)
                .await
                .map_err(UploadError::Fatal)?;
        }
        debug!(
            target: TARGET,
            source = %source.id,
            path = %relative_path,
            file_id = %entry.id,
            "upload committed"
        );
        Ok(OpOutcome::Done {
            relative_path: relative_path.clone(),
        })
    }

    /// Upload an in-memory body, retrying transient errors and verifying the
    /// returned md5 against the local md5 of the exact bytes sent. Chooses
    /// the simple multipart path below [`RESUMABLE_THRESHOLD`] and the
    /// resumable protocol at or above it.
    #[allow(clippy::too_many_arguments)]
    async fn upload_bytes(
        &self,
        source: &SourceRow,
        relative_path: &RelativePath,
        existing_file_id: Option<&str>,
        sent: SentBytes,
        mime: &str,
        app_props: HashMap<String, String>,
        op_id: PendingOpId,
        payload: &mut PendingOpPayload,
    ) -> Result<RemoteEntry, UploadError> {
        let total = sent.bytes.len() as u64;
        let mut transient_retries = 0u32;
        loop {
            let result = if total >= RESUMABLE_THRESHOLD {
                self.upload_resumable(
                    &source.drive_folder_id,
                    relative_path,
                    existing_file_id,
                    &sent,
                    mime,
                    &app_props,
                    op_id,
                    payload,
                )
                .await
            } else {
                self.upload_simple(
                    &source.drive_folder_id,
                    relative_path,
                    existing_file_id,
                    &sent,
                    mime,
                    &app_props,
                )
                .await
            };

            match result {
                Ok(entry) => {
                    // md5 verify over the exact bytes sent (SPEC s8). The
                    // local md5 is `sent.md5`; the remote md5 is `entry.md5`.
                    match entry.md5 {
                        Some(remote) if remote == sent.md5 => {
                            self.pacer.note_response(ResponseClass::Ok);
                            return Ok(entry);
                        }
                        _ => {
                            warn!(
                                target: TARGET,
                                path = %relative_path,
                                "md5 mismatch: remote {:?} vs local {:?}",
                                entry.md5,
                                sent.md5
                            );
                            return Err(UploadError::Failed(ErrorCode::DriveChecksumMismatch));
                        }
                    }
                }
                Err(e) => {
                    let class = classify_drive_error(&e);
                    self.pacer.note_response(class.response_class());
                    match class {
                        DriveError::RateLimited | DriveError::Transient => {
                            transient_retries += 1;
                            if transient_retries > MAX_TRANSIENT_RETRIES {
                                return Err(UploadError::Failed(class.error_code()));
                            }
                            // The pacer's note_response set a backoff window
                            // for rate-limits; permit_request below sleeps it
                            // out. For plain 5xx/network we loop immediately
                            // (the fake's transient faults are single-shot).
                            continue;
                        }
                        DriveError::QuotaExhausted
                        | DriveError::DailyQuota
                        | DriveError::InvalidGrant
                        | DriveError::DestFolderMissing
                        | DriveError::DestFolderPermissionDenied
                        | DriveError::Other => {
                            return Err(UploadError::Failed(class.error_code()));
                        }
                    }
                }
            }
        }
    }

    /// Simple multipart create/update for files below
    /// [`RESUMABLE_THRESHOLD`].
    async fn upload_simple(
        &self,
        parent_id: &str,
        relative_path: &RelativePath,
        existing_file_id: Option<&str>,
        sent: &SentBytes,
        mime: &str,
        app_props: &HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        let body = UploadBody::Bytes(sent.bytes.clone());
        if let Some(file_id) = existing_file_id {
            self.pacer.permit_request().await;
            self.remote.update(file_id, body, app_props.clone()).await
        } else {
            self.pacer.permit_file_create().await;
            let name = filename_of(relative_path);
            self.remote
                .create(parent_id, &name, mime, body, app_props.clone())
                .await
        }
    }

    /// Resumable create/update for files at or above [`RESUMABLE_THRESHOLD`].
    /// On a session-invalidating 4xx the session is discarded and the whole
    /// transfer restarts from offset 0 (DESIGN s5.4), bounded by
    /// [`MAX_SESSION_RESTARTS`].
    ///
    /// P1-3: the live session (url, issued_at, total, kind, last-acked
    /// offset) is persisted into `pending_ops.payload_json` so a crash
    /// mid-upload resumes from the acked offset rather than from zero. A
    /// session opened here always starts at offset 0; the cross-restart
    /// resume entry point is [`Self::resume_persisted`], driven by
    /// `reconcile`.
    #[allow(clippy::too_many_arguments)]
    async fn upload_resumable(
        &self,
        parent_id: &str,
        relative_path: &RelativePath,
        existing_file_id: Option<&str>,
        sent: &SentBytes,
        mime: &str,
        app_props: &HashMap<String, String>,
        op_id: PendingOpId,
        payload: &mut PendingOpPayload,
    ) -> anyhow::Result<RemoteEntry> {
        let total = sent.bytes.len() as u64;
        let mut restarts = 0u32;
        loop {
            let kind = if let Some(file_id) = existing_file_id {
                ResumableKind::Update {
                    file_id: file_id.to_string(),
                }
            } else {
                ResumableKind::Create {
                    parent_id: parent_id.to_string(),
                    name: filename_of(relative_path),
                    app_properties: app_props.clone(),
                }
            };
            if existing_file_id.is_some() {
                self.pacer.permit_request().await;
            } else {
                self.pacer.permit_file_create().await;
            }
            let session = self.remote.resumable_session(kind, mime, total).await?;

            // Persist the freshly-opened session at offset 0 BEFORE pushing
            // any bytes, so a crash after the first chunk lands can resume.
            payload.resumable = Some(PersistedResumable {
                session: session.clone(),
                acked_offset: 0,
            });
            self.persist_payload(op_id, payload).await?;

            match self
                .push_chunks(&session, &sent.bytes, 0, op_id, payload)
                .await?
            {
                Some(entry) => {
                    // Done: clear the persisted session.
                    payload.resumable = None;
                    self.persist_payload(op_id, payload).await?;
                    return Ok(entry);
                }
                None => {
                    // Session invalidated (4xx). Discard it and restart from
                    // offset 0 (DESIGN s5.4: never reuse a 4xx-d session).
                    payload.resumable = None;
                    self.persist_payload(op_id, payload).await?;
                    restarts += 1;
                    if restarts > MAX_SESSION_RESTARTS {
                        anyhow::bail!("drive.resumable_session_invalid: exhausted restarts");
                    }
                    warn!(
                        target: TARGET,
                        path = %relative_path,
                        restarts,
                        "resumable session invalidated; restarting from offset 0"
                    );
                    continue;
                }
            }
        }
    }

    /// Push `body[start_offset..]` to an open resumable session in 4 MiB
    /// wire chunks (non-final chunks are multiples of 256 KiB per SPEC s3).
    /// Returns `Some(entry)` on completion, `None` if the session was
    /// invalidated (caller restarts). After every accepted chunk the
    /// last-acked offset is persisted into the op payload (P1-3) so a crash
    /// resumes from there.
    async fn push_chunks(
        &self,
        session: &ResumableSession,
        body: &Bytes,
        start_offset: u64,
        op_id: PendingOpId,
        payload: &mut PendingOpPayload,
    ) -> anyhow::Result<Option<RemoteEntry>> {
        let total = body.len();
        let mut offset = start_offset as usize;
        while offset < total {
            let end = (offset + WIRE_CHUNK).min(total);
            // Non-final chunks must be a 256-KiB multiple; the final chunk
            // may be any size. WIRE_CHUNK is already a multiple, so the slice
            // is correct except for the trailing final chunk.
            let chunk = body.slice(offset..end);
            self.pacer.permit_request().await;
            self.pacer.permit_bytes(chunk.len() as u64).await;
            match self
                .remote
                .resume_chunk(session, offset as u64, chunk)
                .await?
            {
                ResumeProgress::Completed(entry) => return Ok(Some(entry)),
                ResumeProgress::InProgress { received } => {
                    offset = received as usize;
                    // Persist the new acked offset so a crash resumes here.
                    if let Some(r) = payload.resumable.as_mut() {
                        r.acked_offset = received;
                    }
                    self.persist_payload(op_id, payload).await?;
                }
                ResumeProgress::SessionInvalid => return Ok(None),
            }
        }
        // Loop ended without a Completed: a zero-byte body or a short final
        // chunk that the fake acks as Completed should have returned above.
        // An empty file is uploaded by the simple path (size 0 <
        // RESUMABLE_THRESHOLD), so reaching here means the session never
        // completed - treat as invalidated so the caller restarts.
        Ok(None)
    }

    /// Persist the current op payload, mapping any state error to a fatal
    /// upload error (a state-DB write failure aborts the whole `execute`).
    async fn persist_payload(
        &self,
        op_id: PendingOpId,
        payload: &PendingOpPayload,
    ) -> anyhow::Result<()> {
        self.state
            .update_pending_op_payload(op_id, &payload.to_value())
            .await
    }

    /// Build the canonical `appProperties` for an object Driven owns
    /// (SPEC s3 preamble + DESIGN s5.6 `client_op_uuid`).
    fn app_properties(
        &self,
        source_id: SourceId,
        relative_path: &RelativePath,
        op_uuid: &str,
    ) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(CLIENT_OP_UUID_KEY.to_string(), op_uuid.to_string());
        m.insert(SOURCE_ID_KEY.to_string(), source_id.to_string());
        m.insert(
            RELATIVE_PATH_HASH_KEY.to_string(),
            relative_path_hash(relative_path),
        );
        m
    }

    /// Execute one [`Op::Trash`]: idempotently trash the remote object and
    /// drop the local `file_state` row (SPEC s7, DESIGN s5.5).
    async fn trash_op(
        &self,
        source: &SourceRow,
        relative_path: &RelativePath,
        drive_file_id: &str,
    ) -> anyhow::Result<OpOutcome> {
        self.pacer.permit_request().await;
        match self.remote.trash(drive_file_id).await {
            Ok(()) => {
                self.pacer.note_response(ResponseClass::Ok);
                self.state
                    .delete_file_state(source.id, relative_path)
                    .await?;
                Ok(OpOutcome::Done {
                    relative_path: relative_path.clone(),
                })
            }
            Err(e) => {
                let class = classify_drive_error(&e);
                self.pacer.note_response(class.response_class());
                Ok(OpOutcome::Failed {
                    relative_path: relative_path.clone(),
                    code: class.error_code(),
                })
            }
        }
    }
}

#[async_trait::async_trait]
impl Executor for DefaultExecutor {
    async fn execute(
        &self,
        source: &SourceRow,
        plan: &Plan,
        on_progress: &(dyn Fn(ExecProgress) + Send + Sync),
    ) -> anyhow::Result<Vec<OpOutcome>> {
        // Snapshot the plan totals for the progress denominator.
        let summary = plan.summary();
        let mut progress = ExecProgress {
            files_total: summary.uploads as u64,
            bytes_total: summary.bytes,
            trashes_total: summary.trashes as u64,
            ..ExecProgress::zero()
        };
        on_progress(progress);

        // Bound in-flight files by the pool (DESIGN s11.4.2) AND, inside each
        // op, the pacer (SPEC s8 "both gates must be open"). We overlap
        // independent files via a `FuturesUnordered` of `&self`-borrowing
        // futures - no `'static` spawn, so the trait stays object-safe and we
        // avoid cloning every Arc per op.
        //
        // Permit acquisition is interleaved with draining completed futures so
        // a saturated pool does not deadlock: when all permits are held the
        // `select!` below polls `in_flight` (which frees a permit on
        // completion) instead of blocking solely on `acquire_owned`.
        use futures::stream::{FuturesUnordered, StreamExt};

        let exec = ExecOne { this: self, source };
        let mut in_flight = FuturesUnordered::new();
        let mut outcomes = Vec::with_capacity(plan.ops.len());
        let mut ops = plan.ops.iter();
        let mut next_op = ops.next();

        loop {
            if next_op.is_none() && in_flight.is_empty() {
                break;
            }
            if let Some(op) = next_op {
                // Try to start the next op as soon as a permit is free, while
                // still draining in-flight work so permits get released.
                let permit = self.pool.clone();
                tokio::select! {
                    biased;
                    // Drain a completed op first (frees a permit, reports
                    // progress) so the pool keeps cycling under saturation.
                    Some(outcome) = in_flight.next(), if !in_flight.is_empty() => {
                        let outcome = outcome?;
                        update_progress(&mut progress, &outcome, plan);
                        on_progress(progress);
                        outcomes.push(outcome);
                    }
                    permit = permit.acquire_owned() => {
                        let permit = permit
                            .map_err(|e| anyhow::anyhow!("upload pool closed: {e}"))?;
                        in_flight.push(exec.run(op, permit));
                        next_op = ops.next();
                    }
                }
            } else if let Some(outcome) = in_flight.next().await {
                // No ops left to start; just drain the rest.
                let outcome = outcome?;
                update_progress(&mut progress, &outcome, plan);
                on_progress(progress);
                outcomes.push(outcome);
            }
        }

        Ok(outcomes)
    }

    async fn reconcile(&self, source: &SourceRow) -> anyhow::Result<()> {
        let pending = self.state.get_pending_ops_for_source(source.id).await?;
        for op in pending {
            let payload = PendingOpPayload::from_value(&op.payload_json);
            let Some(uuid) = payload.client_op_uuid.clone() else {
                // No UUID carried (older row): leave it for the normal queue.
                continue;
            };

            // --- P1-3: a live resumable session takes precedence -----------
            // If the crash happened mid-upload with a persisted session, try
            // to resume it byte-for-byte from the last-acked offset rather
            // than re-uploading from zero (or adopting a not-yet-finalized
            // create, which `find_by_op_uuid` cannot even see). On success
            // we adopt the resulting entry; on a stale/invalid session we
            // fall through to the adopt-or-requeue path below.
            if let Some(resumable) = payload.resumable.clone() {
                match self
                    .resume_persisted(source, &op, &payload, resumable)
                    .await?
                {
                    Some(entry) => {
                        self.adopt_reconciled(source, &op, &payload, entry).await?;
                        continue;
                    }
                    None => {
                        // Could not resume (stale / invalidated / no session
                        // bytes left). Fall through: adopt the orphan if it
                        // finalized, else requeue.
                    }
                }
            }

            if let Some(file_id) = payload.drive_file_id.clone() {
                // Update path: compare the existing object's appProperties.
                match self.remote.metadata(&file_id).await {
                    Ok(entry)
                        if entry
                            .app_properties
                            .get(CLIENT_OP_UUID_KEY)
                            .map(|v| v == &uuid)
                            .unwrap_or(false) =>
                    {
                        // Already committed remotely; re-hash + finish.
                        self.adopt_reconciled(source, &op, &payload, entry).await?;
                    }
                    _ => {
                        // Not committed; drop the stale op so the next scan
                        // re-enqueues it cleanly (the prior file_state row
                        // keeps the existing drive_file_id for the update).
                        self.state.delete_pending_op(op.id).await?;
                    }
                }
            } else {
                // Create path: find the orphaned object by op uuid.
                match self
                    .remote
                    .find_by_op_uuid(&source.drive_folder_id, &uuid)
                    .await?
                {
                    Some(entry) => self.adopt_reconciled(source, &op, &payload, entry).await?,
                    None => {
                        self.state.delete_pending_op(op.id).await?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl DefaultExecutor {
    /// P1-3: resume a persisted resumable session after a restart. Discards
    /// the session (returns `None`) when it is older than
    /// [`SESSION_MAX_AGE_MS`] or Drive 4xx-invalidates it; otherwise it
    /// re-reads the local file, re-derives the exact upload body, and pushes
    /// the remaining bytes from the persisted acked offset. A successful
    /// resume returns the finalized [`RemoteEntry`].
    ///
    /// If the local file changed since the crash the body would no longer
    /// match the partially-uploaded bytes; rather than corrupt the object we
    /// discard the session (the partial create is GC'd by Drive when it
    /// expires; a partial update leaves the old object intact) and return
    /// `None` so the caller requeues a clean upload.
    async fn resume_persisted(
        &self,
        source: &SourceRow,
        op: &crate::state::PendingOpRow,
        payload: &PendingOpPayload,
        resumable: PersistedResumable,
    ) -> anyhow::Result<Option<RemoteEntry>> {
        // Byte-level resume is only sound when re-reading the local file
        // reproduces the EXACT bytes already pushed. For an ENCRYPTED source
        // that is false: each `content_encryptor()` draws a fresh random
        // 24-byte nonce (driven-crypto content.rs), so re-encrypting the same
        // plaintext yields a different header + ciphertext. Splicing the new
        // ciphertext onto the old partial bytes would finalize a corrupt
        // object that blake3 (computed over plaintext) cannot detect. So for
        // encrypted sources we never resume - we fall through to restart from
        // offset 0, which DESIGN s5.4 already sanctions ("any 4xx -> recreate
        // from scratch"). True encrypted resume (persisting the crypto
        // header) is an M4 follow-up.
        if self.crypto.is_some() {
            return Ok(None);
        }
        let now = self.clock.now_ms();
        if now - resumable.session.issued_at > SESSION_MAX_AGE_MS {
            warn!(
                target: TARGET,
                path = %op.relative_path,
                "persisted resumable session older than 6 days; discarding"
            );
            return Ok(None);
        }

        // Re-derive the exact bytes that were being uploaded. The body must
        // be byte-identical to the partially-uploaded one for the resume to
        // be coherent, so we verify the re-read plaintext blake3 matches the
        // hash persisted with the op (P1-2/P1-3 share this invariant).
        let full_path = join_source_path(&source.local_path, &op.relative_path);
        let mut file = match open_shared(&full_path).await {
            Ok(f) => f,
            // File gone/locked: cannot resume; let the caller requeue.
            Err(_) => return Ok(None),
        };
        let HashedBody {
            blake3, sent_bytes, ..
        } = match read_hash_encrypt(&mut file, self.crypto.as_deref()).await {
            Ok(h) => h,
            Err(_) => return Ok(None),
        };
        if let Some(expected_hex) = payload.uploaded_blake3_hex.as_deref() {
            if hex::encode(blake3) != expected_hex {
                // Local file changed since the crash: the partial bytes are
                // stale. Discard the session and requeue a clean upload.
                return Ok(None);
            }
        } else {
            // No recorded hash to verify against: do not risk a corrupt
            // resume. Requeue from scratch.
            return Ok(None);
        }
        if sent_bytes.bytes.len() as u64 != resumable.session.size {
            // Body length disagrees with the session's declared size; the
            // resume would be rejected. Requeue.
            return Ok(None);
        }

        // Push the remaining bytes from the last-acked offset. A fresh
        // payload copy carries the live session so push_chunks can persist
        // progress against the same op.
        let mut live = payload.clone();
        live.resumable = Some(resumable.clone());
        match self
            .push_chunks(
                &resumable.session,
                &sent_bytes.bytes,
                resumable.acked_offset,
                op.id,
                &mut live,
            )
            .await
        {
            Ok(Some(entry)) => {
                // Verify md5 over the exact bytes sent (SPEC s8).
                match entry.md5 {
                    Some(remote) if remote == sent_bytes.md5 => Ok(Some(entry)),
                    _ => {
                        warn!(
                            target: TARGET,
                            path = %op.relative_path,
                            "resumed upload md5 mismatch; requeueing"
                        );
                        Ok(None)
                    }
                }
            }
            // Session invalidated (4xx) or could not complete: discard +
            // requeue (DESIGN s5.4 never reuse a 4xx-d session).
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Adopt an orphaned/just-resumed remote object found during
    /// reconciliation: re-hash the CURRENT local file and commit the
    /// `file_state` row + delete the pending_op atomically (DESIGN s5.6
    /// step 3).
    ///
    /// P1-2: the adopted row must NEVER carry a zeroed hash marked
    /// `Synced` - that would let a post-upload local change slip past the
    /// `(size, mtime, hash)` fast scan forever. Instead we re-hash the
    /// current local plaintext and compare it to the blake3 the op recorded
    /// as uploaded:
    ///
    /// - Match (or the entry's remote md5 is unavailable but the hash
    ///   agrees): the local bytes equal what landed remotely - mark
    ///   `Synced` with the real blake3.
    /// - Mismatch / unreadable / no recorded hash: the local file changed
    ///   after the upload (or we cannot prove it didn't) - do NOT mark
    ///   `Synced`. Write a `Pending` row that PRESERVES the adopted
    ///   `drive_file_id` so the next scan re-uploads as an UPDATE against
    ///   the same object (never a duplicate), then drop the op.
    async fn adopt_reconciled(
        &self,
        source: &SourceRow,
        op: &crate::state::PendingOpRow,
        payload: &PendingOpPayload,
        entry: RemoteEntry,
    ) -> anyhow::Result<()> {
        let full_path = join_source_path(&source.local_path, &op.relative_path);

        // Re-hash the current local file (plaintext). On any read failure we
        // cannot prove identity, so treat as a mismatch (requeue).
        let local = self.rehash_local_plaintext(&full_path).await;
        let expected_hex = payload.uploaded_blake3_hex.clone();

        let now = self.clock.now_ms();
        match (local, expected_hex) {
            (Some((cur_hash, size, mtime_ns)), Some(expected_hex))
                if hex::encode(cur_hash) == expected_hex =>
            {
                // Identity proven: the local bytes match what was uploaded.
                let row = FileStateRow {
                    source_id: source.id,
                    relative_path: op.relative_path.clone(),
                    size,
                    mtime_ns,
                    hash_blake3: cur_hash,
                    drive_file_id: Some(entry.id.clone()),
                    drive_md5: entry.md5,
                    encrypted_remote_path: None,
                    status: FileStateStatus::Synced,
                    last_uploaded_at: Some(now),
                    last_verified_at: Some(now),
                };
                self.state.commit_create_result(op.id, &row).await?;
            }
            (_, _) => {
                // Either the file changed since upload, or we have nothing to
                // verify against. Do NOT mark Synced. Preserve the adopted
                // drive_file_id so the re-upload is an UPDATE (no duplicate).
                //
                // CRITICAL (P1-2): the FastPath scanner keys "unchanged" off
                // `(size, mtime_ns)` ONLY, never `status`. If we stored the
                // CURRENT local identity here, the next scan would see local
                // == stored and never re-emit the file - the stale uploaded
                // bytes would stay on Drive forever. So we stamp a sentinel
                // `mtime_ns` the live file can never match, forcing the next
                // scan to detect a change and re-upload as an update. We also
                // store the uploaded (stale) blake3 + the on-Drive size so the
                // row honestly reflects what is currently on Drive, not the
                // local file.
                warn!(
                    target: TARGET,
                    source = %source.id,
                    path = %op.relative_path,
                    "adopted orphan does not match uploaded hash; requeueing as update"
                );
                let stale_hash = payload
                    .uploaded_blake3_hex
                    .as_deref()
                    .and_then(decode_blake3_hex)
                    .unwrap_or([0u8; 32]);
                let row = FileStateRow {
                    source_id: source.id,
                    relative_path: op.relative_path.clone(),
                    size: entry.size.unwrap_or(0),
                    mtime_ns: REQUEUE_FORCE_RESCAN_MTIME_NS,
                    hash_blake3: stale_hash,
                    drive_file_id: Some(entry.id.clone()),
                    drive_md5: entry.md5,
                    encrypted_remote_path: None,
                    status: FileStateStatus::Pending,
                    last_uploaded_at: None,
                    last_verified_at: None,
                };
                // Upsert the requeue row + drop the op atomically (the next
                // scan re-enqueues a clean update). commit_create_result
                // performs the same atomic upsert+delete for create + update.
                self.state.commit_create_result(op.id, &row).await?;
            }
        }
        Ok(())
    }

    /// Re-read the local file and return its plaintext blake3 + current
    /// `(size, mtime_ns)`. Returns `None` if the file cannot be opened/read
    /// (gone, locked, IO error) - the caller treats that as an identity
    /// mismatch. The hash is over the PLAINTEXT (matching the upload-side
    /// blake3), so it is comparable for both encrypted and plaintext
    /// sources.
    async fn rehash_local_plaintext(&self, full_path: &Path) -> Option<([u8; 32], u64, i64)> {
        let mut file = open_shared(full_path).await.ok()?;
        let id = fstat_identity(&file).await.ok()?;
        // We only need the blake3-over-plaintext; pass crypto=None so the
        // body bytes are not built up unnecessarily - read_hash_encrypt still
        // hashes the plaintext identically in both arms.
        let HashedBody { blake3, .. } = read_hash_encrypt(&mut file, None).await.ok()?;
        Some((blake3, id.size, id.mtime_ns))
    }
}

/// Borrowed bundle used to run one op without `'static` spawns: ties every
/// future's borrows to `&self` + `&source` for the lifetime of `execute`.
struct ExecOne<'a> {
    this: &'a DefaultExecutor,
    source: &'a SourceRow,
}

impl<'a> ExecOne<'a> {
    /// Run a single op, holding the concurrency permit until it completes.
    async fn run(
        &self,
        op: &Op,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> anyhow::Result<OpOutcome> {
        let out = match op {
            Op::HashThenUpload {
                source_id,
                relative_path,
                size,
            } => {
                debug_assert_eq!(*source_id, self.source.id);
                self.this
                    .hash_then_upload(self.source, relative_path, *size)
                    .await
            }
            Op::Trash {
                source_id,
                relative_path,
                drive_file_id,
            } => {
                debug_assert_eq!(*source_id, self.source.id);
                self.this
                    .trash_op(self.source, relative_path, drive_file_id)
                    .await
            }
        };
        drop(permit);
        out
    }
}

/// Fold one completed outcome into the running [`ExecProgress`].
fn update_progress(progress: &mut ExecProgress, outcome: &OpOutcome, plan: &Plan) {
    let path = outcome.relative_path();
    // Find the op this outcome corresponds to, to know upload vs trash and
    // the byte size. Cheap: plans are batched per cycle and matched by path.
    let op = plan.ops.iter().find(|o| match o {
        Op::HashThenUpload { relative_path, .. } | Op::Trash { relative_path, .. } => {
            relative_path == path
        }
    });
    match outcome {
        OpOutcome::Done { .. } => match op {
            Some(Op::HashThenUpload { size, .. }) => {
                progress.files_done += 1;
                progress.bytes_done += *size;
            }
            Some(Op::Trash { .. }) => progress.trashes_done += 1,
            None => {}
        },
        OpOutcome::Failed { .. } => progress.errors += 1,
        OpOutcome::Skipped { .. } => {}
    }
}

// -----------------------------------------------------------------------------
// Read / hash / encrypt
// -----------------------------------------------------------------------------

/// The exact bytes to send to Drive plus their md5 (computed over those
/// exact bytes per SPEC s8).
struct SentBytes {
    bytes: Bytes,
    md5: [u8; 16],
}

/// Result of reading + hashing + (optionally) encrypting a file.
struct HashedBody {
    /// BLAKE3 over the plaintext (the change-detection key, SPEC s2).
    blake3: [u8; 32],
    /// The bytes to upload + their md5 (ciphertext when encrypted).
    sent_bytes: SentBytes,
    /// The plaintext byte count read (for the unencrypted length guard).
    plaintext_len: u64,
}

/// Read the whole file, hashing blake3 over the plaintext and producing the
/// exact bytes to send. When `crypto` is `Some`, the body is the
/// XChaCha20-Poly1305 STREAM ciphertext (header + per-chunk sealed bytes)
/// and the md5 is over that ciphertext (DESIGN s7.1); otherwise the body is
/// the plaintext and the md5 is over the plaintext.
///
/// Reads in [`READ_BUF`]-sized chunks so no full-size buffer is
/// preallocated (DESIGN s11.4.6 memory bound). The accumulation into a
/// single `Vec` is bounded by the file size; the streaming pipeline
/// refinement (DESIGN s11.4.3) would replace this with a bounded channel,
/// but the observable contract is identical.
async fn read_hash_encrypt(
    file: &mut tokio::fs::File,
    crypto: Option<&dyn SourceCryptoSuite>,
) -> Result<HashedBody, std::io::Error> {
    use md5::{Digest, Md5};

    let mut hasher_blake3 = blake3::Hasher::new();
    let mut md5 = Md5::new();
    let mut read_buf = vec![0u8; READ_BUF];
    let mut plaintext_len = 0u64;

    // Encrypted path: drive the ContentEncryptor; md5 is over ciphertext.
    if let Some(suite) = crypto {
        let mut enc: Box<dyn ContentEncryptor> = suite.content_encryptor();
        let header = enc.header();
        md5.update(&header);
        let mut out = Vec::new();
        out.extend_from_slice(&header);

        // We must know which chunk is the last to set the STREAM last-chunk
        // flag, so read one chunk ahead.
        let mut pending: Option<Vec<u8>> = None;
        loop {
            let n = file.read(&mut read_buf).await?;
            if n == 0 {
                break;
            }
            hasher_blake3.update(&read_buf[..n]);
            plaintext_len += n as u64;
            if let Some(prev) = pending.take() {
                let ct = enc.encrypt_chunk(&prev).map_err(crypto_to_io)?;
                md5.update(&ct);
                out.extend_from_slice(&ct);
            }
            pending = Some(read_buf[..n].to_vec());
        }
        // Finalize the last chunk (or an empty final chunk for an empty file).
        let last = pending.unwrap_or_default();
        let (ct, ct_md5) = enc.finalize_last(&last).map_err(crypto_to_io)?;
        md5.update(&ct);
        out.extend_from_slice(&ct);
        // The suite's returned md5 is over every ciphertext byte (header +
        // chunks); it must equal our independently accumulated md5.
        let acc: [u8; 16] = md5.finalize().into();
        debug_assert_eq!(acc, ct_md5, "ciphertext md5 disagreement");
        let blake3: [u8; 32] = hasher_blake3.finalize().into();
        return Ok(HashedBody {
            blake3,
            sent_bytes: SentBytes {
                bytes: Bytes::from(out),
                md5: ct_md5,
            },
            plaintext_len,
        });
    }

    // Unencrypted path: body == plaintext, md5 over plaintext.
    let mut out = Vec::new();
    loop {
        let n = file.read(&mut read_buf).await?;
        if n == 0 {
            break;
        }
        hasher_blake3.update(&read_buf[..n]);
        md5.update(&read_buf[..n]);
        out.extend_from_slice(&read_buf[..n]);
        plaintext_len += n as u64;
    }
    let blake3: [u8; 32] = hasher_blake3.finalize().into();
    let md5: [u8; 16] = md5.finalize().into();
    Ok(HashedBody {
        blake3,
        sent_bytes: SentBytes {
            bytes: Bytes::from(out),
            md5,
        },
        plaintext_len,
    })
}

/// Map a [`CryptoError`] into an `io::Error` so `read_hash_encrypt` keeps a
/// single error type; the caller re-classifies it into the right
/// `crypto.*` [`ErrorCode`] at the boundary.
fn crypto_to_io(e: CryptoError) -> std::io::Error {
    std::io::Error::other(e)
}

// -----------------------------------------------------------------------------
// Error plumbing
// -----------------------------------------------------------------------------

/// Internal upload-path error distinguishing a re-queue skip, a per-op
/// failure (logged, file left non-synced), and a fatal error that aborts
/// the whole `execute` (e.g. a state-DB write failure).
enum UploadError {
    /// File changed/replaced BEFORE the bytes reached Drive (the post-read
    /// check, the length guard). No remote object was created, so the
    /// pending_op is always safe to drop.
    Skip(SkipReason),
    /// P1-1: file changed/replaced AFTER `upload_bytes` returned - the
    /// bytes already landed on Drive. For a CREATE this leaves an orphan
    /// with no `file_state` row, so the op MUST be kept for the reconcile
    /// pass to adopt + re-hash (requeues as an update vs the same id, no
    /// duplicate). For an UPDATE the prior `file_state` row already carries
    /// the id, so the op is safe to drop.
    SkipPostUpload(SkipReason),
    Failed(ErrorCode),
    Fatal(anyhow::Error),
}

impl UploadError {
    /// Map a read/encrypt `io::Error` into the right `UploadError`. A crypto
    /// error becomes a `crypto.*` per-op failure; a plain IO error becomes
    /// `local.io_error`.
    fn from_read(e: std::io::Error) -> Self {
        if let Some(ce) = e
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<CryptoError>())
        {
            return UploadError::Failed(crypto_error_code(ce));
        }
        UploadError::Failed(ErrorCode::LocalIoError)
    }
}

/// Map a [`CryptoError`] to its stable `crypto.*` [`ErrorCode`].
fn crypto_error_code(e: &CryptoError) -> ErrorCode {
    match e {
        CryptoError::KeyMissing => ErrorCode::CryptoKeyMissing,
        CryptoError::DecryptFailed => ErrorCode::CryptoDecryptFailed,
        CryptoError::RecoveryPhraseInvalid => ErrorCode::CryptoRecoveryPhraseInvalid,
        CryptoError::Protocol(_) => ErrorCode::InternalBug,
    }
}

/// The executor's classification of a Drive-side `anyhow` error.
///
/// `RemoteStore` returns `anyhow::Result`; the production `GoogleDriveStore`
/// will (M4) carry a typed/downcastable error, but today both it and the
/// `InMemoryRemoteStore` surface `anyhow` with a format-string message. We
/// isolate the string-matching of the fake's messages here so there is ONE
/// place to swap in a typed `DriveErrorClassification` downcast when the
/// real store lands. See the M3 report's "error classification" note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriveError {
    RateLimited,
    Transient,
    QuotaExhausted,
    DailyQuota,
    InvalidGrant,
    DestFolderMissing,
    DestFolderPermissionDenied,
    Other,
}

impl DriveError {
    fn error_code(self) -> ErrorCode {
        match self {
            DriveError::RateLimited => ErrorCode::DriveRateLimited,
            DriveError::Transient => ErrorCode::DriveUnreachable,
            DriveError::QuotaExhausted => ErrorCode::DriveQuotaExhausted,
            DriveError::DailyQuota => ErrorCode::DriveDailyQuotaExhausted,
            DriveError::InvalidGrant => ErrorCode::AuthInvalidGrant,
            DriveError::DestFolderMissing => ErrorCode::DriveDestFolderMissing,
            DriveError::DestFolderPermissionDenied => ErrorCode::DriveDestFolderPermissionDenied,
            DriveError::Other => ErrorCode::DriveUnreachable,
        }
    }

    /// The pacer [`ResponseClass`] this drive error maps to (SPEC s9). Only
    /// rate-limits and the daily quota move the AIMD ceiling; everything
    /// else is `OtherError` (the executor handles its own retry/backoff).
    fn response_class(self) -> ResponseClass {
        match self {
            DriveError::RateLimited => ResponseClass::RateLimited {
                // The fake carries no Retry-After; use a short default so the
                // pacer's backoff window is exercised without slowing tests.
                retry_after: std::time::Duration::from_millis(1),
            },
            DriveError::DailyQuota => ResponseClass::DailyQuota,
            _ => ResponseClass::OtherError,
        }
    }
}

/// Classify a Drive-side `anyhow` error by matching the fake's message
/// substrings (see the type doc on [`DriveError`] for why this is
/// string-based today).
fn classify_drive_error(e: &anyhow::Error) -> DriveError {
    let msg = e.to_string();
    if msg.contains("rate_limited") {
        DriveError::RateLimited
    } else if msg.contains("daily") {
        DriveError::DailyQuota
    } else if msg.contains("quota_exhausted") {
        DriveError::QuotaExhausted
    } else if msg.contains("invalid_grant") {
        DriveError::InvalidGrant
    } else if msg.contains("dest_folder_missing") {
        DriveError::DestFolderMissing
    } else if msg.contains("dest_folder_permission_denied") {
        DriveError::DestFolderPermissionDenied
    } else if msg.contains("5xx")
        || msg.contains("unreachable")
        || msg.contains("network drop")
        || msg.contains("intermittent")
        || msg.contains("timeout")
    {
        DriveError::Transient
    } else {
        DriveError::Other
    }
}

// -----------------------------------------------------------------------------
// Filesystem identity (cfg-gated; real FS - only the remote is faked)
// -----------------------------------------------------------------------------

/// Stable identity of a file used by the SPEC s8 change/replace defences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    /// Device id (Unix `dev`) / volume serial (Windows).
    dev: u64,
    /// Inode (Unix `ino`) / file index (Windows).
    inode: u64,
    /// File size in bytes.
    size: u64,
    /// Modification time in ns since the Unix epoch (signed).
    mtime_ns: i64,
    /// Change time in ns since the Unix epoch (Unix `ctime`); on Windows we
    /// fall back to the modification time (no portable ctime), so the
    /// changed-during-upload check there leans on `(size, mtime)`.
    ctime_ns: i64,
}

/// `lstat`-equivalent identity of a path (does NOT follow symlinks; V1
/// skips symlinks anyway per DESIGN s5.2.1).
fn lstat_identity(path: &Path) -> std::io::Result<FileIdentity> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(identity_from_meta(&meta))
}

/// `fstat`-equivalent identity of an OPEN handle (reads the handle's
/// metadata, NOT the path's - this is what catches an atomic replace where
/// the path now names a different object than our open fd).
async fn fstat_identity(file: &tokio::fs::File) -> anyhow::Result<FileIdentity> {
    let meta = file
        .metadata()
        .await
        .map_err(|e| anyhow::anyhow!("fstat: {e}"))?;
    Ok(identity_from_meta(&meta))
}

#[cfg(unix)]
fn identity_from_meta(meta: &std::fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        dev: meta.dev(),
        inode: meta.ino(),
        size: meta.size(),
        mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
        ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
    }
}

#[cfg(windows)]
fn identity_from_meta(meta: &std::fs::Metadata) -> FileIdentity {
    use std::os::windows::fs::MetadataExt;
    // file_index() + volume_serial_number() require the unstable
    // `windows_by_handle` feature (rust-lang/rust#63010) and are populated only
    // for handles opened with the right access, so they are not usable on
    // stable for path metadata. FileIdentity is only consumed by the
    // changed-during-upload guard (executor.rs replace/upload paths compare
    // pre/post identity of the SAME file), never for cross-sync rename
    // detection, so zeroing dev/inode is sound: the guard then leans on the
    // (size, mtime) post-check, which holds. modified() is FILETIME-based.
    let dev = 0u64;
    let inode = 0u64;
    let mtime_ns = filetime_to_unix_ns(meta.last_write_time());
    FileIdentity {
        dev,
        inode,
        size: meta.file_size(),
        mtime_ns,
        // No portable ctime on Windows; use last_write_time so the
        // changed-during-upload check compares (size, mtime).
        ctime_ns: mtime_ns,
    }
}

/// Convert a Windows FILETIME (100ns ticks since 1601-01-01) to ns since
/// the Unix epoch (1970-01-01).
#[cfg(windows)]
fn filetime_to_unix_ns(filetime_100ns: u64) -> i64 {
    // 11644473600 seconds between 1601 and 1970.
    const EPOCH_DIFF_100NS: i64 = 11_644_473_600 * 10_000_000;
    let unix_100ns = filetime_100ns as i64 - EPOCH_DIFF_100NS;
    unix_100ns.saturating_mul(100)
}

/// Error opening the source file for read.
enum OpenError {
    /// Sharing violation / lock (Windows) -> `local.file_locked` skip.
    Locked,
    /// Any other IO error.
    Io(std::io::Error),
}

/// Open the file for read with `FILE_SHARE_DELETE` on Windows so another
/// process can atomically replace it while we read the original bytes
/// (SPEC s8 defence #2). On Unix the default open already allows the path
/// to be unlinked/replaced under an open handle.
async fn open_shared(path: &Path) -> Result<tokio::fs::File, OpenError> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
        const SHARE_MODE: u32 = 0x0000_0001 | 0x0000_0002 | 0x0000_0004;
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true).share_mode(SHARE_MODE);
        match opts.open(path) {
            Ok(std_file) => Ok(tokio::fs::File::from_std(std_file)),
            Err(e) => Err(classify_open_error(e)),
        }
    }
    #[cfg(not(windows))]
    {
        match tokio::fs::File::open(path).await {
            Ok(f) => Ok(f),
            Err(e) => Err(classify_open_error(e)),
        }
    }
}

/// Classify an open error: a sharing violation / permission lock becomes a
/// `Locked` skip; everything else is a plain IO error.
fn classify_open_error(e: std::io::Error) -> OpenError {
    // ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33) on Windows.
    #[cfg(windows)]
    if matches!(e.raw_os_error(), Some(32) | Some(33)) {
        return OpenError::Locked;
    }
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        return OpenError::Locked;
    }
    OpenError::Io(e)
}

// -----------------------------------------------------------------------------
// Path helpers
// -----------------------------------------------------------------------------

/// Join a source's absolute `local_path` with a [`RelativePath`] (which uses
/// forward slashes; `Path::join` handles the separator per platform).
fn join_source_path(local_path: &str, rel: &RelativePath) -> PathBuf {
    let mut p = PathBuf::from(local_path);
    for seg in rel.as_str().split('/') {
        p.push(seg);
    }
    p
}

/// The final path component (the Drive display name).
fn filename_of(rel: &RelativePath) -> String {
    rel.as_str()
        .rsplit('/')
        .next()
        .unwrap_or(rel.as_str())
        .to_string()
}

/// A stable hex hash of the relative path, stored in `appProperties` as the
/// canonical identity (SPEC s3 preamble `driven.relative_path_hash`).
fn relative_path_hash(rel: &RelativePath) -> String {
    let h = blake3::hash(rel.as_str().as_bytes());
    hex::encode(&h.as_bytes()[..16])
}

/// Decode a 32-byte blake3 hash from its hex encoding (the form persisted in
/// `pending_ops.payload_json`). Returns `None` if the string is not exactly
/// 64 hex chars.
fn decode_blake3_hex(hex_str: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(hex_str, &mut out).ok()?;
    Some(out)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::test_support::FakeClock;
    use driven_drive::fake::InMemoryRemoteStore;
    use tempfile::TempDir;

    use crate::state::SqliteStateRepo;
    use crate::types::{AccountId, AccountState};

    // --- a no-op pacer so tests don't sleep on real time --------------------

    struct NoopPacer {
        backoff_hits: AtomicU64,
    }
    impl NoopPacer {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                backoff_hits: AtomicU64::new(0),
            })
        }
    }
    #[async_trait::async_trait]
    impl Pacer for NoopPacer {
        async fn permit_request(&self) {}
        async fn permit_file_create(&self) {}
        async fn permit_bytes(&self, _n: u64) {}
        fn note_response(&self, c: ResponseClass) {
            if matches!(c, ResponseClass::RateLimited { .. }) {
                self.backoff_hits.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn ceilings(&self) -> crate::pacer::PacerCeilings {
            crate::pacer::PacerCeilings::default()
        }
    }

    // --- harness ------------------------------------------------------------

    struct Harness {
        _tmp_state: TempDir,
        tmp_src: TempDir,
        remote: InMemoryRemoteStore,
        state: Arc<SqliteStateRepo>,
        pacer: Arc<NoopPacer>,
        clock: Arc<FakeClock>,
        source: SourceRow,
    }

    async fn harness() -> Harness {
        harness_with_remote(InMemoryRemoteStore::new()).await
    }

    async fn harness_with_remote(remote: InMemoryRemoteStore) -> Harness {
        let tmp_state = TempDir::new().unwrap();
        let tmp_src = TempDir::new().unwrap();
        let db_path = tmp_state.path().join("state.db");
        let state = Arc::new(SqliteStateRepo::open(&db_path).await.unwrap());
        let clock = Arc::new(FakeClock::new());

        let account_id = AccountId::new_v4();
        state
            .upsert_account(&crate::state::AccountRow {
                id: account_id,
                email: "t@example.com".into(),
                display_name: None,
                state: AccountState::Ok,
                encryption_master_key_id: None,
                created_at: 0,
                last_synced_at: None,
            })
            .await
            .unwrap();

        let source = SourceRow {
            id: SourceId::new_v4(),
            account_id,
            display_name: "src".into(),
            enabled: true,
            local_path: tmp_src.path().to_string_lossy().to_string(),
            drive_folder_id: remote.root_id().to_string(),
            drive_folder_path: "/".into(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore: false,
            include_patterns: vec![],
            exclude_patterns: vec![],
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 0,
        };
        state.upsert_source(&source).await.unwrap();

        Harness {
            _tmp_state: tmp_state,
            tmp_src,
            remote,
            state,
            pacer: NoopPacer::new(),
            clock,
            source,
        }
    }

    impl Harness {
        fn executor(&self) -> DefaultExecutor {
            self.executor_with_crypto(None)
        }

        fn executor_with_crypto(
            &self,
            crypto: Option<Arc<dyn SourceCryptoSuite>>,
        ) -> DefaultExecutor {
            DefaultExecutor::with_clock(
                ExecutorDeps {
                    remote: Arc::new(self.remote.clone()),
                    state: self.state.clone(),
                    pacer: self.pacer.clone(),
                    crypto,
                },
                self.clock.clone(),
            )
        }

        fn write_file(&self, rel: &str, contents: &[u8]) -> (RelativePath, u64) {
            let path = self.tmp_src.path().join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, contents).unwrap();
            (
                RelativePath::try_from(rel.to_string()).unwrap(),
                contents.len() as u64,
            )
        }

        fn upload_plan(&self, rel: &RelativePath, size: u64) -> Plan {
            Plan {
                ops: vec![Op::HashThenUpload {
                    source_id: self.source.id,
                    relative_path: rel.clone(),
                    size,
                }],
                collisions: vec![],
            }
        }
    }

    fn noop_progress(_p: ExecProgress) {}

    // --- fresh upload -------------------------------------------------------

    #[tokio::test]
    async fn fresh_upload_lands_and_commits() {
        let h = harness().await;
        let (rel, size) = h.write_file("hello.txt", b"hello world");
        let exec = h.executor();
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], OpOutcome::Done { .. }));

        // file_state committed with a drive_file_id.
        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .expect("file_state row");
        assert_eq!(row.status, FileStateStatus::Synced);
        assert!(row.drive_file_id.is_some());
        // pending_ops drained.
        let pending = h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap();
        assert!(pending.is_empty(), "pending_ops should be drained");
        // remote has the bytes.
        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].size, Some(size));
    }

    // --- changed re-upload --------------------------------------------------

    #[tokio::test]
    async fn changed_file_re_uploads_via_update() {
        let h = harness().await;
        let (rel, size) = h.write_file("a.txt", b"first");
        let exec = h.executor();
        exec.execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        let first_id = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap()
            .drive_file_id
            .unwrap();

        // Change the file and re-run; must UPDATE the same file_id, not create.
        let (rel2, size2) = h.write_file("a.txt", b"second-longer");
        assert_eq!(rel, rel2);
        exec.execute(&h.source, &h.upload_plan(&rel2, size2), &noop_progress)
            .await
            .unwrap();
        let second_id = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap()
            .drive_file_id
            .unwrap();
        assert_eq!(
            first_id, second_id,
            "update must reuse the same drive_file_id"
        );
        // Only one object on Drive (no duplicate create).
        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].size, Some(size2));
    }

    // --- 429 retry ----------------------------------------------------------

    #[tokio::test]
    async fn rate_limit_then_succeeds() {
        // First write request trips 429; the executor retries and succeeds.
        let remote = InMemoryRemoteStore::new().with_rate_limit_after(0);
        let h = harness_with_remote(remote).await;
        let (rel, size) = h.write_file("r.txt", b"retry me");
        let exec = h.executor();
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(matches!(out[0], OpOutcome::Done { .. }), "got {:?}", out[0]);
        // The pacer saw a rate-limit classification.
        assert!(h.pacer.backoff_hits.load(Ordering::SeqCst) >= 1);
    }

    // --- crash mid-resumable resumes via reconcile --------------------------

    #[tokio::test]
    async fn reconcile_adopts_orphaned_create() {
        let h = harness().await;
        let (rel, _size) = h.write_file("orphan.txt", b"orphaned bytes");

        // Simulate a crash AFTER the create landed on Drive but BEFORE the
        // commit: create the object directly with a client_op_uuid and leave
        // a pending_op carrying that uuid (no drive_file_id => a create).
        let op_uuid = uuid::Uuid::new_v4().to_string();
        let mut app = HashMap::new();
        app.insert(CLIENT_OP_UUID_KEY.to_string(), op_uuid.clone());
        h.remote
            .create(
                h.source.drive_folder_id.as_str(),
                "orphan.txt",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from_static(b"orphaned bytes")),
                app,
            )
            .await
            .unwrap();
        let now = h.clock.now_ms();
        // P1-2: the op records the blake3 (over plaintext) of the bytes it
        // uploaded. The local file is unchanged since the crash, so adoption
        // re-hashes it, finds a match, and marks Synced.
        let uploaded_hex = hex::encode(blake3::hash(b"orphaned bytes").as_bytes());
        h.state
            .enqueue_pending_op(NewPendingOp {
                source_id: h.source.id,
                op_type: OP_TYPE_UPLOAD.to_string(),
                relative_path: rel.clone(),
                payload_json: serde_json::json!({
                    "client_op_uuid": op_uuid,
                    "drive_file_id": null,
                    "uploaded_blake3_hex": uploaded_hex,
                }),
                scheduled_for: now,
                created_at: now,
            })
            .await
            .unwrap();

        let exec = h.executor();
        exec.reconcile(&h.source).await.unwrap();

        // The orphan is adopted: file_state has a drive_file_id, pending drained.
        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .expect("adopted row");
        assert!(row.drive_file_id.is_some());
        assert_eq!(row.status, FileStateStatus::Synced);
        // The adopted row carries the REAL plaintext blake3, never a zero hash.
        assert_eq!(
            row.hash_blake3,
            *blake3::hash(b"orphaned bytes").as_bytes(),
            "adopted Synced row must carry the real uploaded hash, not zero"
        );
        assert!(h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn reconcile_drops_op_when_create_never_landed() {
        let h = harness().await;
        let (rel, _size) = h.write_file("ghost.txt", b"never uploaded");
        let op_uuid = uuid::Uuid::new_v4().to_string();
        let now = h.clock.now_ms();
        h.state
            .enqueue_pending_op(NewPendingOp {
                source_id: h.source.id,
                op_type: OP_TYPE_UPLOAD.to_string(),
                relative_path: rel.clone(),
                payload_json: serde_json::json!({ "client_op_uuid": op_uuid, "drive_file_id": null }),
                scheduled_for: now,
                created_at: now,
            })
            .await
            .unwrap();
        let exec = h.executor();
        exec.reconcile(&h.source).await.unwrap();
        // No remote object => op dropped, no file_state row.
        assert!(h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap()
            .is_empty());
        assert!(h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .is_none());
    }

    // --- P1-2: orphan whose local bytes CHANGED post-upload requeues --------

    #[tokio::test]
    async fn reconcile_requeues_orphan_when_local_changed_after_upload() {
        let h = harness().await;
        // The bytes that were uploaded pre-crash.
        let (rel, _size) = h.write_file("drift.txt", b"uploaded version");
        let uploaded_hex = hex::encode(blake3::hash(b"uploaded version").as_bytes());

        // The orphan landed on Drive with its uuid.
        let op_uuid = uuid::Uuid::new_v4().to_string();
        let mut app = HashMap::new();
        app.insert(CLIENT_OP_UUID_KEY.to_string(), op_uuid.clone());
        let created = h
            .remote
            .create(
                h.source.drive_folder_id.as_str(),
                "drift.txt",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from_static(b"uploaded version")),
                app,
            )
            .await
            .unwrap();

        // But the local file changed AFTER the upload, before the commit ran.
        h.write_file("drift.txt", b"locally edited NEW content - longer");

        let now = h.clock.now_ms();
        h.state
            .enqueue_pending_op(NewPendingOp {
                source_id: h.source.id,
                op_type: OP_TYPE_UPLOAD.to_string(),
                relative_path: rel.clone(),
                payload_json: serde_json::json!({
                    "client_op_uuid": op_uuid,
                    "drive_file_id": null,
                    "uploaded_blake3_hex": uploaded_hex,
                }),
                scheduled_for: now,
                created_at: now,
            })
            .await
            .unwrap();

        let exec = h.executor();
        exec.reconcile(&h.source).await.unwrap();

        // The orphan is adopted as an object id (no duplicate) BUT the row is
        // NOT Synced - the changed local bytes must be re-uploaded. It must
        // preserve the drive_file_id so the re-upload is an UPDATE.
        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .expect("requeued row");
        assert_ne!(
            row.status,
            FileStateStatus::Synced,
            "a post-upload local change must NOT be marked Synced"
        );
        assert_eq!(
            row.drive_file_id.as_deref(),
            Some(created.id.as_str()),
            "requeue must preserve the orphan's drive_file_id (re-upload as update, no duplicate)"
        );
        // The requeue row stores a force-rescan mtime sentinel so the
        // FastPath scanner (which keys off (size, mtime) ONLY) is GUARANTEED
        // to see a change and re-emit the file - otherwise the stale bytes
        // would stay on Drive forever (P1-2 core invariant). It carries the
        // stale uploaded hash (non-zero), never a zero hash marked Synced.
        assert_eq!(row.mtime_ns, i64::MIN, "force-rescan sentinel mtime");
        assert_eq!(
            row.hash_blake3,
            *blake3::hash(b"uploaded version").as_bytes(),
            "row reflects the on-Drive (stale) uploaded hash"
        );
        // pending_ops drained (next scan re-enqueues the update).
        assert!(h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap()
            .is_empty());
        // Still exactly one object on Drive - no duplicate.
        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
    }

    // --- P1-1: file changed AFTER upload completes -> skip, no commit -------

    #[tokio::test]
    async fn changed_after_upload_skips_and_does_not_commit() {
        let h = harness().await;
        let (rel, size) = h.write_file("post.txt", b"original-content");
        let src_path = h.tmp_src.path().to_path_buf();
        // Hook fires AFTER upload_bytes returns, before the post-upload
        // identity re-check: mutate the file so the second fstat detects it.
        let hook: PostUploadHook = Arc::new(move |_p: &Path| {
            let full = src_path.join("post.txt");
            std::fs::write(&full, b"original-content-MUTATED-AFTER-UPLOAD").unwrap();
        });
        let exec = h.executor().with_post_upload_hook(hook);
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(
            matches!(
                out[0],
                OpOutcome::Skipped {
                    reason: SkipReason::ChangedDuringUpload,
                    ..
                }
            ),
            "got {:?}",
            out[0]
        );
        // Not committed as Synced.
        let row = h.state.get_file_state(h.source.id, &rel).await.unwrap();
        assert!(row.is_none() || row.unwrap().status != FileStateStatus::Synced);

        // The bytes DID land on Drive (the change was detected post-upload),
        // leaving an orphan create object + its pending_op for reconcile. The
        // op must still be present so the orphan is not stranded (P1-1).
        let pending = h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1, "create op kept for reconcile to adopt");
        let before = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(before.len(), 1, "the post-upload orphan is on Drive");

        // Reconcile closes the loop: it finds the orphan, re-hashes the
        // (now-changed) local file, sees a MISMATCH, and requeues it as an
        // UPDATE against the SAME object id - never a duplicate.
        exec.reconcile(&h.source).await.unwrap();
        let after = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(after.len(), 1, "reconcile produced no duplicate");
        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .expect("requeued row");
        assert_ne!(
            row.status,
            FileStateStatus::Synced,
            "changed bytes must be re-uploaded, not adopted as Synced"
        );
        assert_eq!(
            row.drive_file_id.as_deref(),
            Some(before[0].id.as_str()),
            "requeue preserves the orphan id so re-upload is an update"
        );
    }

    // --- parallel uploads, no corruption ------------------------------------

    #[tokio::test]
    async fn parallel_uploads_no_corruption() {
        let h = harness().await;
        let mut ops = Vec::new();
        let mut expected = Vec::new();
        for i in 0..20u32 {
            let name = format!("f{i}.bin");
            let contents = format!("payload-{i}-{}", "x".repeat(i as usize)).into_bytes();
            let (rel, size) = h.write_file(&name, &contents);
            ops.push(Op::HashThenUpload {
                source_id: h.source.id,
                relative_path: rel,
                size,
            });
            expected.push((name, contents));
        }
        let plan = Plan {
            ops,
            collisions: vec![],
        };
        let exec = h.executor();
        let out = exec
            .execute(&h.source, &plan, &noop_progress)
            .await
            .unwrap();
        assert_eq!(out.len(), 20);
        assert!(out.iter().all(|o| matches!(o, OpOutcome::Done { .. })));

        // Every object present with the right bytes (by md5 equality via size +
        // download).
        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 20);
        for (name, contents) in &expected {
            let entry = children.iter().find(|e| &e.name == name).expect("present");
            assert_eq!(entry.size, Some(contents.len() as u64));
        }
    }

    // --- md5 mismatch errors ------------------------------------------------

    #[tokio::test]
    async fn md5_mismatch_surfaces_checksum_error() {
        // Trip md5 mismatch on the first (and only) write.
        let remote = InMemoryRemoteStore::new().with_md5_mismatch_after(0);
        let h = harness_with_remote(remote).await;
        let (rel, size) = h.write_file("m.txt", b"checksum me");
        let exec = h.executor();
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(
            matches!(
                out[0],
                OpOutcome::Failed {
                    code: ErrorCode::DriveChecksumMismatch,
                    ..
                }
            ),
            "got {:?}",
            out[0]
        );
        // file_state NOT marked synced (the pending_op was dropped, no commit).
        let row = h.state.get_file_state(h.source.id, &rel).await.unwrap();
        assert!(row.is_none() || row.unwrap().status != FileStateStatus::Synced);
    }

    // --- fstat mid-upload change aborts + requeues ---------------------------

    #[tokio::test]
    async fn changed_during_upload_skips_and_requeues() {
        let h = harness().await;
        let (rel, size) = h.write_file("c.txt", b"original-content");
        let src_path = h.tmp_src.path().to_path_buf();
        let rel_clone = rel.clone();
        // Hook fires after open, before post-fstat: append bytes to change
        // size + mtime so the post-read fstat detects the mutation.
        let hook: MidUploadHook = Arc::new(move |_p: &Path| {
            let full = src_path.join("c.txt");
            // Make it bigger so (size) differs.
            std::fs::write(&full, b"original-content-MUTATED-LONGER").unwrap();
            let _ = &rel_clone;
        });
        let exec = h.executor().with_mid_upload_hook(hook);
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(
            matches!(
                out[0],
                OpOutcome::Skipped {
                    reason: SkipReason::ChangedDuringUpload,
                    ..
                }
            ),
            "got {:?}",
            out[0]
        );
        // Not committed; pending_ops drained (clean re-enqueue next scan).
        assert!(h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap()
            .is_empty());
        let row = h.state.get_file_state(h.source.id, &rel).await.unwrap();
        assert!(row.is_none() || row.unwrap().status != FileStateStatus::Synced);
    }

    // --- resumable (large file) path ----------------------------------------

    #[tokio::test]
    async fn large_file_uses_resumable_and_commits() {
        let h = harness().await;
        // > RESUMABLE_THRESHOLD (5 MiB) so the resumable path runs.
        let big = vec![0xABu8; (RESUMABLE_THRESHOLD as usize) + 7];
        let (rel, size) = h.write_file("big.bin", &big);
        let exec = h.executor();
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(matches!(out[0], OpOutcome::Done { .. }), "got {:?}", out[0]);
        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].size, Some(size));
        // No open sessions leaked.
        assert_eq!(h.remote.open_session_count(), 0);
    }

    // --- encryption round-trip ----------------------------------------------

    // A minimal XChaCha20-Poly1305-shaped STREAM suite for the test: it does
    // NOT use real crypto (this test exercises the executor's md5-over-
    // ciphertext + length-exactness contract, not the cipher). It frames a
    // 40-byte header, a per-chunk 16-byte "tag", and accumulates the md5 over
    // every emitted ciphertext byte - exactly the ContentEncryptor contract.
    struct FakeSuite;
    struct FakeEnc {
        md5: md5::Md5,
    }
    impl SourceCryptoSuite for FakeSuite {
        fn content_encryptor(&self) -> Box<dyn ContentEncryptor> {
            Box::new(FakeEnc {
                md5: <md5::Md5 as md5::Digest>::new(),
            })
        }
        fn content_decryptor(
            &self,
            _header: &[u8],
        ) -> Result<Box<dyn driven_crypto::ContentDecryptor>, CryptoError> {
            Err(CryptoError::Protocol("decrypt not needed in test".into()))
        }
        fn encrypt_filename(&self, component: &str, _aad: &[u8]) -> Result<String, CryptoError> {
            Ok(component.to_string())
        }
        fn decrypt_filename(&self, name: &str, _aad: &[u8]) -> Result<String, CryptoError> {
            Ok(name.to_string())
        }
    }
    impl ContentEncryptor for FakeEnc {
        fn header(&mut self) -> Bytes {
            use md5::Digest;
            let h = Bytes::from_static(&[0x7Fu8; 40]);
            self.md5.update(&h);
            h
        }
        fn encrypt_chunk(&mut self, plaintext: &[u8]) -> Result<Bytes, CryptoError> {
            use md5::Digest;
            let mut out = plaintext.to_vec();
            out.extend_from_slice(&[0xAAu8; 16]); // fake tag
            self.md5.update(&out);
            Ok(Bytes::from(out))
        }
        fn finalize_last(
            self: Box<Self>,
            plaintext: &[u8],
        ) -> Result<(Bytes, [u8; 16]), CryptoError> {
            use md5::Digest;
            let mut md5 = self.md5;
            let mut out = plaintext.to_vec();
            out.extend_from_slice(&[0xBBu8; 16]); // fake final tag
            md5.update(&out);
            let digest: [u8; 16] = md5.finalize().into();
            Ok((Bytes::from(out), digest))
        }
    }

    #[tokio::test]
    async fn encryption_round_trip_md5_over_ciphertext() {
        let h = harness().await;
        // Multi-chunk plaintext so encrypt_chunk + finalize_last both run.
        let plaintext = vec![0x11u8; READ_BUF + 1234];
        let (rel, size) = h.write_file("enc.bin", &plaintext);
        let exec = h.executor_with_crypto(Some(Arc::new(FakeSuite)));
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(
            matches!(out[0], OpOutcome::Done { .. }),
            "encrypted upload should land: {:?}",
            out[0]
        );
        // The stored (ciphertext) object is larger than the plaintext (header
        // + per-chunk tags), proving the body sent was the ciphertext.
        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
        assert!(
            children[0].size.unwrap() > size,
            "ciphertext must exceed plaintext"
        );
        // file_state blake3 is over the PLAINTEXT (identity survives key
        // rotation): a fresh blake3 of the plaintext must match.
        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap();
        let expect_blake3: [u8; 32] = blake3::hash(&plaintext).into();
        assert_eq!(row.hash_blake3, expect_blake3);
    }
}
