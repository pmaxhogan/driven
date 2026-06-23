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
use driven_vss::{fallback_decision, FallbackDecision, OpenAttempt, SnapshotOutcome, VssMode};
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

/// Files at or above this use the bounded 3-stage producer/consumer
/// pipeline (DESIGN s5.4 / s11.4.3 `PIPELINE_THRESHOLD = 4 MiB`); below run
/// inline in one task (pipeline overhead would dominate). The pipeline
/// reads 64 KiB plaintext chunks (reader stage), hashes + encrypts them
/// (cpu stage), and accumulates the output into wire chunks for the
/// uploader stage - connected by BOUNDED channels so a 1 GiB file never
/// buffers more than [`PIPELINE_CHANNEL_CAP`] x [`READ_BUF`] of in-flight
/// data plus one wire chunk (DESIGN s11.4.6 memory bound).
pub const PIPELINE_THRESHOLD: u64 = 4 * 1024 * 1024;

/// Files at or above this hash with `blake3::Hasher::update_rayon` for
/// multi-core hashing in the pipeline cpu stage (DESIGN s5.4 / s11.4.4
/// `RAYON_HASH_THRESHOLD = 100 MiB`). The `blake3` crate is pulled with its
/// `rayon` feature so `update_rayon` is available; it is applied per
/// pipeline chunk above this size (not over a full mmap'd file, so the
/// DESIGN's 5 GB/s figure - which assumes one contiguous buffer - does not
/// apply; the win is multi-core hashing of each bounded chunk).
pub const RAYON_HASH_THRESHOLD: u64 = 100 * 1024 * 1024;

/// Bound on each inter-stage channel in the streaming pipeline (DESIGN
/// s11.4.3 "bounded(4)"). A small capacity so backpressure caps the
/// in-flight memory: the reader blocks once the cpu stage is this many
/// chunks behind, and the cpu stage blocks once the uploader is this many
/// wire chunks behind. 8 (vs the DESIGN's 4) leaves a little slack for
/// cooperative scheduling without materially raising the memory ceiling.
const PIPELINE_CHANNEL_CAP: usize = 8;

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
    /// The resume-safe file IDENTITY captured at session start (P1-2). The
    /// streaming upload produces the plaintext blake3 only DURING the upload,
    /// so a crash mid-stream leaves no `uploaded_blake3_hex` to validate a
    /// resume against. Instead we stamp the open handle's
    /// `(dev, inode, size, mtime_ns, ctime_ns)` BEFORE the first chunk: a
    /// resume re-opens the file, checks the identity is unchanged, resumes
    /// the byte stream from `acked_offset`, and computes the blake3 over the
    /// FULL re-read stream for the final integrity check (md5 vs Drive). The
    /// identity is the resume GATE; the content hash is computed fresh on
    /// resume, never required to start one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume_identity: Option<ResumeIdentity>,
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

/// The resume-safe identity of the local file an in-flight resumable upload
/// is reading (P1-2). Captured at session start from the OPEN handle, before
/// any bytes are pushed, so a resume after a crash can prove the local file
/// is byte-identical to what was being uploaded WITHOUT needing the (then
/// unknowable) final content hash. Mirrors the [`FileIdentity`] fields the
/// SPEC s8 change/replace defences compare; serialized into the op payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ResumeIdentity {
    /// Device id (Unix `dev`) / volume serial (Windows; 0 on stable).
    dev: u64,
    /// Inode (Unix `ino`) / file index (Windows; 0 on stable).
    inode: u64,
    /// File size in bytes at session start.
    size: u64,
    /// Modification time (ns since the Unix epoch) at session start.
    mtime_ns: i64,
    /// Change time (ns since the Unix epoch; Unix `ctime`, falls back to
    /// mtime on Windows) at session start.
    ctime_ns: i64,
}

impl ResumeIdentity {
    /// Capture the resume identity from an [`FileIdentity`] (the same struct
    /// the SPEC s8 defences fstat into), so the resume gate and the
    /// changed-during-upload guard share one source of truth.
    fn from_file_identity(id: FileIdentity) -> Self {
        Self {
            dev: id.dev,
            inode: id.inode,
            size: id.size,
            mtime_ns: id.mtime_ns,
            ctime_ns: id.ctime_ns,
        }
    }

    /// Does a freshly-fstat'd identity still match what we recorded at
    /// session start? Compares the full `(dev, inode, size, mtime, ctime)`
    /// tuple - any change means the local bytes are no longer the ones we
    /// were uploading, so the resume must be abandoned.
    fn matches(&self, id: &FileIdentity) -> bool {
        self.dev == id.dev
            && self.inode == id.inode
            && self.size == id.size
            && self.mtime_ns == id.mtime_ns
            && self.ctime_ns == id.ctime_ns
    }
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
    /// The file is locked and VSS could not help because it is UNAVAILABLE -
    /// the process is not elevated (or off Windows / `vss_mode = never`), so a
    /// shadow copy was never attempted. Distinct from [`SkipReason::Locked`]
    /// (where VSS WAS tried and still failed) so the user can tell "would back
    /// up if Driven ran elevated" from "genuinely unreadable"
    /// (`local.vss_unavailable`, SPEC s24).
    VssUnavailable,
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
            SkipReason::VssUnavailable => ErrorCode::LocalVssUnavailable,
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
    /// The per-cycle Windows VSS snapshot provider (ROADMAP M3.5, DESIGN
    /// s5.3), or `None` to disable the VSS fallback entirely (the historical
    /// behaviour: a locked file is always skipped). The orchestrator owns the
    /// snapshot lifecycle and passes a CLONE of the same `Arc` here so the
    /// executor's open path can read a locked file from the shadow copy.
    pub vss: Option<Arc<dyn driven_vss::VssProvider>>,
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
    /// Per-cycle VSS snapshot provider (ROADMAP M3.5), or `None` to disable
    /// the locked-file fallback (then a locked file is skipped as before).
    vss: Option<Arc<dyn driven_vss::VssProvider>>,
    /// Inter-file concurrency gate (DESIGN s11.4.2). `acquire`d per op.
    pool: Arc<Semaphore>,
    /// Test-only peak-memory gauge for the streaming pipeline (P1-4
    /// acceptance). `None` in production (zero overhead beyond an
    /// `Option::is_none` check per chunk). Exposed via the doc-hidden
    /// [`Self::with_mem_gauge`] so the `pipeline_cpu_and_memory_bounds`
    /// integration row can assert the in-flight bytes stay bounded far below
    /// the file size - the one qualitative pipeline contract that IS
    /// deterministically measurable against the instantaneous fake.
    mem_gauge: Option<Arc<MemGauge>>,
    #[cfg(test)]
    mid_upload_hook: Option<MidUploadHook>,
    #[cfg(test)]
    post_upload_hook: Option<PostUploadHook>,
}

/// A peak in-flight-bytes gauge for the streaming pipeline (P1-4 test
/// instrumentation). The reader stage bumps `current` (and tracks `peak`)
/// when it buffers a chunk; the uploader stage drops `current` once Drive
/// accepts a wire chunk. A correctly-bounded pipeline keeps `peak` a small
/// multiple of the channel/wire-chunk sizes regardless of file size; a
/// regression to whole-file buffering pushes `peak` to the file size.
///
/// `#[doc(hidden)]` + `pub` so the acceptance integration test (a separate
/// crate) can read it; not part of the supported API.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct MemGauge {
    current: std::sync::atomic::AtomicU64,
    peak: std::sync::atomic::AtomicU64,
}

impl MemGauge {
    /// Record `n` more bytes resident in the pipeline, updating the peak.
    fn add(&self, n: u64) {
        use std::sync::atomic::Ordering;
        let cur = self.current.fetch_add(n, Ordering::AcqRel) + n;
        self.peak.fetch_max(cur, Ordering::AcqRel);
    }

    /// Record `n` bytes leaving the pipeline (accepted by Drive).
    fn sub(&self, n: u64) {
        self.current
            .fetch_sub(n, std::sync::atomic::Ordering::AcqRel);
    }

    /// The peak simultaneous resident bytes observed. Read by the acceptance
    /// test after an upload to assert the pipeline stayed bounded.
    #[doc(hidden)]
    pub fn peak(&self) -> u64 {
        self.peak.load(std::sync::atomic::Ordering::Acquire)
    }
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
            vss: deps.vss,
            pool,
            mem_gauge: None,
            #[cfg(test)]
            mid_upload_hook: None,
            #[cfg(test)]
            post_upload_hook: None,
        }
    }

    /// Test-only (doc-hidden): attach a [`MemGauge`] so the streaming
    /// pipeline records its peak in-flight bytes. Used by the
    /// `pipeline_cpu_and_memory_bounds` acceptance row.
    #[doc(hidden)]
    pub fn with_mem_gauge(mut self, gauge: Arc<MemGauge>) -> Self {
        self.mem_gauge = Some(gauge);
        self
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

    /// Resolve the EFFECTIVE read path for `live_path` and open it, honouring
    /// the VSS fallback (ROADMAP M3.5, DESIGN s5.3).
    ///
    /// Flow (all pure-decision logic lives in `driven_vss::fallback_decision`):
    /// - No VSS provider (the historical config): open the live path; a lock
    ///   is an unconditional skip.
    /// - `vss_mode = always` + available: snapshot the volume even for a
    ///   readable file and read from the shadow copy (SPEC s22 "paranoid").
    /// - `vss_mode = auto`: open live first; only a locked file consults the
    ///   snapshot.
    /// - VSS unavailable (not elevated / `never` / off Windows / snapshot
    ///   failed): a locked file is skipped, exactly as before.
    ///
    /// Returns the opened handle paired with the path it was opened from (the
    /// live path or the snapshot-mapped path), so the caller threads ONE path
    /// through every SPEC s8 identity check.
    async fn open_effective(&self, live_path: &Path) -> EffectiveOpen {
        let Some(vss) = self.vss.as_ref() else {
            // No VSS configured: live open, lock => skip (historical path).
            return match open_shared(live_path).await {
                Ok(file) => EffectiveOpen::Opened {
                    read_path: live_path.to_path_buf(),
                    file,
                },
                Err(OpenError::Locked) => EffectiveOpen::Skip(SkipReason::Locked),
                Err(OpenError::Io(e)) => {
                    warn!(target: TARGET, path = %live_path.display(), error = %e, "open failed");
                    EffectiveOpen::Failed(ErrorCode::LocalIoError)
                }
            };
        };

        let mode = vss.mode();
        let elevated = vss.available();

        // `always` mode consults the snapshot up front even when the live open
        // would succeed (the dead-code trap the advisor flagged: if we only
        // hooked the Locked arm, `always` would silently behave as `auto`).
        // `auto` / `never` open live first.
        let attempt;
        let mut live_file = None;
        if mode == VssMode::Always {
            // Do not even attempt the live open in always mode; we route reads
            // through the snapshot. Probe lock state only to feed the decision.
            attempt = match open_shared(live_path).await {
                Ok(file) => {
                    live_file = Some(file);
                    OpenAttempt::Ok
                }
                Err(OpenError::Locked) => OpenAttempt::Locked,
                Err(OpenError::Io(e)) => {
                    warn!(target: TARGET, path = %live_path.display(), error = %e, "open failed");
                    return EffectiveOpen::Failed(ErrorCode::LocalIoError);
                }
            };
        } else {
            match open_shared(live_path).await {
                Ok(file) => {
                    // Live open worked; in auto/never this is the read path.
                    return EffectiveOpen::Opened {
                        read_path: live_path.to_path_buf(),
                        file,
                    };
                }
                Err(OpenError::Locked) => attempt = OpenAttempt::Locked,
                Err(OpenError::Io(e)) => {
                    warn!(target: TARGET, path = %live_path.display(), error = %e, "open failed");
                    return EffectiveOpen::Failed(ErrorCode::LocalIoError);
                }
            }
        }

        // Consult the provider for this file's volume only when the decision
        // needs it (always-mode always; auto-mode only on a lock). The provider
        // lazily creates + caches one snapshot per volume for the cycle.
        let snapshot = if mode == VssMode::Always || attempt == OpenAttempt::Locked {
            vss.map_for_volume(live_path)
        } else {
            SnapshotOutcome::Unavailable
        };

        match fallback_decision(attempt, mode, elevated, snapshot) {
            FallbackDecision::OpenLive => {
                // The pure decision chose the live bytes (e.g. always-mode but
                // the snapshot failed on a readable file). Reuse the handle we
                // already opened if we have it, else open now.
                if let Some(file) = live_file {
                    EffectiveOpen::Opened {
                        read_path: live_path.to_path_buf(),
                        file,
                    }
                } else {
                    match open_shared(live_path).await {
                        Ok(file) => EffectiveOpen::Opened {
                            read_path: live_path.to_path_buf(),
                            file,
                        },
                        Err(OpenError::Locked) => EffectiveOpen::Skip(SkipReason::Locked),
                        Err(OpenError::Io(e)) => {
                            warn!(target: TARGET, path = %live_path.display(), error = %e, "open failed");
                            EffectiveOpen::Failed(ErrorCode::LocalIoError)
                        }
                    }
                }
            }
            FallbackDecision::OpenSnapshot(snapshot_path) => {
                // Open the frozen shadow-copy path. A second sharing violation
                // here (extremely unusual - the snapshot is read-only) degrades
                // to skip.
                match open_shared(&snapshot_path).await {
                    Ok(file) => {
                        tracing::info!(target: TARGET, live = %live_path.display(), snapshot = %snapshot_path.display(), "VSS: reading locked file from snapshot");
                        EffectiveOpen::Opened {
                            read_path: snapshot_path,
                            file,
                        }
                    }
                    Err(OpenError::Locked) => EffectiveOpen::Skip(SkipReason::Locked),
                    Err(OpenError::Io(e)) => {
                        warn!(target: TARGET, path = %snapshot_path.display(), error = %e, "VSS: snapshot open failed; degrading to skip");
                        EffectiveOpen::Skip(SkipReason::Locked)
                    }
                }
            }
            FallbackDecision::SkipLocked => {
                // P2-6 (SPEC s24): distinguish "locked + VSS unavailable" from
                // "locked + VSS tried and failed". `elevated` is the provider's
                // `available()`; when VSS is unavailable (un-elevated / off
                // Windows / `never`) a snapshot was never attempted for this
                // locked file, so surface `local.vss_unavailable` ("would back
                // up if Driven ran elevated"). When VSS WAS available but the
                // snapshot/map still failed (`snapshot == Unavailable` despite
                // elevation), it is a genuine `local.file_locked`.
                let reason = if attempt == OpenAttempt::Locked && !elevated {
                    SkipReason::VssUnavailable
                } else {
                    SkipReason::Locked
                };
                EffectiveOpen::Skip(reason)
            }
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
        let live_path = join_source_path(&source.local_path, relative_path);

        // --- resolve the EFFECTIVE read path + open it ----------------------
        // M3.5: a locked file (ERROR_SHARING_VIOLATION) falls through to a VSS
        // snapshot, in which case EVERY subsequent filesystem operation for
        // this op - the pre-open lstat, the post-read fstat, and the
        // post-upload lstat recheck (SPEC s8 change/replace defences) - must
        // read from the FROZEN snapshot path, not the live path. Comparing a
        // frozen stat to a live one on an actively-written file (the whole
        // VSS use case) would spuriously trip ChangedDuringUpload and the file
        // would never back up. So `read_path` is the single source of truth
        // for all FS reads below; only `relative_path` / the Drive target stay
        // logical. When VSS is off / unavailable, `read_path == live_path` and
        // the behaviour is identical to before.
        let (read_path, mut file) = match self.open_effective(&live_path).await {
            EffectiveOpen::Opened { read_path, file } => (read_path, file),
            EffectiveOpen::Skip(reason) => {
                return Ok(OpOutcome::Skipped {
                    relative_path: relative_path.clone(),
                    reason,
                });
            }
            EffectiveOpen::Failed(code) => {
                return Ok(OpOutcome::Failed {
                    relative_path: relative_path.clone(),
                    code,
                });
            }
        };

        // --- pre-open lstat (on the effective path) ------------------------
        let pre = match lstat_identity(&read_path) {
            Ok(id) => id,
            Err(e) => {
                warn!(target: TARGET, path = %read_path.display(), error = %e, "lstat failed");
                return Ok(OpOutcome::Failed {
                    relative_path: relative_path.clone(),
                    code: ErrorCode::LocalIoError,
                });
            }
        };
        let opened = fstat_identity(&file).await?;
        if (opened.dev, opened.inode) != (pre.dev, pre.inode) {
            // Replaced between our lstat and our open (SPEC s8 defence #1).
            // On a snapshot read this is vacuously satisfied (the shadow copy
            // is immutable), so it only ever fires on a live read.
            return Ok(OpOutcome::Skipped {
                relative_path: relative_path.clone(),
                reason: SkipReason::ReplacedBeforeOpen,
            });
        }

        // Test seam: let a test mutate/replace the file now, before we read
        // and post-fstat, so the mid-upload defences are deterministic. Fired
        // on the LIVE path - a test that mutates the live file while we read a
        // snapshot copy must see the op still complete (the snapshot is
        // frozen), which is exactly the VSS contract.
        self.fire_mid_upload_hook(&live_path);

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
        // P1-B: a read served from a VSS snapshot (read_path != live_path) is
        // NON-RESUMABLE. The per-cycle shadow is released at cycle end, so a
        // crash mid-resumable could not resume against the same frozen bytes
        // next cycle (the snapshot is gone), and reconcile reopening the LIVE
        // file would splice a different/locked byte stream. So force the simple
        // (non-resumable) upload path and never persist a resumable session for
        // a snapshot read; on failure the op is preserved + requeued clean so
        // the next cycle re-snapshots + re-uploads from scratch.
        let from_vss = read_path != live_path;
        let app_props = self.app_properties(source.id, relative_path, &op_uuid);
        let outcome = self
            .upload_and_commit(
                source,
                relative_path,
                &read_path,
                size,
                &mut file,
                pre,
                existing_file_id.as_deref(),
                op_id,
                payload,
                app_props,
                from_vss,
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
            Err(UploadError::DeferToReconcile(code)) => {
                // P1-3: ambiguous create failure. KEEP the pending op (do NOT
                // delete it) so the reconcile pass can adopt the orphan by
                // op-uuid (if Drive did create it) or requeue a clean create
                // (if it did not) - either way no duplicate. The file is
                // reported as Failed for this pass (left non-synced), but the
                // surviving op carries the recovery forward. This is the only
                // Failed-shaped outcome that does NOT drop the op.
                //
                // Post-state (op kept, NO file_state row, create semantics) is
                // byte-identical to the SkipPostUpload-create arm above, so the
                // next scan re-plans the path the same way that already-shipped
                // path does. Recovery is reconcile's job; reconcile iterates
                // pending_ops (not file_state), so the surviving op IS the
                // discovery handle. Writing a file_state row here would be
                // wrong both ways: a sentinel-mtime row has no drive_file_id to
                // update against (fresh create -> duplicate), and a real-mtime
                // row makes the FastPath scanner treat the never-created file
                // as handled (silent data loss). The mid-run-defer-before-next-
                // reconcile window (reconcile is startup-gated) is the
                // orchestrator's cadence concern, shared identically with
                // SkipPostUpload - not fixable from inside the executor.
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
    ///
    /// Two upload strategies share this driver:
    /// - Files below [`PIPELINE_THRESHOLD`] read fully into memory once
    ///   (`read_hash_encrypt`), then upload the buffered bytes. Cheap; the
    ///   pipeline overhead would dominate a tiny file.
    /// - Files at or above it run the bounded 3-stage streaming pipeline
    ///   (`stream_upload`, DESIGN s11.4.3): reader -> cpu(hash+encrypt) ->
    ///   uploader, connected by BOUNDED channels so the in-flight memory is
    ///   capped regardless of file size (a 1 GiB file never buffers 1 GiB).
    ///
    /// Both produce identical observable state - blake3-over-plaintext,
    /// md5-over-the-exact-sent-bytes, the P1-1 post-upload identity recheck,
    /// the P1-5 encrypted remote path - and commit the same `file_state`
    /// row. The Cluster-A crash-safety invariants are preserved in both.
    #[allow(clippy::too_many_arguments)]
    async fn upload_and_commit(
        &self,
        source: &SourceRow,
        relative_path: &RelativePath,
        read_path: &Path,
        // The scanner's LIVE size, deliberately ignored below in favour of the
        // EFFECTIVE (snapshot) size from `pre` (P1-3). Kept in the signature so
        // the caller's plumbing is unchanged and the intent is explicit.
        _scanner_size: u64,
        file: &mut tokio::fs::File,
        pre: FileIdentity,
        existing_file_id: Option<&str>,
        op_id: PendingOpId,
        mut payload: PendingOpPayload,
        app_props: HashMap<String, String>,
        // P1-B: false when the bytes are read from a VSS snapshot - then NO
        // resumable session is opened/persisted (the per-cycle shadow is gone
        // by next cycle, so a resume could not re-read the same frozen bytes).
        from_vss: bool,
    ) -> Result<OpOutcome, UploadError> {
        // M3.5: every FS recheck below reads the EFFECTIVE path (the VSS
        // snapshot copy for a locked file), so a frozen-vs-live stat mismatch
        // never trips on an actively-written source. `read_path == live` when
        // VSS is off.
        let full_path = read_path.to_path_buf();
        let mime = "application/octet-stream";
        // P1-B: resumable is allowed only for a live read. A snapshot read uses
        // the simple (single-shot) upload path regardless of size.
        let allow_resumable = !from_vss;

        // P1-3 (M3.5 codex): size ALL of the upload off the EFFECTIVE (snapshot)
        // stat, not the scanner's live `size`. The scanner measured the LIVE
        // file before the snapshot froze it; on a locked DB/PST that keeps
        // changing, the coherent snapshot byte count can differ. Using the live
        // size here would make the read-stage length check, the resumable /
        // pipeline threshold decisions, the resume identity, and progress
        // accounting all disagree with the bytes we actually read - tripping a
        // spurious ChangedDuringUpload on exactly the file VSS exists to back
        // up. `pre` is the lstat of `read_path` (the snapshot path when VSS is
        // engaged, else the live path), so `pre.size` is the effective size.
        // When VSS is off this equals the scanner's live size and behaviour is
        // unchanged.
        let size = pre.size;

        // --- P1-5: resolve the remote target (encrypted path when the source
        // is encrypted; flat plaintext name otherwise). This ensure_folders
        // the encrypted parent directory components up front so the upload
        // lands under the ciphertext path, not a leaked plaintext name.
        let target = self
            .resolve_remote_target(source, relative_path, app_props)
            .await
            .map_err(UploadError::Fatal)?;

        // --- P1-2: stamp the resume-safe IDENTITY before any bytes leave ---
        // Only resumable uploads can be resumed across a crash; for those, the
        // streaming pipeline cannot persist the final content hash until the
        // upload finishes, so a mid-stream crash would otherwise leave a
        // session with no way to validate a resume. We persist the open
        // handle's identity NOW (it is byte-identical to what the reader will
        // hash), so `resume_persisted` can prove the local file is unchanged
        // without the (still-unknown) content hash. The plaintext blake3 is
        // re-derived over the full re-read stream on resume (final integrity
        // check vs Drive's md5). For sub-resumable uploads there is no resume,
        // so we skip the extra write. P1-B: a snapshot read is never resumable,
        // so it never stamps a resume identity either - reconcile must not try
        // to resume a VSS op against the (gone) snapshot or the live file.
        if allow_resumable && size >= RESUMABLE_THRESHOLD {
            payload.resume_identity = Some(ResumeIdentity::from_file_identity(pre));
            self.state
                .update_pending_op_payload(op_id, &payload.to_value())
                .await
                .map_err(UploadError::Fatal)?;
        }

        // --- produce the exact upload body + its hashes --------------------
        // Below the pipeline threshold: buffer the whole file once. At or
        // above: drive the bounded streaming pipeline. Both yield the same
        // (blake3-over-plaintext, the uploaded RemoteEntry) and verify the
        // md5 over the exact sent bytes (SPEC s8). The streaming arm returns
        // the entry directly (it cannot return a buffered body); the inline
        // arm buffers then uploads.
        let UploadProduct { blake3, entry } = if size >= PIPELINE_THRESHOLD {
            self.stream_upload(
                source,
                relative_path,
                size,
                file,
                pre,
                existing_file_id,
                mime,
                &target,
                op_id,
                &mut payload,
                allow_resumable,
            )
            .await?
        } else {
            self.inline_upload(
                source,
                relative_path,
                &full_path,
                size,
                file,
                pre,
                existing_file_id,
                mime,
                &target,
                op_id,
                &mut payload,
                allow_resumable,
            )
            .await?
        };

        // --- P1-1: post-UPLOAD identity re-check (SPEC s8) -----------------
        // The hash/read proved the file did not change while we were READING
        // it; but the bytes could still be mutated between the read and the
        // moment Drive finishes accepting them. Re-stat the open handle AND
        // the path now that the object is fully uploaded, before we commit it
        // as Synced. On any change/replace, do NOT commit: the remote object
        // is an orphan that reconcile adopts + re-hashes (P1-2), and the op
        // is re-enqueued by the caller.
        self.fire_post_upload_hook(&full_path);
        let post = fstat_identity(file).await.map_err(UploadError::Fatal)?;
        if (post.size, post.ctime_ns) != (pre.size, pre.ctime_ns) {
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
            encrypted_remote_path: target.encrypted_remote_path.clone(),
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

    /// Inline (fully-buffered) upload path for files below
    /// [`PIPELINE_THRESHOLD`]. Reads + hashes + (encrypts) the whole file
    /// once, runs the post-read identity check, persists the uploaded blake3
    /// (P1-2), then uploads the buffered bytes and verifies md5.
    #[allow(clippy::too_many_arguments)]
    async fn inline_upload(
        &self,
        _source: &SourceRow,
        _relative_path: &RelativePath,
        read_path: &Path,
        size: u64,
        file: &mut tokio::fs::File,
        pre: FileIdentity,
        existing_file_id: Option<&str>,
        mime: &str,
        target: &RemoteTarget,
        op_id: PendingOpId,
        payload: &mut PendingOpPayload,
        // P1-B: false for a VSS-snapshot read - force the non-resumable path.
        allow_resumable: bool,
    ) -> Result<UploadProduct, UploadError> {
        // M3.5: the post-read identity recheck reads the EFFECTIVE path (the
        // VSS snapshot copy for a locked file), matching the pre-open lstat.
        let full_path = read_path.to_path_buf();

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
            Err(_) => return Err(UploadError::Skip(SkipReason::ReplacedDuringUpload)),
        }

        // The plaintext we read must match the size the planner observed; a
        // grow/shrink is the changed-during-upload case the fstat check above
        // already catches, but guard explicitly so the declared upload length
        // is never wrong for an unencrypted body.
        if self.crypto.is_none() && plaintext_len != size {
            return Err(UploadError::Skip(SkipReason::ChangedDuringUpload));
        }

        // --- P1-2: persist the uploaded blake3 BEFORE the bytes land -------
        payload.uploaded_blake3_hex = Some(hex::encode(blake3));
        self.state
            .update_pending_op_payload(op_id, &payload.to_value())
            .await
            .map_err(UploadError::Fatal)?;

        let entry = self
            .upload_bytes(
                target,
                existing_file_id,
                sent_bytes,
                mime,
                op_id,
                payload,
                allow_resumable,
            )
            .await?;
        Ok(UploadProduct { blake3, entry })
    }

    /// The bounded 3-stage streaming upload pipeline for files at or above
    /// [`PIPELINE_THRESHOLD`] (DESIGN s11.4.3). Reader -> cpu(hash+encrypt) ->
    /// uploader, connected by BOUNDED [`tokio::sync::mpsc`] channels so the
    /// in-flight memory is capped by backpressure: a 1 GiB file never buffers
    /// more than a handful of [`READ_BUF`] chunks plus one [`WIRE_CHUNK`]
    /// (DESIGN s11.4.6). The three stages are concurrent in-task futures
    /// (`try_join!`) - NOT `'static` spawns - so the reader borrows
    /// `&mut file` and the post-upload identity recheck (P1-1, done by the
    /// caller) re-uses the same handle.
    ///
    /// The exact wire length is PREDICTED up front from `size` (DESIGN s7.1
    /// framing) so the resumable session / `UploadBody::Stream` can declare
    /// `Content-Length` before the bytes are produced. blake3 is over the
    /// plaintext; the md5 is accumulated over the exact bytes sent and
    /// verified against Drive's md5 at the end (SPEC s8). If the local file
    /// changed mid-read so the byte count disagrees with the prediction, the
    /// transfer is abandoned as `ChangedDuringUpload` (the caller's
    /// post-upload fstat also catches it - this just fails fast and cleanly).
    #[allow(clippy::too_many_arguments)]
    async fn stream_upload(
        &self,
        _source: &SourceRow,
        relative_path: &RelativePath,
        size: u64,
        file: &mut tokio::fs::File,
        _pre: FileIdentity,
        existing_file_id: Option<&str>,
        mime: &str,
        target: &RemoteTarget,
        op_id: PendingOpId,
        payload: &mut PendingOpPayload,
        // P1-B: false for a VSS-snapshot read - force the simple streaming path.
        allow_resumable: bool,
    ) -> Result<UploadProduct, UploadError> {
        // Predict the exact number of bytes that will be sent to Drive.
        let total = predicted_sent_len(size, self.crypto.is_some());
        let encrypted = self.crypto.is_some();

        // P1-2: persist the uploaded blake3 once it is known. With streaming
        // the hash is produced DURING the upload, so we persist it after the
        // pipeline completes but before commit. A crash before this point
        // leaves either no finalized object (resumable Create commits only on
        // the final chunk -> reconcile drops the op) or a finalized orphan
        // with no recorded hash -> reconcile requeues it (safe: a redundant
        // re-upload, never a silent adopt of stale bytes). Either way the
        // P1-2 "never adopt-as-Synced without a hash match" invariant holds.

        // Bounded channels: reader -> cpu (plaintext chunks), cpu -> uploader
        // (output chunks). Small caps so backpressure bounds the memory.
        let (raw_tx, raw_rx) = tokio::sync::mpsc::channel::<Bytes>(PIPELINE_CHANNEL_CAP);
        let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Bytes>(PIPELINE_CHANNEL_CAP);

        // The pacer's byte bucket is applied in the READER stage, before each
        // chunk is read/emitted, so the bandwidth cap + backpressure are at
        // the source of the data flow (SPEC s8 / DESIGN s5.4).
        let pacer = self.pacer.clone();
        let reader = read_stage(file, size, pacer, raw_tx, self.mem_gauge.clone());
        let cpu = cpu_stage(raw_rx, out_tx, self.crypto.clone(), size);
        let uploader = self.upload_stage(
            target,
            existing_file_id,
            mime,
            total,
            encrypted,
            out_rx,
            op_id,
            payload,
            allow_resumable,
        );

        // Run all three concurrently. The cpu stage returns (blake3, md5);
        // the uploader returns the entry. `try_join!` short-circuits on the
        // first error and drops the others (closing channels, unblocking
        // peers).
        let (read_res, cpu_res, entry) = tokio::join!(reader, cpu, uploader);
        // PRECEDENCE (advisor): the uploader's Drive-side error is the real
        // failure and must win. When the uploader errors mid-stream it drops
        // `out_rx`, so the cpu stage's `out_tx.send` fails and the cpu stage
        // returns `StageError::DownstreamGone` - an ARTIFACT of the uploader
        // error, not a real local change. So surface the uploader error
        // FIRST; only then consider a genuine reader/cpu error (identity /
        // size / crypto), and never let a `DownstreamGone` artifact mask the
        // upload failure.
        let entry = entry?;
        read_res.map_err(stage_err_to_upload)?;
        let CpuOutput { blake3, md5 } = cpu_res.map_err(stage_err_to_upload)?;

        // md5 verify over the exact bytes sent (SPEC s8).
        match entry.md5 {
            Some(remote) if remote == md5 => {
                self.pacer.note_response(ResponseClass::Ok);
            }
            _ => {
                warn!(
                    target: TARGET,
                    path = %relative_path,
                    "streamed md5 mismatch: remote {:?} vs local {:?}",
                    entry.md5,
                    md5
                );
                // P1-4: the just-finalized object is corrupt (its bytes do not
                // match what we sent). For a CREATE it is a brand-new orphan
                // carrying this op's uuid; leaving it would let reconcile
                // adopt it as Synced (its local-vs-uploaded hash still agrees)
                // and strand corrupt bytes on Drive, while the next scan
                // uploads a second copy. Trash it before failing so the
                // failure leaves NO object behind. Never trash on an UPDATE -
                // that id is the user's pre-existing file.
                self.trash_corrupt_create(existing_file_id, &entry.id, relative_path.as_str())
                    .await;
                return Err(UploadError::Failed(ErrorCode::DriveChecksumMismatch));
            }
        }

        // Persist the now-known plaintext blake3 (P1-2) before the caller
        // commits the row.
        payload.uploaded_blake3_hex = Some(hex::encode(blake3));
        self.persist_payload(op_id, payload)
            .await
            .map_err(UploadError::Fatal)?;

        Ok(UploadProduct { blake3, entry })
    }

    /// The uploader stage of the streaming pipeline. Drains the cpu stage's
    /// output channel, accumulates it into [`WIRE_CHUNK`] wire chunks (each a
    /// 256-KiB multiple except the final one, SPEC s3), and pushes them to
    /// Drive. Files at or above [`RESUMABLE_THRESHOLD`] use the resumable
    /// session protocol (persisting acked offsets for P1-3 resume); the
    /// 4-5 MiB simple band hands the channel straight to `create`/`update`
    /// via [`UploadBody::Stream`].
    #[allow(clippy::too_many_arguments)]
    async fn upload_stage(
        &self,
        target: &RemoteTarget,
        existing_file_id: Option<&str>,
        mime: &str,
        total: u64,
        _encrypted: bool,
        out_rx: tokio::sync::mpsc::Receiver<Bytes>,
        op_id: PendingOpId,
        payload: &mut PendingOpPayload,
        // P1-B: false for a VSS-snapshot read - never open a resumable session,
        // stream as a single simple upload even above RESUMABLE_THRESHOLD.
        allow_resumable: bool,
    ) -> Result<RemoteEntry, UploadError> {
        if allow_resumable && total >= RESUMABLE_THRESHOLD {
            self.upload_stage_resumable(
                target,
                existing_file_id,
                mime,
                total,
                out_rx,
                op_id,
                payload,
            )
            .await
        } else {
            self.upload_stage_simple(target, existing_file_id, mime, total, out_rx)
                .await
        }
    }

    /// Simple-band streaming upload (4 MiB <= size < 5 MiB): hand the cpu
    /// output channel directly to `create`/`update` as an [`UploadBody::Stream`].
    async fn upload_stage_simple(
        &self,
        target: &RemoteTarget,
        existing_file_id: Option<&str>,
        mime: &str,
        total: u64,
        out_rx: tokio::sync::mpsc::Receiver<Bytes>,
    ) -> Result<RemoteEntry, UploadError> {
        use futures::StreamExt;
        let stream = tokio_stream::wrappers::ReceiverStream::new(out_rx).map(Ok);
        let body = UploadBody::Stream {
            len: total,
            stream: Box::new(stream),
        };
        let result = if let Some(file_id) = existing_file_id {
            self.pacer.permit_request().await;
            self.remote
                .update(file_id, body, target.app_props.clone())
                .await
        } else {
            self.pacer.permit_file_create().await;
            self.remote
                .create(
                    &target.parent_id,
                    &target.name,
                    mime,
                    body,
                    target.app_props.clone(),
                )
                .await
        };
        result.map_err(|e| {
            let class = classify_drive_error(&e);
            self.pacer.note_response(class.response_class());
            // P1-3: the simple-band (4-5 MiB) CREATE is also a single POST -
            // an ambiguous transient (network/5xx) may have committed the
            // object before the response was lost. Defer to reconcile (keep
            // the op) instead of failing + dropping the op (which would both
            // strand the orphan and lose the recovery handle). Updates are
            // idempotent and rate-limits were rejected, so only a transient
            // create defers; everything else fails as before.
            if existing_file_id.is_none() && matches!(class, DriveError::Transient) {
                UploadError::DeferToReconcile(class.error_code())
            } else {
                UploadError::Failed(class.error_code())
            }
        })
    }

    /// Resumable streaming upload (size >= 5 MiB): open a resumable session,
    /// then accumulate the cpu output channel into [`WIRE_CHUNK`] wire chunks
    /// and push them, persisting acked offsets (P1-3). On a session 4xx the
    /// session restarts from offset 0 - but the cpu output channel can only
    /// be consumed ONCE, so a streaming restart would need a re-read. We
    /// surface the (rare) streamed-session-invalidation as a per-op failure
    /// that the next scan retries from scratch, rather than buffering the
    /// whole file to replay (which would defeat the memory bound).
    #[allow(clippy::too_many_arguments)]
    async fn upload_stage_resumable(
        &self,
        target: &RemoteTarget,
        existing_file_id: Option<&str>,
        mime: &str,
        total: u64,
        mut out_rx: tokio::sync::mpsc::Receiver<Bytes>,
        op_id: PendingOpId,
        payload: &mut PendingOpPayload,
    ) -> Result<RemoteEntry, UploadError> {
        let session = self
            .open_resumable_session(target, existing_file_id, mime, total, op_id, payload)
            .await
            .map_err(UploadError::Fatal)?;

        let mut acc: Vec<u8> = Vec::with_capacity(WIRE_CHUNK);
        let mut offset: u64 = 0;
        let mut produced: u64 = 0;

        // Drain the cpu output, flushing full WIRE_CHUNKs as they fill.
        loop {
            let chunk = out_rx.recv().await;
            match chunk {
                Some(bytes) => {
                    produced += bytes.len() as u64;
                    acc.extend_from_slice(&bytes);
                    // Flush every full wire chunk, but never the byte that
                    // would make this the final chunk - we only know a chunk
                    // is final once the channel closes, and a final chunk may
                    // be any size while non-final ones must be 256-KiB
                    // multiples. So hold back at least one wire chunk's worth
                    // until EOF.
                    while acc.len() >= 2 * WIRE_CHUNK {
                        let wire = Bytes::copy_from_slice(&acc[..WIRE_CHUNK]);
                        acc.drain(..WIRE_CHUNK);
                        match self
                            .push_one_wire_chunk(&session, offset, wire, op_id, payload)
                            .await?
                        {
                            PushOne::Acked(new_off) => offset = new_off,
                            PushOne::Done(entry) => {
                                // Final-chunk completion mid-stream means the
                                // declared size was already reached; the
                                // remaining channel data would overshoot.
                                return self
                                    .finish_streamed(entry, produced, total, payload, op_id)
                                    .await;
                            }
                            PushOne::Invalid => {
                                return Err(UploadError::Failed(ErrorCode::DriveUnreachable));
                            }
                        }
                    }
                }
                None => break,
            }
        }

        // EOF: flush whatever remains in `acc` as a tail of >=1 wire chunks,
        // the last of which is the (any-size) final chunk.
        while !acc.is_empty() {
            let take = acc.len().min(WIRE_CHUNK);
            let is_final = take == acc.len();
            // A non-final chunk MUST be a 256-KiB multiple; if the remaining
            // tail is not a multiple and is NOT final, trim to a multiple so
            // the leftover joins the final chunk.
            let take = if is_final {
                take
            } else {
                (take / CHUNK_MULTIPLE) * CHUNK_MULTIPLE
            };
            let wire = Bytes::copy_from_slice(&acc[..take]);
            acc.drain(..take);
            match self
                .push_one_wire_chunk(&session, offset, wire, op_id, payload)
                .await?
            {
                PushOne::Acked(new_off) => offset = new_off,
                PushOne::Done(entry) => {
                    return self
                        .finish_streamed(entry, produced, total, payload, op_id)
                        .await;
                }
                PushOne::Invalid => {
                    return Err(UploadError::Failed(ErrorCode::DriveUnreachable));
                }
            }
        }

        // The channel closed without a Completed - the produced byte count
        // disagreed with the declared session size (the local file changed
        // mid-read). Treat as changed-during-upload: the session never
        // finalized, so no object materialized.
        Err(UploadError::Skip(SkipReason::ChangedDuringUpload))
    }

    /// Push exactly one wire chunk to a resumable session and persist the new
    /// acked offset (P1-3). Maps Drive errors to a fatal/abort path. The
    /// pacer byte bucket was already spent in the reader stage (one bucket
    /// charge per byte), so only the per-request gate is taken here.
    async fn push_one_wire_chunk(
        &self,
        session: &ResumableSession,
        offset: u64,
        wire: Bytes,
        op_id: PendingOpId,
        payload: &mut PendingOpPayload,
    ) -> Result<PushOne, UploadError> {
        let wire_len = wire.len() as u64;
        self.pacer.permit_request().await;
        let progress = self
            .remote
            .resume_chunk(session, offset, wire)
            .await
            .map_err(UploadError::Fatal)?;
        // Drive accepted the chunk: it has left the in-flight buffers (test
        // instrumentation; matches the reader stage's `add`).
        if let Some(g) = self.mem_gauge.as_ref() {
            g.sub(wire_len);
        }
        match progress {
            ResumeProgress::Completed(entry) => Ok(PushOne::Done(entry)),
            ResumeProgress::InProgress { received } => {
                if let Some(r) = payload.resumable.as_mut() {
                    r.acked_offset = received;
                }
                self.persist_payload(op_id, payload)
                    .await
                    .map_err(UploadError::Fatal)?;
                Ok(PushOne::Acked(received))
            }
            ResumeProgress::SessionInvalid => Ok(PushOne::Invalid),
        }
    }

    /// Finish a streamed resumable upload that completed: verify the produced
    /// byte count matched the declared size and clear the persisted session.
    async fn finish_streamed(
        &self,
        entry: RemoteEntry,
        produced: u64,
        total: u64,
        payload: &mut PendingOpPayload,
        op_id: PendingOpId,
    ) -> Result<RemoteEntry, UploadError> {
        if produced != total {
            // Unreachable in practice: Drive/the fake only returns Completed
            // once received == the declared session size, so produced ==
            // total here. Defensive: if the object DID finalize with the
            // wrong byte count it is a finalized orphan, so use
            // SkipPostUpload (keep the create op for reconcile to adopt +
            // re-hash) rather than Skip (which would strand it).
            return Err(UploadError::SkipPostUpload(SkipReason::ChangedDuringUpload));
        }
        payload.resumable = None;
        self.persist_payload(op_id, payload)
            .await
            .map_err(UploadError::Fatal)?;
        Ok(entry)
    }

    /// Upload an in-memory body, retrying transient errors and verifying the
    /// returned md5 against the local md5 of the exact bytes sent. Chooses
    /// the simple multipart path below [`RESUMABLE_THRESHOLD`] and the
    /// resumable protocol at or above it. Uploads to `target` (the resolved
    /// parent folder + name; the encrypted ciphertext path for an encrypted
    /// source, the flat plaintext name otherwise - P1-5).
    #[allow(clippy::too_many_arguments)]
    async fn upload_bytes(
        &self,
        target: &RemoteTarget,
        existing_file_id: Option<&str>,
        sent: SentBytes,
        mime: &str,
        op_id: PendingOpId,
        payload: &mut PendingOpPayload,
        // P1-B: false for a VSS-snapshot read - never open a resumable session,
        // upload as a single simple multipart even above RESUMABLE_THRESHOLD.
        allow_resumable: bool,
    ) -> Result<RemoteEntry, UploadError> {
        let total = sent.bytes.len() as u64;
        let use_resumable = allow_resumable && total >= RESUMABLE_THRESHOLD;
        let mut transient_retries = 0u32;
        loop {
            // A simple-multipart CREATE is the one ambiguous case: a single
            // POST creates the object, so a network drop / timeout AFTER Drive
            // committed it but BEFORE the response arrived leaves a real
            // orphan, and an inline re-POST would duplicate it (P1-3). The
            // resumable CREATE path finalizes only on its final chunk, so a
            // mid-stream transient leaves no object - safe to retry inline.
            // Updates are idempotent (same file_id) - safe either way. A VSS
            // (non-resumable) read above the threshold is a simple create too,
            // so it is ambiguous in the same way - keep the op for reconcile.
            let ambiguous_simple_create = !use_resumable && existing_file_id.is_none();
            let result = if use_resumable {
                self.upload_resumable(target, existing_file_id, &sent, mime, op_id, payload)
                    .await
            } else {
                self.upload_simple(target, existing_file_id, &sent, mime)
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
                                name = %target.name,
                                "md5 mismatch: remote {:?} vs local {:?}",
                                entry.md5,
                                sent.md5
                            );
                            // P1-4: trash a corrupt CREATE before failing so
                            // no bad object is stranded (see
                            // `trash_corrupt_create`). Never on an UPDATE.
                            self.trash_corrupt_create(existing_file_id, &entry.id, &target.name)
                                .await;
                            return Err(UploadError::Failed(ErrorCode::DriveChecksumMismatch));
                        }
                    }
                }
                Err(e) => match self.classify_retry(
                    &e,
                    &mut transient_retries,
                    ambiguous_simple_create,
                )? {
                    RetryDecision::Retry => continue,
                    RetryDecision::Fail(code) => return Err(UploadError::Failed(code)),
                    RetryDecision::DeferCreate(code) => {
                        return Err(UploadError::DeferToReconcile(code))
                    }
                },
            }
        }
    }

    /// Simple multipart create/update for files below
    /// [`RESUMABLE_THRESHOLD`].
    async fn upload_simple(
        &self,
        target: &RemoteTarget,
        existing_file_id: Option<&str>,
        sent: &SentBytes,
        mime: &str,
    ) -> anyhow::Result<RemoteEntry> {
        let body = UploadBody::Bytes(sent.bytes.clone());
        if let Some(file_id) = existing_file_id {
            self.pacer.permit_request().await;
            self.remote
                .update(file_id, body, target.app_props.clone())
                .await
        } else {
            self.pacer.permit_file_create().await;
            self.remote
                .create(
                    &target.parent_id,
                    &target.name,
                    mime,
                    body,
                    target.app_props.clone(),
                )
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
    async fn upload_resumable(
        &self,
        target: &RemoteTarget,
        existing_file_id: Option<&str>,
        sent: &SentBytes,
        mime: &str,
        op_id: PendingOpId,
        payload: &mut PendingOpPayload,
    ) -> anyhow::Result<RemoteEntry> {
        let total = sent.bytes.len() as u64;
        let mut restarts = 0u32;
        loop {
            let session = self
                .open_resumable_session(target, existing_file_id, mime, total, op_id, payload)
                .await?;

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
                    if !self
                        .restart_resumable(&target.name, op_id, payload, &mut restarts)
                        .await?
                    {
                        anyhow::bail!("drive.resumable_session_invalid: exhausted restarts");
                    }
                    continue;
                }
            }
        }
    }

    /// Open a fresh resumable session against `target` and persist it (at
    /// offset 0) into the op payload BEFORE any bytes are pushed, so a crash
    /// after the first chunk lands can resume (P1-3). Shared by the buffered
    /// [`Self::upload_resumable`] and the streaming [`Self::stream_upload`].
    async fn open_resumable_session(
        &self,
        target: &RemoteTarget,
        existing_file_id: Option<&str>,
        mime: &str,
        total: u64,
        op_id: PendingOpId,
        payload: &mut PendingOpPayload,
    ) -> anyhow::Result<ResumableSession> {
        let kind = if let Some(file_id) = existing_file_id {
            ResumableKind::Update {
                file_id: file_id.to_string(),
            }
        } else {
            ResumableKind::Create {
                parent_id: target.parent_id.clone(),
                name: target.name.clone(),
                app_properties: target.app_props.clone(),
            }
        };
        if existing_file_id.is_some() {
            self.pacer.permit_request().await;
        } else {
            self.pacer.permit_file_create().await;
        }
        let session = self.remote.resumable_session(kind, mime, total).await?;
        payload.resumable = Some(PersistedResumable {
            session: session.clone(),
            acked_offset: 0,
        });
        self.persist_payload(op_id, payload).await?;
        Ok(session)
    }

    /// A resumable session was invalidated (4xx). Clear it from the payload
    /// and decide whether to restart from offset 0 (DESIGN s5.4: never reuse
    /// a 4xx-d session), bounded by [`MAX_SESSION_RESTARTS`]. Returns `true`
    /// to retry, `false` when the restart budget is exhausted.
    async fn restart_resumable(
        &self,
        name: &str,
        op_id: PendingOpId,
        payload: &mut PendingOpPayload,
        restarts: &mut u32,
    ) -> anyhow::Result<bool> {
        payload.resumable = None;
        self.persist_payload(op_id, payload).await?;
        *restarts += 1;
        if *restarts > MAX_SESSION_RESTARTS {
            return Ok(false);
        }
        warn!(
            target: TARGET,
            name = %name,
            restarts = *restarts,
            "resumable session invalidated; restarting from offset 0"
        );
        Ok(true)
    }

    /// Classify a Drive-side error from an upload attempt into a retry
    /// decision, ticking the pacer and the transient-retry counter. Shared
    /// by the buffered and streaming upload loops so both honour the same
    /// `5xx -> backoff, max 6 retries` / `4xx -> fail` policy (DESIGN s5.4).
    ///
    /// `ambiguous_create` is `true` only for a simple-multipart CREATE, whose
    /// single POST may have committed the object before the response was lost
    /// (P1-3); on a transient error there we defer to reconcile instead of
    /// re-POSTing (which would duplicate).
    fn classify_retry(
        &self,
        e: &anyhow::Error,
        transient_retries: &mut u32,
        ambiguous_create: bool,
    ) -> Result<RetryDecision, UploadError> {
        let class = classify_drive_error(e);
        self.pacer.note_response(class.response_class());
        match class {
            // P2-8: rate limits (429 / userRateLimitExceeded) retry
            // INDEFINITELY under the pacer's backoff (`note_response` above
            // already halved the ceiling + scheduled the wait); they do NOT
            // share the finite 5xx transient cap. A permanently-throttled
            // Drive loops here by design, paced ever slower, rather than
            // giving up after a handful of attempts and stranding the upload.
            // A rate-limited request was REJECTED (no object created), so it
            // is unambiguous even for a create - retry inline.
            DriveError::RateLimited => Ok(RetryDecision::Retry),
            // 5xx / lower-level transient errors are bounded: retry with
            // backoff up to MAX_TRANSIENT_RETRIES, then surface a failure.
            DriveError::Transient => {
                // P1-3: on an AMBIGUOUS simple-create the object may already
                // exist; defer to reconcile rather than re-POST a duplicate.
                if ambiguous_create {
                    return Ok(RetryDecision::DeferCreate(class.error_code()));
                }
                *transient_retries += 1;
                if *transient_retries > MAX_TRANSIENT_RETRIES {
                    return Ok(RetryDecision::Fail(class.error_code()));
                }
                Ok(RetryDecision::Retry)
            }
            DriveError::QuotaExhausted
            | DriveError::DailyQuota
            | DriveError::InvalidGrant
            | DriveError::DestFolderMissing
            | DriveError::DestFolderPermissionDenied
            | DriveError::Other => Ok(RetryDecision::Fail(class.error_code())),
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

    /// P1-5: resolve the Drive destination for `relative_path` under
    /// `source`, carrying the canonical `app_props` (built by the caller with
    /// the op_uuid).
    ///
    /// - **Plaintext source**: the file lands flat under the source's
    ///   `drive_folder_id` with its plaintext final component as the name
    ///   (the pre-existing M3 behaviour; nested plaintext folders are not in
    ///   scope here).
    /// - **Encrypted source** (DESIGN s7): every path component is encrypted
    ///   independently via [`SourceCryptoSuite::encrypt_filename`], chaining
    ///   each child under its parent's ciphertext as AEAD AAD. The encrypted
    ///   parent directory components are `ensure_folder`ed on Drive so the
    ///   object lands under the CIPHERTEXT path - the plaintext name never
    ///   leaves the machine. The slash-joined encrypted path is returned as
    ///   `encrypted_remote_path` for the `file_state` row; the plaintext
    ///   `relative_path` stays only in local state / the restore UI.
    async fn resolve_remote_target(
        &self,
        source: &SourceRow,
        relative_path: &RelativePath,
        app_props: HashMap<String, String>,
    ) -> anyhow::Result<RemoteTarget> {
        let Some(crypto) = self.crypto.as_deref() else {
            // Plaintext: flat under the source root with the plaintext name.
            return Ok(RemoteTarget {
                parent_id: source.drive_folder_id.clone(),
                name: filename_of(relative_path),
                app_props,
                encrypted_remote_path: None,
            });
        };

        // Encrypted: ensure_folder the encrypted parent chain, then encrypt
        // the leaf under the deepest folder's ciphertext AAD.
        let EncryptedParents {
            parent_id,
            parent_aad,
            mut encrypted_components,
            leaf,
        } = self
            .ensure_encrypted_parents(source, relative_path, crypto)
            .await?;

        let enc_leaf = crypto
            .encrypt_filename(&leaf, &parent_aad)
            .map_err(|e| anyhow::anyhow!("filename encrypt failed for the leaf: {e}"))?;
        encrypted_components.push(enc_leaf.clone());

        Ok(RemoteTarget {
            parent_id,
            name: enc_leaf,
            app_props,
            encrypted_remote_path: Some(encrypted_components.join("/")),
        })
    }

    /// Re-derive (read path) the Drive folder id the orphan of
    /// `relative_path` would live directly under, for the reconcile create
    /// path (P1-5 / Cluster-A no-duplicate). For a plaintext source this is
    /// the source root; for an encrypted source it is the deepest encrypted
    /// parent folder, re-derived via the same idempotent `ensure_folder`
    /// chain `resolve_remote_target` used on the original upload (so the
    /// `find_by_op_uuid` search hits the right parent, not the root).
    async fn reconcile_parent_id(
        &self,
        source: &SourceRow,
        relative_path: &RelativePath,
    ) -> anyhow::Result<String> {
        match self.crypto.as_deref() {
            None => Ok(source.drive_folder_id.clone()),
            Some(crypto) => {
                let EncryptedParents { parent_id, .. } = self
                    .ensure_encrypted_parents(source, relative_path, crypto)
                    .await?;
                Ok(parent_id)
            }
        }
    }

    /// P2-6: re-derive the `encrypted_remote_path` for `relative_path` during
    /// reconcile so an adopted/requeued row restores the same ciphertext path
    /// the original upload stored (crash recovery must not blank it, since
    /// restore and listing key off it). Returns `None` for a plaintext source (the
    /// column is genuinely empty there) and the slash-joined ciphertext path
    /// for an encrypted source, derived via the SAME idempotent
    /// `ensure_encrypted_parents` chain + leaf encryption as the upload
    /// (`resolve_remote_target`), so it is byte-identical to what was written.
    async fn reconcile_encrypted_remote_path(
        &self,
        source: &SourceRow,
        relative_path: &RelativePath,
    ) -> anyhow::Result<Option<String>> {
        let Some(crypto) = self.crypto.as_deref() else {
            return Ok(None);
        };
        let EncryptedParents {
            parent_aad,
            mut encrypted_components,
            leaf,
            ..
        } = self
            .ensure_encrypted_parents(source, relative_path, crypto)
            .await?;
        let enc_leaf = crypto
            .encrypt_filename(&leaf, &parent_aad)
            .map_err(|e| anyhow::anyhow!("filename encrypt failed for the leaf: {e}"))?;
        encrypted_components.push(enc_leaf);
        Ok(Some(encrypted_components.join("/")))
    }

    /// Walk the directory components of `relative_path`, encrypting each
    /// under its parent's ciphertext AAD and `ensure_folder`ing it on Drive
    /// (idempotent), returning the deepest folder id, the AAD to bind the
    /// leaf under, the encrypted directory components, and the plaintext
    /// leaf. Shared by `resolve_remote_target` (upload) and
    /// `reconcile_parent_id` (recovery).
    async fn ensure_encrypted_parents(
        &self,
        source: &SourceRow,
        relative_path: &RelativePath,
        crypto: &dyn SourceCryptoSuite,
    ) -> anyhow::Result<EncryptedParents> {
        let components: Vec<&str> = relative_path
            .as_str()
            .split('/')
            .filter(|c| !c.is_empty())
            .collect();
        // RelativePath is validated non-empty with a real final component;
        // this is the SPEC s0 trivially-unreachable carve-out.
        let (leaf, dirs) = components
            .split_last()
            .ok_or_else(|| anyhow::anyhow!("relative_path has no components: {relative_path}"))?;

        let mut parent_id = source.drive_folder_id.clone();
        // The parent ciphertext name bound in as AEAD AAD (empty at the root,
        // DESIGN s7.1). Tracks the deepest folder's ciphertext name.
        let mut parent_aad: Vec<u8> = Vec::new();
        let mut encrypted_components: Vec<String> = Vec::with_capacity(components.len());

        for dir in dirs {
            let enc_name = crypto
                .encrypt_filename(dir, &parent_aad)
                .map_err(|e| anyhow::anyhow!("filename encrypt failed for a directory: {e}"))?;
            self.pacer.permit_request().await;
            let folder = self.remote.ensure_folder(&parent_id, &enc_name).await?;
            self.pacer.note_response(ResponseClass::Ok);
            parent_id = folder.id;
            parent_aad = enc_name.as_bytes().to_vec();
            encrypted_components.push(enc_name);
        }

        Ok(EncryptedParents {
            parent_id,
            parent_aad,
            encrypted_components,
            leaf: leaf.to_string(),
        })
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

    /// P1-4: trash a corrupt object that a CREATE upload just finalized with
    /// a checksum mismatch, so the failure leaves NOTHING behind on Drive.
    ///
    /// Scoped to creates: `existing_file_id.is_none()`. For an UPDATE the
    /// returned `file_id` is the user's PRE-EXISTING object - we must never
    /// trash it on a mismatch (the prior good bytes stay put; the op is
    /// dropped and the next scan retries the update). For a CREATE the object
    /// is a brand-new orphan carrying this op's uuid; if left in place,
    /// reconcile would adopt it as Synced (local-vs-uploaded blake3 still
    /// agrees - only the on-wire md5 disagreed) and corrupt bytes would
    /// persist while the next scan uploads a duplicate. Trashing it is
    /// best-effort: a failure here is logged, not propagated (the upload has
    /// already failed; a left-behind orphan is caught by a later reconcile).
    async fn trash_corrupt_create(
        &self,
        existing_file_id: Option<&str>,
        file_id: &str,
        context: &str,
    ) {
        if existing_file_id.is_some() {
            return;
        }
        self.pacer.permit_request().await;
        match self.remote.trash(file_id).await {
            Ok(()) => {
                self.pacer.note_response(ResponseClass::Ok);
                warn!(
                    target: TARGET,
                    name = %context,
                    file_id = %file_id,
                    "trashed the corrupt create object after md5 mismatch"
                );
            }
            Err(e) => {
                let class = classify_drive_error(&e);
                self.pacer.note_response(class.response_class());
                warn!(
                    target: TARGET,
                    name = %context,
                    file_id = %file_id,
                    "failed to trash corrupt create object after md5 mismatch: {e}"
                );
            }
        }
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
            //
            // P1-B: a VSS-snapshot read NEVER persists `payload.resumable` (it
            // is forced down the simple, non-resumable path), so this block is
            // skipped for it - reconcile never tries to resume a VSS op against
            // the live file (the snapshot's frozen bytes are already gone). It
            // falls straight to adopt-or-requeue, which cleanly re-enqueues a
            // fresh op the next cycle re-snapshots + re-uploads from scratch.
            if let Some(resumable) = payload.resumable.clone() {
                match self
                    .resume_persisted(source, &op, &payload, resumable)
                    .await?
                {
                    Some((entry, resumed_blake3)) => {
                        // P1-2 trap: the streaming-crash payload carries no
                        // `uploaded_blake3_hex`, so adopt would otherwise hit
                        // its mismatch branch and REQUEUE the object we just
                        // resumed. `resume_persisted` already proved identity
                        // + re-hashed the full stream + verified md5 vs Drive,
                        // so the re-read blake3 IS the proven content hash:
                        // hand it to adopt so the row is marked Synced with
                        // the real hash, not a placeholder.
                        let mut adopted = payload.clone();
                        adopted.uploaded_blake3_hex = Some(hex::encode(resumed_blake3));
                        self.adopt_reconciled(source, &op, &adopted, entry).await?;
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
                // Create path: find the orphaned object by op uuid under its
                // PARENT folder. For an encrypted source the object lives
                // under a nested ciphertext folder, not the source root, so
                // searching the root alone would miss the orphan and leave a
                // duplicate on the next scan (P1-5 must not regress the
                // Cluster-A no-duplicate contract). Re-derive the parent
                // (idempotent `ensure_folder` for the encrypted dir chain).
                let parent_id = self.reconcile_parent_id(source, &op.relative_path).await?;
                match self.remote.find_by_op_uuid(&parent_id, &uuid).await? {
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
    /// P1-2 / P1-3: resume a persisted resumable session after a restart.
    /// Discards the session (returns `None`) when it is older than
    /// [`SESSION_MAX_AGE_MS`], the local file changed, or Drive
    /// 4xx-invalidates it; otherwise it re-reads the local file, re-derives
    /// the exact upload body, and pushes the remaining bytes from the
    /// persisted acked offset. A successful resume returns the finalized
    /// [`RemoteEntry`] together with the plaintext blake3 computed over the
    /// re-read stream (the caller adopts it as the proven content hash, so
    /// the now-Synced row never carries a placeholder).
    ///
    /// P1-2: resume validation does NOT depend on the final content hash -
    /// the streaming pipeline only produces that hash DURING the upload, so a
    /// crash mid-stream leaves none. Instead the session-start IDENTITY
    /// (`resume_identity`: dev/inode/size/mtime/ctime, persisted before the
    /// first chunk) is the gate: re-stat the re-opened handle and require it
    /// matches. With identity proven unchanged, re-reading reproduces the
    /// EXACT bytes already pushed, so the byte-level resume is coherent; the
    /// blake3 over the full re-read stream is the final integrity check (md5
    /// vs Drive). A legacy row that recorded `uploaded_blake3_hex` (the
    /// buffered path) is additionally cross-checked against it.
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
    ) -> anyhow::Result<Option<(RemoteEntry, [u8; 32])>> {
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

        // Re-open the local file and check the resume-safe IDENTITY recorded
        // at session start (P1-2). The identity - not the content hash - is
        // what proves the bytes we are about to re-read are the same ones the
        // crashed run was uploading.
        let full_path = join_source_path(&source.local_path, &op.relative_path);
        let mut file = match open_shared(&full_path).await {
            Ok(f) => f,
            // File gone/locked: cannot resume; let the caller requeue.
            Err(_) => return Ok(None),
        };
        match payload.resume_identity {
            Some(expected) => {
                let cur = match fstat_identity(&file).await {
                    Ok(id) => id,
                    Err(_) => return Ok(None),
                };
                if !expected.matches(&cur) {
                    // Local file changed since the crash: the partial bytes
                    // are stale. Discard the session and requeue a clean
                    // upload (the partial create is GC'd by Drive on expiry).
                    return Ok(None);
                }
            }
            None => {
                // No identity AND no legacy hash to validate against: do not
                // risk a corrupt resume. (A legacy row with only
                // `uploaded_blake3_hex` is still accepted via the hash check
                // below.)
                if payload.uploaded_blake3_hex.is_none() {
                    return Ok(None);
                }
            }
        }

        // Re-derive the exact bytes that were being uploaded + the plaintext
        // blake3 over the full re-read stream (the final integrity check).
        let HashedBody {
            blake3, sent_bytes, ..
        } = match read_hash_encrypt(&mut file, self.crypto.as_deref()).await {
            Ok(h) => h,
            Err(_) => return Ok(None),
        };
        // Legacy cross-check: a row written by the buffered path recorded the
        // uploaded blake3 up front; honour it if present (identity already
        // covered the streaming path).
        if let Some(expected_hex) = payload.uploaded_blake3_hex.as_deref() {
            if hex::encode(blake3) != expected_hex {
                return Ok(None);
            }
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
                    Some(remote) if remote == sent_bytes.md5 => Ok(Some((entry, blake3))),
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

        // P2-6: re-derive the ciphertext remote path so the adopted/requeued
        // row restores the same `encrypted_remote_path` the upload wrote (None
        // for a plaintext source). Crash recovery must preserve it - restore +
        // listing look the object up by this path.
        let encrypted_remote_path = self
            .reconcile_encrypted_remote_path(source, &op.relative_path)
            .await?;

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
                    encrypted_remote_path: encrypted_remote_path.clone(),
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
                    encrypted_remote_path: encrypted_remote_path.clone(),
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
// Streaming pipeline support types + stages (P1-4, DESIGN s11.4.3)
// -----------------------------------------------------------------------------

/// The resolved Drive destination for one upload (P1-5). For a plaintext
/// source this is the source root + the plaintext leaf name; for an
/// encrypted source the parent is the deepest `ensure_folder`ed ciphertext
/// directory and `name` is the leaf's ciphertext, with the full slash-joined
/// ciphertext path captured in `encrypted_remote_path`.
struct RemoteTarget {
    /// Drive folder id the object lives directly under.
    parent_id: String,
    /// The object's Drive display name (ciphertext for encrypted sources).
    name: String,
    /// The canonical `appProperties` (identity + crash-safe op_uuid).
    app_props: HashMap<String, String>,
    /// The slash-joined ciphertext path for an encrypted source; `None` for
    /// plaintext. Persisted in `file_state.encrypted_remote_path`.
    encrypted_remote_path: Option<String>,
}

/// What an upload (buffered or streamed) produced: the plaintext blake3 and
/// the resulting Drive object.
struct UploadProduct {
    blake3: [u8; 32],
    entry: RemoteEntry,
}

/// The result of `ensure_encrypted_parents`: the deepest encrypted parent
/// folder, the AAD to bind the leaf under, the encrypted dir components, and
/// the plaintext leaf component.
struct EncryptedParents {
    parent_id: String,
    parent_aad: Vec<u8>,
    encrypted_components: Vec<String>,
    leaf: String,
}

/// The cpu stage's result: the plaintext blake3 + the md5 over the exact
/// bytes it emitted to the uploader.
struct CpuOutput {
    blake3: [u8; 32],
    md5: [u8; 16],
}

/// Retry decision returned by [`DefaultExecutor::classify_retry`].
enum RetryDecision {
    /// Transient (5xx / network / rate-limit) within the budget; loop again.
    Retry,
    /// Non-retryable, or the transient budget is exhausted; fail with code.
    Fail(ErrorCode),
    /// P1-3: an ambiguous transient error on a simple-multipart CREATE - the
    /// object MIGHT already exist on Drive. Do NOT re-POST inline; defer to
    /// the reconcile pass (keep the op) so a later cycle adopts-or-requeues
    /// without risking a duplicate.
    DeferCreate(ErrorCode),
}

/// Result of pushing one wire chunk to a resumable session.
enum PushOne {
    /// Drive acked up to this offset (exclusive); keep going.
    Acked(u64),
    /// The final chunk completed the upload.
    Done(RemoteEntry),
    /// The session 4xx'd; abort the streamed transfer.
    Invalid,
}

/// Error from the reader / cpu pipeline stages. Distinguishes a local
/// changed/replaced-during-upload skip from a crypto / IO per-op failure so
/// `stage_err_to_upload` can map it to the right [`UploadError`].
enum StageError {
    /// The local file changed/shrank/grew mid-read (byte count disagreed
    /// with the planner's size) -> `ChangedDuringUpload` skip.
    Changed,
    /// A read IO error.
    Io(std::io::Error),
    /// A crypto error in the cpu stage.
    Crypto(CryptoError),
    /// The downstream channel closed while this stage still had data to send:
    /// an ARTIFACT of a downstream (uploader) error, not a real local change.
    /// The caller surfaces the uploader's real error first; this only maps if
    /// it somehow leaks through (it should not).
    DownstreamGone,
}

/// Map a [`StageError`] onto the executor's [`UploadError`].
fn stage_err_to_upload(e: StageError) -> UploadError {
    match e {
        // The bytes may or may not have landed; the caller's post-upload
        // fstat treats a CREATE orphan correctly. Use SkipPostUpload so a
        // create orphan is kept for reconcile (mirrors the inline post-read
        // skip semantics for the streamed window).
        StageError::Changed => UploadError::SkipPostUpload(SkipReason::ChangedDuringUpload),
        StageError::Io(io) => {
            warn!(target: TARGET, error = %io, "streaming read IO error");
            UploadError::Failed(ErrorCode::LocalIoError)
        }
        StageError::Crypto(ce) => UploadError::Failed(crypto_error_code(&ce)),
        // Should be unreachable: the caller surfaces the uploader error
        // first. Map to a transient failure so the op is retried rather than
        // silently swallowed if it ever does surface.
        StageError::DownstreamGone => UploadError::Failed(ErrorCode::DriveUnreachable),
    }
}

/// Predict the EXACT number of bytes that will be sent to Drive for a file
/// of `plaintext_len` plaintext bytes (DESIGN s7.1 framing). Needed up front
/// so the streaming upload can declare its `Content-Length` before producing
/// the bytes.
///
/// - Plaintext: `plaintext_len`.
/// - Encrypted: `HEADER_LEN` (40) + every plaintext byte + a 16-byte
///   Poly1305 tag per 64-KiB plaintext chunk. An empty file still emits one
///   (empty) final chunk, hence `max(1, ...)` chunks.
fn predicted_sent_len(plaintext_len: u64, encrypted: bool) -> u64 {
    if !encrypted {
        return plaintext_len;
    }
    let read_buf = READ_BUF as u64;
    let chunks = plaintext_len.div_ceil(read_buf).max(1);
    driven_crypto::HEADER_LEN as u64 + plaintext_len + chunks * TAG_LEN as u64
}

/// Poly1305 tag length appended to each STREAM chunk's ciphertext
/// (chacha20poly1305). Used by [`predicted_sent_len`].
const TAG_LEN: usize = 16;

/// The READER stage of the streaming pipeline (DESIGN s11.4.3). Reads the
/// open `file` in [`READ_BUF`] (64 KiB) plaintext chunks - the format chunk
/// boundary the content encryptor and the restore decryptor both replay -
/// applying the pacer byte bucket BEFORE each read so the bandwidth cap +
/// backpressure are at the source. Sends each chunk to the cpu stage over
/// the bounded channel. Stops early (no error) if the downstream channel
/// closes. Reads AT MOST `size` bytes and errors `Changed` if the file's
/// byte count disagrees with the planner's `size`.
async fn read_stage(
    file: &mut tokio::fs::File,
    size: u64,
    pacer: Arc<dyn Pacer>,
    raw_tx: tokio::sync::mpsc::Sender<Bytes>,
    mem_gauge: Option<Arc<MemGauge>>,
) -> Result<(), StageError> {
    let mut buf = vec![0u8; READ_BUF];
    let mut read_total: u64 = 0;
    loop {
        let n = file.read(&mut buf).await.map_err(StageError::Io)?;
        if n == 0 {
            break;
        }
        read_total += n as u64;
        if read_total > size {
            // The file grew mid-read; the declared length would be wrong.
            return Err(StageError::Changed);
        }
        pacer.permit_bytes(n as u64).await;
        // Record the bytes entering the pipeline (test instrumentation; the
        // matching `sub` happens once Drive accepts the wire chunk).
        if let Some(g) = mem_gauge.as_ref() {
            g.add(n as u64);
        }
        if raw_tx
            .send(Bytes::copy_from_slice(&buf[..n]))
            .await
            .is_err()
        {
            // Downstream (cpu/uploader) gone - a downstream error aborted the
            // pipeline. The real error surfaces from that stage; this is just
            // an artifact (the caller surfaces the uploader error first).
            return Err(StageError::DownstreamGone);
        }
    }
    if read_total != size {
        // The file shrank mid-read.
        return Err(StageError::Changed);
    }
    Ok(())
}

/// The CPU stage of the streaming pipeline (DESIGN s11.4.3 / s11.4.4).
/// Drains the reader's plaintext chunks, hashing blake3 over the plaintext
/// (`update_rayon` for files above [`RAYON_HASH_THRESHOLD`], multi-core) and,
/// when `crypto` is `Some`, encrypting each 64-KiB plaintext chunk through
/// the [`ContentEncryptor`] (header first, last chunk via `finalize_last`).
/// Accumulates the md5 over the exact bytes it emits and forwards them to the
/// uploader over the bounded channel. Returns the plaintext blake3 + that
/// md5. Reads one chunk ahead so it can flag the final plaintext chunk for
/// the STREAM last-block marker.
async fn cpu_stage(
    mut raw_rx: tokio::sync::mpsc::Receiver<Bytes>,
    out_tx: tokio::sync::mpsc::Sender<Bytes>,
    crypto: Option<Arc<dyn SourceCryptoSuite>>,
    size: u64,
) -> Result<CpuOutput, StageError> {
    use md5::{Digest, Md5};

    let mut hasher = blake3::Hasher::new();
    let use_rayon = size >= RAYON_HASH_THRESHOLD;
    let mut md5 = Md5::new();

    // Hash a plaintext chunk into blake3, multi-core for big files.
    let hash_chunk = |h: &mut blake3::Hasher, chunk: &[u8]| {
        if use_rayon {
            h.update_rayon(chunk);
        } else {
            h.update(chunk);
        }
    };

    if let Some(suite) = crypto {
        let mut enc: Box<dyn ContentEncryptor> = suite.content_encryptor();
        let header = enc.header();
        md5.update(&header);
        if out_tx.send(header).await.is_err() {
            return Err(StageError::DownstreamGone);
        }
        // Read one chunk ahead so the final chunk can be finalized.
        let mut pending: Option<Bytes> = None;
        while let Some(chunk) = raw_rx.recv().await {
            hash_chunk(&mut hasher, &chunk);
            if let Some(prev) = pending.take() {
                let ct = enc.encrypt_chunk(&prev).map_err(StageError::Crypto)?;
                md5.update(&ct);
                if out_tx.send(ct).await.is_err() {
                    return Err(StageError::DownstreamGone);
                }
            }
            pending = Some(chunk);
        }
        let last = pending.unwrap_or_default();
        let (ct, _ct_md5) = enc.finalize_last(&last).map_err(StageError::Crypto)?;
        md5.update(&ct);
        let _ = out_tx.send(ct).await; // downstream may have finished
    } else {
        while let Some(chunk) = raw_rx.recv().await {
            hash_chunk(&mut hasher, &chunk);
            md5.update(&chunk);
            if out_tx.send(chunk).await.is_err() {
                return Err(StageError::DownstreamGone);
            }
        }
    }

    let blake3: [u8; 32] = hasher.finalize().into();
    let md5: [u8; 16] = md5.finalize().into();
    Ok(CpuOutput { blake3, md5 })
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
    /// P1-3: an AMBIGUOUS create failure - a network drop / timeout on a
    /// CREATE POST where Drive MIGHT have created the object even though the
    /// response was lost. Re-POSTing inline would risk a duplicate (and the
    /// search-backed `find_by_op_uuid` can return a false "not found" inside
    /// Drive's index-lag window, so an inline probe cannot disambiguate
    /// safely). So the op is KEPT and the failure surfaces here; the next
    /// reconcile pass (a LATER cycle, outside the lag window) adopts the
    /// object by op-uuid if it exists, else requeues cleanly. Only ever
    /// raised for creates (`existing_file_id.is_none()`); updates are
    /// idempotent and retry inline.
    DeferToReconcile(ErrorCode),
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

/// The outcome of [`DefaultExecutor::open_effective`]: an opened handle plus
/// the path it was opened from (live or VSS-snapshot), or a skip / fail.
enum EffectiveOpen {
    /// The file is open; `read_path` is the path EVERY SPEC s8 identity check
    /// must re-stat (the live path, or the frozen VSS snapshot copy).
    Opened {
        /// The effective path the handle reads (live or snapshot).
        read_path: PathBuf,
        /// The open handle.
        file: tokio::fs::File,
    },
    /// The file is locked and VSS could not help: skip + surface it.
    Skip(SkipReason),
    /// A non-lock IO error opening the file: fail the op.
    Failed(ErrorCode),
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

/// Classify an open error (P2-F): ONLY a Windows sharing/lock violation is a
/// "locked file" (the VSS / `local.file_locked` path). An ACL / access-denied
/// failure is NOT a lock - it is a permission problem the user must fix - so it
/// maps to a plain IO error (`local.io_error`), never `local.vss_unavailable`.
/// We inspect the raw OS error, not just [`std::io::ErrorKind`], because both a
/// sharing violation and an access-denial surface as `PermissionDenied` on
/// Windows, yet only the former is a lock VSS can read around.
fn classify_open_error(e: std::io::Error) -> OpenError {
    // ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33): the file is
    // open exclusively by another process - the genuine "locked" case VSS
    // exists to read around.
    #[cfg(windows)]
    if matches!(e.raw_os_error(), Some(32) | Some(33)) {
        return OpenError::Locked;
    }
    // Everything else - including ERROR_ACCESS_DENIED (5) / an ACL denial that
    // surfaces as `PermissionDenied` - is a plain IO error. Routing an ACL
    // failure through the locked-file/VSS path would mislead the user with
    // `local.vss_unavailable` ("would back up if elevated") when elevation
    // would not help, and conflicts with the stress harness's expectations.
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
                    vss: None,
                },
                self.clock.clone(),
            )
        }

        /// An executor wired with a [`driven_vss::VssProvider`] (M3.5).
        fn executor_with_vss(&self, vss: Arc<dyn driven_vss::VssProvider>) -> DefaultExecutor {
            DefaultExecutor::with_clock(
                ExecutorDeps {
                    remote: Arc::new(self.remote.clone()),
                    state: self.state.clone(),
                    pacer: self.pacer.clone(),
                    crypto: None,
                    vss: Some(vss),
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

        // P1-4: the corrupt CREATE object must NOT be stranded on Drive. The
        // executor trashes it before failing, so no LIVE object remains (if it
        // stayed, reconcile would adopt it as Synced - its local-vs-uploaded
        // blake3 still agrees, only the wire md5 disagreed - and the next scan
        // would upload a duplicate).
        let live = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert!(
            live.is_empty(),
            "corrupt create object trashed, none stranded; got {live:?}"
        );
        // It is present-but-trashed (idempotent trash, not a hard delete).
        let with_trashed = h
            .remote
            .list_folder_with_trashed(h.source.drive_folder_id.as_str());
        assert_eq!(
            with_trashed.iter().filter(|e| e.trashed).count(),
            1,
            "the corrupt object is trashed, not gone"
        );
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

    // --- M3.5 VSS effective-read-path + degrade -----------------------------

    /// Regression for the frozen-vs-live identity trap (the advisor's blocking
    /// finding): in `always` mode the executor reads from the VSS snapshot, so
    /// a LIVE mutation mid-op must NOT trip `ChangedDuringUpload` - the op
    /// completes and uploads the FROZEN bytes. Without the effective-read-path
    /// fix this would compare the frozen stat to the live stat and skip,
    /// meaning an actively-written locked file (the whole VSS use case) would
    /// never back up. Runs on every OS (no real lock needed; the FakeVss maps
    /// reads to a real "snapshot" copy directory).
    #[tokio::test]
    async fn always_mode_reads_frozen_snapshot_despite_live_mutation() {
        let h = harness().await;
        let frozen = b"FROZEN-point-in-time-bytes";
        let (rel, size) = h.write_file("db.dat", frozen);

        // Build a "snapshot" copy dir holding the frozen bytes; the FakeVss
        // maps every read to <snap_dir>/<leaf>.
        let snap_dir = tempfile::tempdir().unwrap();
        std::fs::write(snap_dir.path().join("db.dat"), frozen).unwrap();
        let vss: Arc<dyn driven_vss::VssProvider> =
            Arc::new(driven_vss::FakeVssProvider::mapped_under(
                driven_vss::VssMode::Always,
                snap_dir.path().to_path_buf(),
            ));

        // Mid-upload hook mutates the LIVE file (grows it) - which would trip
        // ChangedDuringUpload if we were stat-ing the live path.
        let src_path = h.tmp_src.path().to_path_buf();
        let hook: MidUploadHook = Arc::new(move |_p: &Path| {
            std::fs::write(
                src_path.join("db.dat"),
                b"LIVE-bytes-MUTATED-and-much-LONGER",
            )
            .unwrap();
        });

        let exec = h.executor_with_vss(vss).with_mid_upload_hook(hook);
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(
            matches!(out[0], OpOutcome::Done { .. }),
            "always-mode snapshot read must complete despite live mutation; got {:?}",
            out[0]
        );

        // The remote got the FROZEN bytes (size of the snapshot copy), not the
        // mutated live bytes.
        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(
            children[0].size,
            Some(frozen.len() as u64),
            "uploaded the frozen snapshot bytes, not the live-mutated ones"
        );
    }

    /// P1-B: a read served from a VSS snapshot is NON-RESUMABLE even when the
    /// file is large enough (>= [`RESUMABLE_THRESHOLD`]) that a live read would
    /// open a resumable session. The per-cycle shadow is released at cycle end,
    /// so a resumable session persisted against the frozen bytes could not be
    /// resumed next cycle (the snapshot is gone) and reconcile must not resume
    /// it against the live file. So the executor forces the simple upload path
    /// and opens ZERO resumable sessions. A live-read CONTROL on the same size
    /// proves the threshold WOULD otherwise trip the resumable path.
    #[tokio::test]
    async fn vss_snapshot_read_is_non_resumable() {
        // A file comfortably above the 5 MiB resumable threshold.
        let big = vec![0xABu8; (RESUMABLE_THRESHOLD + 512 * 1024) as usize];

        // --- CONTROL: a LIVE read of this size DOES open a resumable session.
        {
            let h = harness().await;
            let (rel, size) = h.write_file("big-live.dat", &big);
            let exec = h.executor(); // no VSS => live read
            let out = exec
                .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
                .await
                .unwrap();
            assert!(matches!(out[0], OpOutcome::Done { .. }), "got {:?}", out[0]);
            assert!(
                h.remote.resumable_sessions_opened() >= 1,
                "control: a live read above the threshold must use the resumable path"
            );
        }

        // --- VSS snapshot read of the SAME size opens NO resumable session.
        let h = harness().await;
        let (rel, size) = h.write_file("big.dat", &big);
        // The FakeVss maps every read to <snap_dir>/<leaf>; put a frozen copy
        // there so `always` mode reads the snapshot path.
        let snap_dir = tempfile::tempdir().unwrap();
        std::fs::write(snap_dir.path().join("big.dat"), &big).unwrap();
        let vss: Arc<dyn driven_vss::VssProvider> =
            Arc::new(driven_vss::FakeVssProvider::mapped_under(
                driven_vss::VssMode::Always,
                snap_dir.path().to_path_buf(),
            ));
        let exec = h.executor_with_vss(vss);
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(
            matches!(out[0], OpOutcome::Done { .. }),
            "VSS snapshot read must still complete; got {:?}",
            out[0]
        );
        assert_eq!(
            h.remote.resumable_sessions_opened(),
            0,
            "a VSS snapshot read must NOT open a resumable session (P1-B)"
        );
        // And it committed cleanly (op deleted, row Synced) - no stranded
        // pending op that reconcile could try to resume against the live file.
        let pending = h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap();
        assert!(
            pending.is_empty(),
            "a clean VSS upload must leave no pending op to resume"
        );
        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .expect("file_state row");
        assert_eq!(row.status, FileStateStatus::Synced);
    }

    /// P1-B: when a VSS-snapshot upload FAILS after enqueue, the pending op is
    /// preserved + requeued CLEANLY (no resumable session, no resume identity),
    /// so the next cycle re-snapshots + re-uploads from scratch rather than
    /// trying to resume against bytes that are gone. We drive a checksum
    /// mismatch (a hard create failure) and assert the op carries no resumable
    /// state and the row is not Synced.
    #[tokio::test]
    async fn vss_snapshot_read_failure_requeues_clean() {
        let remote = InMemoryRemoteStore::new().with_md5_mismatch_after(0);
        let h = harness_with_remote(remote).await;
        let big = vec![0x5Au8; (RESUMABLE_THRESHOLD + 256 * 1024) as usize];
        let (rel, size) = h.write_file("big.dat", &big);
        let snap_dir = tempfile::tempdir().unwrap();
        std::fs::write(snap_dir.path().join("big.dat"), &big).unwrap();
        let vss: Arc<dyn driven_vss::VssProvider> =
            Arc::new(driven_vss::FakeVssProvider::mapped_under(
                driven_vss::VssMode::Always,
                snap_dir.path().to_path_buf(),
            ));
        let exec = h.executor_with_vss(vss);
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        // The op did not succeed.
        assert!(
            !matches!(out[0], OpOutcome::Done { .. }),
            "a checksum-mismatch VSS upload must not report Done; got {:?}",
            out[0]
        );
        // No resumable session was ever opened for the snapshot read.
        assert_eq!(
            h.remote.resumable_sessions_opened(),
            0,
            "a failed VSS read must not have opened a resumable session"
        );
        // The row is not committed Synced - the next cycle re-uploads clean.
        let row = h.state.get_file_state(h.source.id, &rel).await.unwrap();
        assert!(
            row.is_none() || row.unwrap().status != FileStateStatus::Synced,
            "a failed VSS upload must not commit the row as Synced"
        );
    }

    /// P2-F: an ACL / access-denied open error is an IO error
    /// (`local.io_error`), NOT a locked file. Only a Windows sharing/lock
    /// violation (ERROR_SHARING_VIOLATION 32 / ERROR_LOCK_VIOLATION 33) is a
    /// lock VSS can read around. We inspect the raw OS error, not just the
    /// `ErrorKind`, because both surface as `PermissionDenied` on Windows.
    #[test]
    fn classify_open_error_distinguishes_lock_from_acl() {
        use std::io::{Error, ErrorKind};

        // A generic PermissionDenied with no lock raw-code => IO error, not a
        // lock. (On Windows an ACL denial is ERROR_ACCESS_DENIED == 5.)
        let acl = Error::new(ErrorKind::PermissionDenied, "access denied");
        assert!(
            matches!(super::classify_open_error(acl), super::OpenError::Io(_)),
            "an ACL/permission-denied open must map to an IO error, not Locked"
        );

        // Explicit ERROR_ACCESS_DENIED (5) => IO error.
        let eacces = Error::from_raw_os_error(5);
        assert!(
            matches!(super::classify_open_error(eacces), super::OpenError::Io(_)),
            "ERROR_ACCESS_DENIED (5) must map to an IO error, not Locked"
        );

        // A sharing/lock violation IS a lock - but only on Windows, where the
        // raw OS error carries that meaning. Off Windows there is no such
        // concept, so the same numeric code is just an IO error.
        let sharing = Error::from_raw_os_error(32);
        let lock = Error::from_raw_os_error(33);
        #[cfg(windows)]
        {
            assert!(
                matches!(
                    super::classify_open_error(sharing),
                    super::OpenError::Locked
                ),
                "ERROR_SHARING_VIOLATION (32) is a locked file on Windows"
            );
            assert!(
                matches!(super::classify_open_error(lock), super::OpenError::Locked),
                "ERROR_LOCK_VIOLATION (33) is a locked file on Windows"
            );
        }
        #[cfg(not(windows))]
        {
            assert!(
                matches!(super::classify_open_error(sharing), super::OpenError::Io(_)),
                "no Windows lock concept off Windows: raw 32 is a plain IO error"
            );
            assert!(
                matches!(super::classify_open_error(lock), super::OpenError::Io(_)),
                "no Windows lock concept off Windows: raw 33 is a plain IO error"
            );
        }
    }

    /// An UNAVAILABLE VSS provider must not disturb a normal (openable) file in
    /// `auto` mode: the live open succeeds, VSS is never consulted, and the
    /// upload commits exactly as with no provider. The degrade-to-skip path for
    /// a genuinely LOCKED file is the Windows-elevated integration test (a real
    /// `ERROR_SHARING_VIOLATION` cannot be produced cross-OS); the pure
    /// decision is table-tested in `driven_vss::fallback_decision`.
    #[tokio::test]
    async fn auto_mode_unavailable_vss_does_not_disturb_openable_file() {
        let h = harness().await;
        let (rel, size) = h.write_file("normal.txt", b"plain readable file");
        let vss: Arc<dyn driven_vss::VssProvider> =
            Arc::new(driven_vss::FakeVssProvider::unavailable());
        let exec = h.executor_with_vss(vss);
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(matches!(out[0], OpOutcome::Done { .. }), "got {:?}", out[0]);
        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .expect("file_state row");
        assert_eq!(row.status, FileStateStatus::Synced);
    }

    /// REAL VSS locked-file integration test (ROADMAP M3.5 acceptance).
    ///
    /// Opens a file with `CREATE_NEW | GENERIC_WRITE | share=0` (NO sharing) so
    /// a normal `open_shared` hits `ERROR_SHARING_VIOLATION`, then runs a sync
    /// with the REAL [`driven_vss::RealVssProvider`] and asserts the bytes land
    /// on the fake remote - read via the VSS snapshot.
    ///
    /// Honestly GATE-SKIPPED when the process is not elevated (VSS snapshot
    /// creation needs Administrator): CI is non-elevated, so this prints a SKIP
    /// reason and returns rather than failing - it is NOT `#[ignore]`-faked. A
    /// local elevated `cargo test` exercises the real COM path end-to-end.
    #[cfg(windows)]
    #[tokio::test]
    async fn locked_file_backs_up_via_real_vss_snapshot() {
        if !driven_vss::is_elevated() {
            eprintln!(
                "SKIP locked_file_backs_up_via_real_vss_snapshot: process is not elevated; \
                 VSS snapshot creation requires Administrator. Run an elevated `cargo test` \
                 to exercise the real COM path (CI is non-elevated by design)."
            );
            return;
        }

        use std::os::windows::fs::OpenOptionsExt;

        let h = harness().await;
        let contents = b"locked-outlook-pst-like-bytes-that-must-still-back-up";
        let (rel, size) = h.write_file("locked.dat", contents);
        let live = h.tmp_src.path().join("locked.dat");

        // Open the file with NO sharing + write access so any reader gets
        // ERROR_SHARING_VIOLATION. Hold the handle for the whole sync.
        const GENERIC_WRITE: u32 = 0x4000_0000;
        let _exclusive = std::fs::OpenOptions::new()
            .access_mode(GENERIC_WRITE)
            .share_mode(0) // no FILE_SHARE_* => exclusive lock
            .write(true)
            .open(&live)
            .expect("open locked.dat exclusively");

        // Sanity: a plain shared open must now fail with a lock.
        assert!(
            matches!(
                super::open_shared(&live).await,
                Err(super::OpenError::Locked)
            ),
            "test setup: file must be locked"
        );

        let vss: Arc<dyn driven_vss::VssProvider> =
            Arc::new(driven_vss::RealVssProvider::new(driven_vss::VssMode::Auto));
        assert!(vss.available(), "elevated provider should be available");
        let exec = h.executor_with_vss(vss);
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(
            matches!(out[0], OpOutcome::Done { .. }),
            "locked file must back up via VSS; got {:?}",
            out[0]
        );
        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].size, Some(size));
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

    // --- P1-4: predicted_sent_len must match the REAL crypto framing --------

    /// The streaming pipeline declares its `Content-Length` (the resumable
    /// session size / `UploadBody::Stream { len }`) BEFORE producing the
    /// bytes, from [`predicted_sent_len`]. If that prediction is off by even
    /// one Poly1305 tag the fake rejects the upload (length mismatch). Prove
    /// the formula matches the real `DrivenCryptoSuite` framing across the
    /// boundary cases (empty, exact-READ_BUF multiple, READ_BUF+1, multi).
    #[test]
    fn predicted_sent_len_matches_real_crypto_framing() {
        use driven_crypto::{DrivenCryptoSuite, SourceCryptoSuite as _};

        // Unencrypted is trivially the plaintext length.
        for n in [0u64, 1, READ_BUF as u64, READ_BUF as u64 + 1, 5_000_003] {
            assert_eq!(predicted_sent_len(n, false), n);
        }
        // Empty encrypted file: header(40) + 0 plaintext + 1 final tag(16).
        assert_eq!(predicted_sent_len(0, true), 40 + 16);

        let suite = DrivenCryptoSuite::new(driven_crypto::key::SourceKey::generate());
        // Drive the real encryptor exactly as the cpu stage does (64 KiB
        // plaintext chunks, last via finalize_last) and compare the byte
        // count to the prediction.
        for plaintext_len in [
            0usize,
            1,
            READ_BUF,
            READ_BUF + 1,
            2 * READ_BUF,
            3 * READ_BUF + 777,
        ] {
            let plaintext = vec![0x42u8; plaintext_len];
            let mut enc = suite.content_encryptor();
            let mut produced = enc.header().len();
            let mut chunks: Vec<&[u8]> = plaintext.chunks(READ_BUF).collect();
            if chunks.is_empty() {
                chunks.push(&[]);
            }
            let (last, rest) = chunks.split_last().unwrap();
            for c in rest {
                produced += enc.encrypt_chunk(c).unwrap().len();
            }
            let (ct, _md5) = enc.finalize_last(last).unwrap();
            produced += ct.len();
            assert_eq!(
                produced as u64,
                predicted_sent_len(plaintext_len as u64, true),
                "framing mismatch for plaintext_len={plaintext_len}"
            );
        }
    }

    // --- P1-4: large file streams (recv-loop flush + bounded memory) --------

    /// A file well above [`PIPELINE_THRESHOLD`] AND large enough to fill the
    /// uploader's `while acc.len() >= 2 * WIRE_CHUNK` flush branch (which the
    /// 5-MiB+7 `large_file_uses_resumable_and_commits` never reaches). Drives
    /// the full 3-stage streaming pipeline and asserts the bytes land intact
    /// and the in-flight memory stayed bounded far below the file size.
    #[tokio::test]
    async fn large_file_streams_with_bounded_memory() {
        let h = harness().await;
        // 64 MiB: > 2*WIRE_CHUNK (8 MiB) so the streaming flush loop runs many
        // times; bounded memory must keep peak far below 64 MiB.
        let size_bytes = 64 * 1024 * 1024usize;
        let big: Vec<u8> = (0..size_bytes).map(|i| (i % 251) as u8).collect();
        let (rel, size) = h.write_file("huge.bin", &big);
        let gauge = Arc::new(MemGauge::default());
        let exec = h.executor().with_mem_gauge(gauge.clone());
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(matches!(out[0], OpOutcome::Done { .. }), "got {:?}", out[0]);

        // Bytes landed intact.
        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].size, Some(size));
        assert_eq!(h.remote.open_session_count(), 0, "no leaked session");

        // Peak in-flight bytes stayed BOUNDED: a regression to whole-file
        // buffering would push peak to ~64 MiB. The bound is the channel
        // backlog (8 x 64 KiB) + the uploader accumulator (< 2 x 4 MiB wire
        // chunks) + slack -> comfortably under 16 MiB and emphatically <<
        // the 64 MiB file.
        let peak = gauge.peak();
        // Lower bound: prove the STREAMING path actually ran (peak==0 would
        // mean the file took the inline buffered path and the gauge recorded
        // nothing - a vacuous pass). The 2 x WIRE_CHUNK hold-back guarantees
        // peak >= WIRE_CHUNK for any file far above 8 MiB.
        assert!(
            peak >= WIRE_CHUNK as u64,
            "streaming must actually run (peak >= one wire chunk proves accumulation), got {peak}"
        );
        assert!(
            peak < 16 * 1024 * 1024,
            "streaming peak in-flight {peak} bytes must stay bounded (<16 MiB), not buffer the 64 MiB file"
        );
        assert!(
            peak < size / 4,
            "peak {peak} must be far below the {size}-byte file size"
        );
    }

    // --- P1-4: streamed-session-invalidation fails without hanging ----------

    /// If a resumable session 4xx's mid-stream, the streamed transfer cannot
    /// replay the consumed channel, so it surfaces a per-op failure. The
    /// three concurrent stages must NOT deadlock when one errors: a
    /// `tokio::time::timeout` turns a hang into a failure rather than stalling
    /// CI. (Validates the `join!` + send-error-terminates design.)
    #[tokio::test]
    async fn streamed_session_invalid_fails_without_hang() {
        // Arm the next-opened session to die after its first accepted chunk.
        let remote = InMemoryRemoteStore::new().with_session_invalidated_after(1);
        let h = harness_with_remote(remote).await;
        // > PIPELINE_THRESHOLD and > 1 wire chunk so a mid-stream chunk push
        // hits the armed invalidation.
        let big = vec![0x33u8; 12 * 1024 * 1024];
        let (rel, size) = h.write_file("dies.bin", &big);
        let exec = h.executor();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            exec.execute(&h.source, &h.upload_plan(&rel, size), &noop_progress),
        )
        .await
        .expect("streaming must not hang on a mid-stream session failure")
        .unwrap();
        assert!(
            matches!(out[0], OpOutcome::Failed { .. }),
            "a mid-stream session 4xx surfaces a per-op failure, got {:?}",
            out[0]
        );
        // Not committed as Synced. (The fake keeps an invalidated session as
        // a dead tombstone in its map - that is fake-internal modelling, not
        // an executor leak - so we do not assert open_session_count here.)
        let row = h.state.get_file_state(h.source.id, &rel).await.unwrap();
        assert!(row.is_none() || row.unwrap().status != FileStateStatus::Synced);
    }

    // --- P1-5: large encrypted file streams under a ciphertext name ---------

    /// An encrypted file above [`PIPELINE_THRESHOLD`] exercises the encrypted
    /// streaming pipeline + the up-front length prediction against REAL
    /// crypto framing (the `FakeSuite` test above only covers the inline
    /// path). Asserts the upload lands, the stored name is ciphertext (not
    /// the plaintext name), and the content round-trips through decryption.
    #[tokio::test]
    async fn large_encrypted_file_streams_and_round_trips() {
        use driven_crypto::{ContentDecryptor, DrivenCryptoSuite, HEADER_LEN};
        use tokio::io::AsyncReadExt;

        let h = harness().await;
        let source_key = driven_crypto::key::SourceKey::generate();
        let suite = Arc::new(DrivenCryptoSuite::new(source_key.clone()));
        let exec = h.executor_with_crypto(Some(suite));

        // > PIPELINE_THRESHOLD so the encrypted STREAMING path runs.
        let plaintext: Vec<u8> = (0..(6 * 1024 * 1024usize))
            .map(|i| (i % 253) as u8)
            .collect();
        let (rel, size) = h.write_file("big-secret.bin", &plaintext);
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(
            matches!(out[0], OpOutcome::Done { .. }),
            "encrypted streamed upload should land: {:?}",
            out[0]
        );

        // The stored object's NAME is ciphertext, not "big-secret.bin".
        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
        assert_ne!(
            children[0].name, "big-secret.bin",
            "the Drive object name must be the ciphertext, not the plaintext filename"
        );

        // Download the ciphertext + decrypt -> the original plaintext.
        let mut blob = Vec::new();
        h.remote
            .download(&children[0].id)
            .await
            .unwrap()
            .0
            .read_to_end(&mut blob)
            .await
            .unwrap();
        assert_ne!(blob, plaintext, "stored bytes must be encrypted");
        let restore = DrivenCryptoSuite::new(source_key);
        let mut dec: Box<dyn ContentDecryptor> =
            restore.content_decryptor(&blob[..HEADER_LEN]).unwrap();
        let mut restored = Vec::new();
        // Decrypt chunk-by-chunk at the 64-KiB+tag boundary the encryptor used.
        let ct_chunk = READ_BUF + 16; // plaintext chunk + Poly1305 tag
        let body = &blob[HEADER_LEN..];
        let mut off = 0;
        while body.len() - off > ct_chunk {
            restored.extend_from_slice(&dec.decrypt_chunk(&body[off..off + ct_chunk]).unwrap());
            off += ct_chunk;
        }
        restored.extend_from_slice(&dec.decrypt_last(&body[off..]).unwrap());
        assert_eq!(restored, plaintext, "decrypted bytes match the original");

        // file_state carries the plaintext blake3 + the encrypted_remote_path.
        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.hash_blake3, *blake3::hash(&plaintext).as_bytes());
        assert_eq!(
            row.encrypted_remote_path.as_deref(),
            Some(children[0].name.as_str()),
            "encrypted_remote_path is the (flat) ciphertext leaf name"
        );
    }

    // --- P1-5 x Cluster-A: reconcile adopts a NESTED encrypted orphan -------

    /// An encrypted file's orphan lives under a nested CIPHERTEXT folder, not
    /// the source root. Reconcile must re-derive that encrypted parent and
    /// `find_by_op_uuid` THERE, or it would miss the orphan and the next scan
    /// would re-upload it as a DUPLICATE - regressing Cluster-A's no-duplicate
    /// contract. This drives a real encrypted nested upload, simulates the
    /// lost-commit crash, reconciles with a fresh executor, and asserts the
    /// orphan is adopted (no duplicate).
    #[tokio::test]
    async fn reconcile_adopts_nested_encrypted_orphan_no_duplicate() {
        use driven_crypto::DrivenCryptoSuite;

        let h = harness().await;
        let source_key = driven_crypto::key::SourceKey::generate();
        let make_exec =
            || h.executor_with_crypto(Some(Arc::new(DrivenCryptoSuite::new(source_key.clone()))));

        let (rel, size) = h.write_file("a/b/c.bin", b"nested encrypted payload");

        // Phase 1: a normal encrypted upload (lands under nested ciphertext
        // folders).
        let exec = make_exec();
        let out = exec
            .execute(&h.source, &h.upload_plan(&rel, size), &noop_progress)
            .await
            .unwrap();
        assert!(matches!(out[0], OpOutcome::Done { .. }));

        // Find the leaf object + its parent (deepest ciphertext folder) +
        // op_uuid by walking the encrypted tree.
        let lvl1 = &h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap()[0];
        let lvl2 = &h.remote.list_folder(&lvl1.id).await.unwrap()[0];
        let leaf = h.remote.list_folder(&lvl2.id).await.unwrap()[0].clone();
        let op_uuid = leaf
            .app_properties
            .get(CLIENT_OP_UUID_KEY)
            .cloned()
            .expect("leaf carries its client_op_uuid");

        // Simulate the lost commit: drop file_state + re-enqueue the create op.
        h.state.delete_file_state(h.source.id, &rel).await.unwrap();
        let now = h.clock.now_ms();
        let uploaded_hex = hex::encode(blake3::hash(b"nested encrypted payload").as_bytes());
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

        // Phase 2: reconcile with a fresh executor. It must find the orphan
        // under the NESTED encrypted parent (not the root) and adopt it.
        let exec2 = make_exec();
        exec2.reconcile(&h.source).await.unwrap();

        // The SAME object was adopted: still exactly one object in the leaf
        // folder, file_state restored Synced with that id, op drained.
        let leaf_after = h.remote.list_folder(&lvl2.id).await.unwrap();
        assert_eq!(
            leaf_after.len(),
            1,
            "no duplicate under the encrypted folder"
        );
        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .expect("file_state restored by adoption");
        assert_eq!(row.status, FileStateStatus::Synced);
        assert_eq!(
            row.drive_file_id.as_deref(),
            Some(leaf.id.as_str()),
            "adopted the SAME nested object id, not a fresh upload"
        );
        assert!(h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap()
            .is_empty());
    }
}
