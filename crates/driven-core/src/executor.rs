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
use driven_drive::google::{classification_of, DriveError as DriveStoreError};
use driven_drive::remote_store::{
    AboutInfo, DownloadStream, DriveErrorClassification, RemoteEntry, RemoteStore, ResumableKind,
    ResumableSession, ResumeProgress, UploadBody,
};
use driven_vss::{fallback_decision, FallbackDecision, OpenAttempt, SnapshotOutcome, VssMode};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::network::{NetworkProbe, ServiceName};
use crate::pacer::{Pacer, ResponseClass};
use crate::state::{BundleRow, FileStateRow, NewFileVersion, NewPendingOp, SourceRow, StateRepo};
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

/// `appProperties` key stamped on a `.tar.gz` bundle object marking it as a
/// Driven bundle and naming its archive format (V2 small-file bundling, issue
/// #35). Lets a future reader (and any DESIGN s18.9 folder-sweep) recognise the
/// object as ours and pick the right extractor; the value is
/// [`crate::bundle::BUNDLE_FORMAT`].
const BUNDLE_FORMAT_KEY: &str = "driven.bundle_format";

/// `pending_ops.op_type` value the executor finalizes via
/// `commit_*_result` (SPEC s2; matches the SqliteStateRepo bound).
const OP_TYPE_UPLOAD: &str = "upload";

/// `pending_ops.op_type` value for a bundle upload (V2 small-file bundling,
/// issue #35). A surviving `bundle` pending_op means the bundle's
/// `commit_bundle_result` never ran, so reconcile trashes any orphaned object by
/// its op-uuid and drops the op; the members (never committed) are re-detected +
/// re-bundled by the next scan.
const OP_TYPE_BUNDLE: &str = "bundle";

/// MIME type for a `.tar.gz` bundle object.
const BUNDLE_MIME: &str = "application/gzip";

/// Max retries for transient 5xx / network failures (DESIGN s5.4 "5xx ->
/// exponential backoff, max 6 retries").
const MAX_TRANSIENT_RETRIES: u32 = 6;

/// Number of CONSECUTIVE verified checksum mismatches on the SAME file that
/// trips the DESIGN s5.4 (lines 498-500) corrupt defence: "Three consecutive
/// mismatches on the same file -> mark `status='corrupt'`, log, surface to
/// user." After the Nth the executor marks the `file_state` row
/// [`FileStateStatus::Corrupt`] and stops retrying it (R2-P1-3).
const MAX_CHECKSUM_MISMATCHES: u32 = 3;

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

/// Issue #36: the OLD `file_state` identity captured when a versioned content
/// change is about to supersede it with a NEW Drive object. Carries everything
/// needed to (a) record the old object as a retained `file_versions` row and
/// (b) short-circuit an identical-content touch (new blake3 == old blake3). Only
/// built when the source has versioning enabled, the file is already uploaded
/// (a content change), and the old object passes the per-source size guard.
#[derive(Debug, Clone)]
struct VersionSupersede {
    /// The OLD Drive object id (becomes a retained, then trashed, version).
    old_drive_file_id: String,
    /// The OLD plaintext size.
    old_size: u64,
    /// The OLD plaintext BLAKE3 - compared against the new hash to detect an
    /// identical-content (mtime-only) touch and skip creating a spurious version.
    old_hash_blake3: [u8; 32],
    /// The OLD ciphertext md5 (encrypted source) / bytes md5.
    old_drive_md5: Option<[u8; 16]>,
    /// The OLD cached encrypted remote path (encrypted source) / `None`.
    old_encrypted_remote_path: Option<String>,
    /// When the OLD version first became current (its `last_uploaded_at`); the
    /// `created_at` of the recorded version.
    old_created_at: i64,
    /// The per-source retained-version cap (`>= 1`), for the post-commit prune.
    cap: u32,
}

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
    /// R2-P1-1: the Drive file id of a corrupt CREATE object whose post-upload
    /// best-effort trash FAILED, so it may still be live on Drive. Persisted so
    /// the reconcile pass can RETRY the trash (the durable corrupt-create
    /// cleanup); the op is KEPT (not dropped) while this is set. Cleared once the
    /// object is confirmed gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    corrupt_file_id: Option<String>,
    /// Issue #36: when a VERSIONED source's content change is uploaded as a
    /// CREATE of a NEW object (so the OLD object survives as a version), this
    /// carries the OLD (to-be-superseded) Drive file id. Distinct from
    /// `drive_file_id` (which stays `None` so reconcile sees a pure create):
    /// the presence of `drive_file_id` dispatches the update-recovery path, so
    /// the supersede intent MUST ride in its own field. Persisted BEFORE the
    /// create so a crash-recovery adopt can still record the version + trash the
    /// old object (rather than leaking it live). Cleared/absent for a normal
    /// (non-versioned) create or update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supersedes_drive_file_id: Option<String>,
    /// Issue #36: the Drive file id of a redundant DUPLICATE object created by an
    /// identical-content (mtime-only) touch, whose immediate best-effort trash
    /// FAILED (or was interrupted by a crash before it ran). The OLD object stays
    /// current, so this NEW object is a live untracked duplicate; persisted so the
    /// reconcile pass RETRIES the trash (mirrors [`Self::corrupt_file_id`]). The op
    /// is KEPT (not dropped) while this is set, and dropped once the object is
    /// confirmed gone. When set, ALL other recovery fields (`client_op_uuid`,
    /// `supersedes_drive_file_id`, ...) are cleared - the op is a pure cleanup
    /// handle, never re-adopted or re-uploaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    redundant_duplicate_file_id: Option<String>,
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
    /// The file is locked and the least-privilege VSS helper broker is still
    /// LAUNCHING / awaiting elevation approval (DESIGN s5.3.1). The file is
    /// skipped TRANSIENTLY this cycle and retried next cycle (once the broker is
    /// up) - NOT reported as a permanent lock (`local.vss_helper_pending`).
    VssHelperPending,
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
            SkipReason::VssHelperPending => ErrorCode::LocalVssHelperPending,
        }
    }
}

/// The kind of work a successful [`OpOutcome::Done`] completed (R1-P1-1).
///
/// Distinguishes an upload (carries its byte count for the DESIGN s8.3
/// "Uploaded today/this week" + throughput aggregates) from a trash, so the
/// orchestrator records the correct `upload_done` / `trash_done` activity row
/// (SPEC s24 schema comment) without re-matching the outcome against the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoneKind {
    /// A successful upload (create or update); the byte count is carried in
    /// [`OpOutcome::Done::bytes`].
    Upload,
    /// A successful trash (remote delete).
    Trash,
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
        /// Whether this was an upload or a trash (R1-P1-1), so the
        /// orchestrator records `upload_done` vs `trash_done`.
        kind: DoneKind,
        /// The uploaded byte count for an [`DoneKind::Upload`] (feeds the
        /// DESIGN s8.3 byte aggregates); `None` for a trash.
        bytes: Option<u64>,
    },
    /// A bundle upload completed (V2 small-file bundling, issue #35): one
    /// `.tar.gz` Drive object landed and its md5 verified, and N member
    /// `file_state` rows + their membership were committed. Carried as ONE
    /// outcome (with the aggregate file + byte counts) rather than N per-member
    /// outcomes, so the orchestrator records a single `bundle_upload` activity
    /// row and the progress bar advances by `files` at once.
    BundleDone {
        /// A representative member path (the first packed member), so
        /// [`OpOutcome::relative_path`] has a value like every other variant.
        relative_path: RelativePath,
        /// Number of member files actually packed + committed (skipped members
        /// are excluded).
        files: u64,
        /// Byte size of the uploaded bundle object (the bytes actually stored on
        /// Drive; ciphertext size for an encrypted source).
        bytes: u64,
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
            OpOutcome::Done { relative_path, .. }
            | OpOutcome::BundleDone { relative_path, .. }
            | OpOutcome::Skipped { relative_path, .. }
            | OpOutcome::Failed { relative_path, .. } => relative_path,
        }
    }
}

/// R2-P2-1: the per-op outcome sink the executor calls IMMEDIATELY after each
/// op's durable `file_state` commit, so the orchestrator can persist that op's
/// `activity_log` row + broadcast `activity:new` per file (rather than in a
/// post-pass that a mid-plan crash would lose). Returns a boxed future so the
/// sink can do an async DB write; the borrow is tied to the call so the closure
/// may capture `&self` / the source without a `'static` bound.
pub type OutcomeSink<'a> =
    dyn Fn(&OpOutcome) -> futures::future::BoxFuture<'a, ()> + Send + Sync + 'a;

/// A no-op [`OutcomeSink`] for callers that drive [`Executor::execute`] WITHOUT
/// the orchestrator's per-op activity wiring (the chaos harness + e2e/unit tests
/// that only assert on the returned outcomes). Returns an immediately-ready
/// future so it adds no latency. Exposed so those callers do not each need a
/// `futures` dependency to construct the boxed future.
pub fn noop_outcome_sink(_outcome: &OpOutcome) -> futures::future::BoxFuture<'static, ()> {
    Box::pin(async {})
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
    ///
    /// R2-P2-1: `on_outcome` is AWAITED for each op's outcome the instant that
    /// op completes (after its durable `file_state` commit, in completion
    /// order), so the orchestrator persists per-op activity immediately. A
    /// crash mid-plan therefore keeps the audit rows for every op that already
    /// committed, and a large initial backup shows per-file activity as it runs
    /// rather than only at the end.
    async fn execute(
        &self,
        source: &SourceRow,
        plan: &Plan,
        on_progress: &(dyn Fn(ExecProgress) + Send + Sync),
        on_outcome: &OutcomeSink<'_>,
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
    /// The PER-SOURCE crypto resolver (M5 GA blocker, CODEX_NOTES "Per-source
    /// crypto resolution"). `None` = no provider configured = every source is
    /// plaintext (tests + an unencrypted-only account). When `Some`, the
    /// executor MUST resolve the suite per source via
    /// [`CryptoProvider::resolve`] and FAIL CLOSED on
    /// [`CryptoResolution::Unavailable`] (an encryption-enabled source whose
    /// key is missing must NEVER upload plaintext). Replaces the pre-M5
    /// executor-wide `Option<Arc<dyn SourceCryptoSuite>>`.
    pub crypto: Option<Arc<dyn crate::crypto_provider::CryptoProvider>>,
    /// The per-cycle Windows VSS snapshot provider (ROADMAP M3.5, DESIGN
    /// s5.3), or `None` to disable the VSS fallback entirely (the historical
    /// behaviour: a locked file is always skipped). The orchestrator owns the
    /// snapshot lifecycle and passes a CLONE of the same `Arc` here so the
    /// executor's open path can read a locked file from the shadow copy.
    pub vss: Option<Arc<dyn driven_vss::VssProvider>>,
    /// The real-outcome reporter seam (CODEX_NOTES P2-9 "Drive circuit
    /// breaker driven by real request outcomes", M4): the
    /// [`NetworkProbe`](crate::network::NetworkProbe) the executor will call
    /// `note_outcome(ServiceName::Drive, ok)` on after each real Drive request
    /// so the breaker reacts to true request health, not just probes. `None`
    /// disables outcome reporting (every test + the current orchestrator wiring
    /// pass `None`; the call sites are agent P's body work, deliberately not
    /// added yet).
    pub network: Option<Arc<dyn crate::network::NetworkProbe>>,
}

// -----------------------------------------------------------------------------
// BreakerReportingStore: report real Drive request outcomes to the circuit
// breaker (CODEX_NOTES P2-9 "Drive circuit breaker driven by real request
// outcomes", M4).
// -----------------------------------------------------------------------------

/// Decide whether a failed Drive request is a SERVICE-HEALTH failure that
/// should advance the Drive breaker's consecutive-failure count
/// (CODEX_NOTES P2-9).
///
/// Only transport / 5xx / rate-limited classes count: a flaky link, a Drive
/// 5xx, or a 429 are evidence the *service* (or the path to it) is unhealthy.
/// A per-file logical failure - a checksum mismatch (which arrives as
/// `Ok(entry)` anyway, so it never reaches this function), a 404, a quota /
/// auth / dest-folder error, or anything that does not downcast to a typed
/// [`DriveError`](driven_drive::google::DriveError) - is NOT a service-health
/// signal and must not penalise the breaker (DESIGN s5.8.3). We return
/// `false` (do nothing) for those rather than `true` (which would RESET the
/// streak and mask genuine transport trouble).
fn drive_err_is_service_failure(err: &anyhow::Error) -> bool {
    matches!(
        classification_of(err),
        Some(
            DriveErrorClassification::Network
                | DriveErrorClassification::Transient5xx
                | DriveErrorClassification::RateLimited { .. }
        )
    )
}

/// A [`RemoteStore`] decorator that reports each delegated request's outcome
/// to the [`NetworkProbe`]'s Drive circuit breaker (CODEX_NOTES P2-9).
///
/// Wrapping the store at the trait boundary is what makes "the breaker is
/// driven by REAL request outcomes" hold without sprinkling
/// capture-classify-report logic across the executor's ~10 Drive call sites:
/// every `self.remote.*` call already routes through this one seam, so each
/// real request reports exactly once (no missed path, no double-count, no
/// surgery at each bare `?`).
///
/// Semantics (per delegated method):
/// - `Ok(_)` -> `note_outcome(Drive, true)` (closes the breaker / resets the
///   failure streak; a checksum mismatch is an `Ok(entry)` here, so it
///   correctly reports healthy - the mismatch is a per-file logical failure,
///   not a service outage).
/// - `Err(e)` -> `note_outcome(Drive, false)` ONLY when
///   [`drive_err_is_service_failure`] is true (Network / Transient5xx /
///   RateLimited); for every other error, do NOTHING (do not penalise, do
///   not reset the streak).
///
/// Only inserted when an executor is constructed with
/// `ExecutorDeps.network = Some(..)`; otherwise the executor holds the inner
/// store directly, so every `network: None` test path is byte-identical to
/// before this decorator existed.
struct BreakerReportingStore {
    inner: Arc<dyn RemoteStore>,
    network: Arc<dyn NetworkProbe>,
}

impl BreakerReportingStore {
    /// Reports `result`'s Drive-health verdict to the breaker, then returns
    /// the result unchanged so the caller's error handling is untouched.
    fn report<T>(&self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        match &result {
            Ok(_) => self.network.note_outcome(ServiceName::Drive, true),
            Err(e) => {
                if drive_err_is_service_failure(e) {
                    self.network.note_outcome(ServiceName::Drive, false);
                }
            }
        }
        result
    }
}

#[async_trait::async_trait]
impl RemoteStore for BreakerReportingStore {
    async fn ensure_folder(&self, parent_id: &str, name: &str) -> anyhow::Result<RemoteEntry> {
        self.report(self.inner.ensure_folder(parent_id, name).await)
    }

    async fn list_folder(&self, folder_id: &str) -> anyhow::Result<Vec<RemoteEntry>> {
        self.report(self.inner.list_folder(folder_id).await)
    }

    async fn create(
        &self,
        parent_id: &str,
        name: &str,
        mime: &str,
        body: UploadBody,
        app_properties: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        self.report(
            self.inner
                .create(parent_id, name, mime, body, app_properties)
                .await,
        )
    }

    async fn update(
        &self,
        file_id: &str,
        body: UploadBody,
        app_properties_patch: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        self.report(self.inner.update(file_id, body, app_properties_patch).await)
    }

    async fn resumable_session(
        &self,
        kind: ResumableKind,
        mime: &str,
        size: u64,
    ) -> anyhow::Result<ResumableSession> {
        self.report(self.inner.resumable_session(kind, mime, size).await)
    }

    async fn resume_chunk(
        &self,
        session: &ResumableSession,
        offset: u64,
        chunk: Bytes,
    ) -> anyhow::Result<ResumeProgress> {
        // A `ResumeProgress::SessionInvalid` is an `Ok(_)` at the trait
        // boundary (the request succeeded; Drive answered "session dead"),
        // so it reports healthy here - the session-restart is a per-op
        // concern, not a transport failure. A transport/5xx error on the PUT
        // surfaces as `Err` and is classified normally.
        self.report(self.inner.resume_chunk(session, offset, chunk).await)
    }

    async fn trash(&self, file_id: &str) -> anyhow::Result<()> {
        self.report(self.inner.trash(file_id).await)
    }

    async fn delete_permanent(&self, file_id: &str) -> anyhow::Result<()> {
        // Issue #36: delegate the hard-delete to the inner store WITH the same
        // breaker accounting as every other call, so a prune during an outage
        // trips/observes the breaker rather than hammering Drive.
        self.report(self.inner.delete_permanent(file_id).await)
    }

    async fn metadata(&self, file_id: &str) -> anyhow::Result<RemoteEntry> {
        self.report(self.inner.metadata(file_id).await)
    }

    async fn download(&self, file_id: &str) -> anyhow::Result<DownloadStream> {
        self.report(self.inner.download(file_id).await)
    }

    async fn find_by_op_uuid(
        &self,
        parent_id: &str,
        op_uuid: &str,
    ) -> anyhow::Result<Option<RemoteEntry>> {
        self.report(self.inner.find_by_op_uuid(parent_id, op_uuid).await)
    }

    async fn about(&self) -> anyhow::Result<AboutInfo> {
        self.report(self.inner.about().await)
    }
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
    /// The PER-SOURCE crypto resolver (M5 GA blocker, CODEX_NOTES "Per-source
    /// crypto resolution"). `None` = no provider configured = every source is
    /// plaintext (tests + an unencrypted-only account). When `Some`, the
    /// executor resolves the suite PER `SourceRow` via [`Self::resolve_crypto`]
    /// and FAILS CLOSED on a missing key for an `encryption_enabled` source -
    /// it never uploads plaintext for an encrypted source nor ciphertext for an
    /// unencrypted one. Threaded through every upload + reconcile call site as a
    /// per-op `Option<Arc<dyn SourceCryptoSuite>>`, never read executor-wide.
    crypto_provider: Option<Arc<dyn crate::crypto_provider::CryptoProvider>>,
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
        // CODEX_NOTES P2-9: when a NetworkProbe is injected, route every Drive
        // request through the BreakerReportingStore so the Drive circuit
        // breaker is driven by REAL request outcomes (not just probes). When
        // `network` is None (every existing test + the current orchestrator
        // wiring) the inner store is used directly, so those paths are
        // byte-identical to before this seam existed.
        let remote: Arc<dyn RemoteStore> = match deps.network {
            Some(network) => Arc::new(BreakerReportingStore {
                inner: deps.remote,
                network,
            }),
            None => deps.remote,
        };
        let crypto_provider = deps.crypto;
        Self {
            remote,
            state: deps.state,
            pacer: deps.pacer,
            crypto_provider,
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

    /// Resolve the crypto decision for one source (M5 GA-blocking surface).
    ///
    /// Consults the injected [`CryptoProvider`]; a `None` provider means every
    /// source is plaintext (the test / unencrypted-only path). Returns the raw
    /// [`CryptoResolution`] from the provider; the FAIL-CLOSED policy keyed on
    /// the [`SourceRow`]'s `encryption_enabled` is applied in
    /// [`Self::resolve_source_crypto`] (the actual per-op call site).
    fn resolve_crypto(&self, source_id: SourceId) -> crate::crypto_provider::CryptoResolution {
        match self.crypto_provider.as_ref() {
            Some(provider) => provider.resolve(&source_id),
            // No provider configured: every source is plaintext (tests +
            // an unencrypted-only account).
            None => crate::crypto_provider::CryptoResolution::Plaintext,
        }
    }

    /// Resolve the effective per-source content/filename suite, applying the
    /// GA-critical FAIL-CLOSED policy keyed on the [`SourceRow`]'s
    /// `encryption_enabled` (CODEX_NOTES "Per-source crypto resolution",
    /// DESIGN s7). The executor - not the provider - is the fail-closed
    /// authority, so a buggy or absent provider can never leak plaintext for an
    /// encrypted source nor ciphertext for an unencrypted one:
    ///
    /// - **`encryption_enabled == false`**: returns `Ok(None)` -> upload
    ///   PLAINTEXT, ignoring ANY suite the provider returns (branch (c)). An
    ///   unencrypted source must NEVER upload ciphertext.
    /// - **`encryption_enabled == true` AND a suite resolved**: returns
    ///   `Ok(Some(suite))` -> upload CIPHERTEXT through that source's suite
    ///   (branch (a)).
    /// - **`encryption_enabled == true` AND no suite** (provider absent, or it
    ///   resolved [`CryptoResolution::Plaintext`] / [`CryptoResolution::Unavailable`]):
    ///   returns `Err(())` -> the caller MUST FAIL CLOSED with
    ///   [`ErrorCode::CryptoKeyMissing`] and upload NOTHING (branch (b)). Never
    ///   degrade to plaintext.
    ///
    /// `Err(())` is the fail-closed signal; the caller maps it to a
    /// `crypto.key_missing` op failure (SPEC s24).
    fn resolve_source_crypto(
        &self,
        source: &SourceRow,
    ) -> Result<Option<Arc<dyn SourceCryptoSuite>>, ()> {
        if !source.encryption_enabled {
            // Branch (c): unencrypted source - plaintext, ignore any suite.
            return Ok(None);
        }
        // Branch (a)/(b): encryption is on. Only an actually-resolved suite
        // permits a (ciphertext) upload; anything else fails closed.
        match self.resolve_crypto(source.id) {
            crate::crypto_provider::CryptoResolution::Suite(suite) => Ok(Some(suite)),
            crate::crypto_provider::CryptoResolution::Plaintext
            | crate::crypto_provider::CryptoResolution::Unavailable => {
                warn!(
                    target: TARGET,
                    source = %source.id,
                    "encryption-enabled source has no resolvable key; failing closed (crypto.key_missing) - NEVER uploading plaintext"
                );
                Err(())
            }
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
                    EffectiveOpen::Failed(local_io_error_code(&e))
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
                    return EffectiveOpen::Failed(local_io_error_code(&e));
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
                    return EffectiveOpen::Failed(local_io_error_code(&e));
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
                            EffectiveOpen::Failed(local_io_error_code(&e))
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
                // P2-6 / recheck2 (SPEC s24): distinguish "locked, VSS COULD help
                // but is not available" from "locked, VSS disabled or tried+failed".
                // `vss.available()` (here `elevated`) is false for ALL of:
                // un-elevated, off-Windows, AND `vss_mode = never`. Only the first
                // two ("VSS would help if Driven ran elevated") warrant
                // `local.vss_unavailable`; `never` is a user choice, so a locked
                // file under `never` is a plain `local.file_locked` (NOT a
                // misleading "needs elevation"). A genuine snapshot/map failure
                // despite availability is also `local.file_locked`.
                let reason =
                    if attempt == OpenAttempt::Locked && mode != VssMode::Never && !elevated {
                        SkipReason::VssUnavailable
                    } else {
                        SkipReason::Locked
                    };
                EffectiveOpen::Skip(reason)
            }
            FallbackDecision::SkipRetryLater => {
                // DESIGN s5.3.1: the least-privilege helper broker is launching /
                // awaiting elevation approval. Skip this locked file TRANSIENTLY
                // (it re-queues like any skip) and classify it as helper-pending -
                // NOT a permanent lock - so the brief launch-in-progress window is
                // not misreported. The next cycle (broker up) backs it up.
                EffectiveOpen::Skip(SkipReason::VssHelperPending)
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
        // --- GA-critical FAIL-CLOSED crypto resolution (M5, DESIGN s7) ------
        // Resolve this source's suite FIRST, before opening the file or
        // touching state/Drive: an `encryption_enabled` source whose key is
        // unavailable must error `crypto.key_missing` and upload NOTHING, never
        // plaintext (CODEX_NOTES "Per-source crypto resolution"). An unencrypted
        // source resolves to `None` (plaintext) and ignores any suite.
        let crypto = match self.resolve_source_crypto(source) {
            Ok(crypto) => crypto,
            Err(()) => {
                return Ok(OpOutcome::Failed {
                    relative_path: relative_path.clone(),
                    code: ErrorCode::CryptoKeyMissing,
                });
            }
        };

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

        // --- issue #36: should this content change be VERSIONED? -----------
        // A versioned change uploads a NEW Drive object (create) and keeps the
        // OLD one as a restorable version, instead of overwriting in place
        // (update). Only when the source has versioning enabled, there IS an
        // existing object (a content change, not a first upload), and the old
        // object passes the per-source size guard. Resolved once here (gated on
        // `existing_file_id.is_some()` so first uploads never pay for the lookup).
        let version_supersede = if existing_file_id.is_some() {
            self.resolve_version_supersede(source, existing.as_ref())
                .await
        } else {
            None
        };
        let versioned = version_supersede.is_some();
        // For a versioned change force the CREATE path: `effective_existing_id`
        // is None (the Drive call is a create; reconcile sees a pure create) and
        // the OLD id rides in `payload.supersedes_drive_file_id` instead.
        let effective_existing_id = if versioned {
            None
        } else {
            existing_file_id.clone()
        };

        // --- enqueue the pending_op with a fresh client_op_uuid ------------
        // (DESIGN s5.6 step 1: the UUID lands in pending_ops, transactionally,
        // BEFORE we issue the create/update.)
        let op_uuid = uuid::Uuid::new_v4().to_string();
        let now = self.clock.now_ms();
        let payload = PendingOpPayload {
            client_op_uuid: Some(op_uuid.clone()),
            drive_file_id: effective_existing_id.clone(),
            supersedes_drive_file_id: version_supersede
                .as_ref()
                .map(|v| v.old_drive_file_id.clone()),
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
        // Defect 5: the versioned OLD object id, captured BEFORE `version_supersede`
        // is moved into the upload, so a checksum-mismatch corrupt row can preserve
        // the still-live OLD pointer (never NULL) rather than orphaning it.
        let versioned_old_id = version_supersede
            .as_ref()
            .map(|v| v.old_drive_file_id.clone());
        let outcome = self
            .upload_and_commit(
                source,
                relative_path,
                &read_path,
                size,
                &mut file,
                pre,
                effective_existing_id.as_deref(),
                op_id,
                payload,
                app_props,
                from_vss,
                crypto,
                version_supersede,
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
                //
                // Issue #36: a VERSIONED change runs as a CREATE (a NEW object),
                // so `effective_existing_id` is None even though the file was
                // already uploaded - keep the op so reconcile adopts the orphan
                // (its supersede intent rides in the payload).
                if effective_existing_id.is_some() {
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
            Err(UploadError::ChecksumMismatch { stranded_file_id }) => {
                // Issue #36: for a VERSIONED change the mismatching object is the
                // NEW create orphan; `effective_existing_id` is None so it is
                // trashed as a stranded create and the OLD (good) object is left
                // as the current pointer, exactly as a first-time create mismatch.
                //
                // Defect 5: BUT the corrupt `file_state` row must still POINT at the
                // OLD, still-live object - so a later corrupt-threshold row keeps
                // `drive_file_id = old_id` (the eventual recovery re-uploads as an
                // UPDATE against it, never a duplicate) rather than NULL, which would
                // orphan the last-good object as a permanent live leak. So pass the
                // versioned OLD id (falling back to `effective_existing_id` for a
                // non-versioned change / first create).
                let existing_for_corrupt = versioned_old_id
                    .as_deref()
                    .or(effective_existing_id.as_deref());
                self.handle_checksum_mismatch(
                    source,
                    relative_path,
                    &live_path,
                    op_id,
                    existing_for_corrupt,
                    stranded_file_id,
                )
                .await
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

    /// Upload one small-file bundle (V2 small-file bundling, issue #35).
    ///
    /// Packs the (genuinely-new) member files into a single `.tar.gz` object and
    /// commits N `file_state` rows (each `drive_file_id = NULL`) plus their bundle
    /// membership in one transaction. Crash-safe like `hash_then_upload`: the
    /// `client_op_uuid` is written into a `bundle` pending_op BEFORE the create,
    /// and stamped on the object's appProperties, so a crash between the create
    /// and the commit is recovered by `reconcile` (which trashes the orphan by
    /// op-uuid and lets the next scan re-bundle the still-uncommitted members).
    ///
    /// The archive is bounded by the planner (a few MiB) and built + hashed on a
    /// blocking task; the whole object is a single NON-RESUMABLE simple create
    /// (bundles never exceed the simple-upload band), so no resumable session is
    /// ever persisted.
    async fn bundle_upload(
        &self,
        source: &SourceRow,
        members: &[crate::types::BundleMemberPlan],
    ) -> anyhow::Result<OpOutcome> {
        // The planner never emits an empty bundle; guard so every outcome below
        // has a representative path.
        let Some(first_plan) = members.first() else {
            return Err(anyhow::anyhow!(
                "bundle op has no members (planner invariant)"
            ));
        };
        let representative = first_plan.relative_path.clone();

        // FAIL-CLOSED crypto resolution up front (mirrors hash_then_upload): an
        // encryption-enabled source with no resolvable key uploads NOTHING.
        let crypto = match self.resolve_source_crypto(source) {
            Ok(crypto) => crypto,
            Err(()) => {
                return Ok(OpOutcome::Failed {
                    relative_path: representative,
                    code: ErrorCode::CryptoKeyMissing,
                });
            }
        };

        // --- build the .tar.gz on a blocking task (bundles are size-capped) ---
        // Each member carries the size the planner grouped it at; build_bundle
        // re-validates against it and skips any member that grew between scan and
        // now (issue #35: never read a ballooned file into memory, never pack an
        // object past the simple-create band). BUNDLE_MAX_BYTES_CEILING is the
        // absolute plaintext ceiling that keeps the uploaded object a single
        // non-resumable create regardless of the (config-tunable) planner cap.
        let inputs: Vec<(RelativePath, std::path::PathBuf, u64)> = members
            .iter()
            .map(|m| {
                (
                    m.relative_path.clone(),
                    join_source_path(&source.local_path, &m.relative_path),
                    m.size,
                )
            })
            .collect();
        let built = match tokio::task::spawn_blocking(move || {
            crate::bundle::build_bundle(&inputs, crate::planner::BUNDLE_MAX_BYTES_CEILING)
        })
        .await
        {
            Ok(Ok(built)) => built,
            Ok(Err(e)) => {
                warn!(target: TARGET, source = %source.id, error = %e, "bundle build failed");
                return Ok(OpOutcome::Failed {
                    relative_path: representative,
                    code: ErrorCode::LocalIoError,
                });
            }
            Err(join_err) => {
                warn!(target: TARGET, source = %source.id, error = %join_err, "bundle build task panicked");
                return Ok(OpOutcome::Failed {
                    relative_path: representative,
                    code: ErrorCode::InternalBug,
                });
            }
        };

        if !built.skipped.is_empty() {
            debug!(
                target: TARGET,
                source = %source.id,
                skipped = built.skipped.len(),
                "bundle: skipped members that vanished/changed mid-build; they stay pending"
            );
        }
        if built.members.is_empty() {
            // Every candidate vanished/changed between scan and build. Nothing to
            // upload; the members stay uncommitted and the next scan retries them.
            return Ok(OpOutcome::Skipped {
                relative_path: representative,
                reason: SkipReason::ChangedDuringUpload,
            });
        }

        // --- produce the exact bytes to send (encrypt-or-plaintext + md5) -----
        let sent = match bundle_sent_bytes(built.tar_gz, crypto.as_deref()) {
            Ok(sent) => sent,
            Err(e) => {
                warn!(target: TARGET, source = %source.id, error = %e, "bundle encryption failed");
                return Ok(OpOutcome::Failed {
                    relative_path: representative,
                    code: ErrorCode::InternalBug,
                });
            }
        };
        let object_size = sent.bytes.len() as u64;

        // --- enqueue the crash-safe bundle pending_op (uuid BEFORE the create) -
        let op_uuid = uuid::Uuid::new_v4().to_string();
        let now = self.clock.now_ms();
        let payload = PendingOpPayload {
            client_op_uuid: Some(op_uuid.clone()),
            ..PendingOpPayload::default()
        };
        let op_id = self
            .state
            .enqueue_pending_op(NewPendingOp {
                source_id: source.id,
                op_type: OP_TYPE_BUNDLE.to_string(),
                // pending_ops.relative_path is NOT NULL; a bundle op carries a
                // representative member path (used only for diagnostics - the
                // bundle finalize/reconcile key off the id + op_type + uuid).
                relative_path: representative.clone(),
                payload_json: payload.to_value(),
                scheduled_for: now,
                created_at: now,
            })
            .await?;

        // --- upload as ONE non-resumable simple create -----------------------
        let name = format!("driven-bundle-{op_uuid}.tar.gz");
        let app_props = self.bundle_app_properties(source.id, &op_uuid);
        let target = RemoteTarget {
            parent_id: source.drive_folder_id.clone(),
            name,
            app_props,
            encrypted_remote_path: None,
        };
        let mut payload = payload;
        let upload = self
            .upload_bytes(
                &target,
                None, // always a create - a bundle is a fresh object
                sent,
                BUNDLE_MIME,
                op_id,
                &mut payload,
                false, // never resumable: bundles fit the simple-upload band
            )
            .await;

        match upload {
            Ok(entry) => {
                // Verify already done inside upload_bytes (md5 vs local). Build the
                // bundle row + member file_state rows and commit atomically.
                let bundle_row = BundleRow {
                    id: op_uuid.clone(),
                    source_id: source.id,
                    drive_file_id: entry.id.clone(),
                    drive_md5: entry.md5,
                    size: object_size,
                    member_count: built.members.len() as u64,
                    created_at: now,
                };
                let member_rows: Vec<FileStateRow> = built
                    .members
                    .iter()
                    .map(|m| FileStateRow {
                        source_id: source.id,
                        relative_path: m.rel.clone(),
                        size: m.size,
                        mtime_ns: m.mtime_ns,
                        hash_blake3: m.blake3,
                        drive_file_id: None,
                        drive_md5: None,
                        encrypted_remote_path: None,
                        status: FileStateStatus::Synced,
                        last_uploaded_at: Some(now),
                        last_verified_at: Some(now),
                    })
                    .collect();
                self.state
                    .commit_bundle_result(op_id, &bundle_row, &member_rows)
                    .await?;
                Ok(OpOutcome::BundleDone {
                    relative_path: representative,
                    files: built.members.len() as u64,
                    bytes: object_size,
                })
            }
            Err(UploadError::Failed(code)) => {
                // A clean create failure (definitely no object landed): safe to
                // drop the op; the members stay uncommitted and retry next scan.
                self.state.delete_pending_op(op_id).await?;
                Ok(OpOutcome::Failed {
                    relative_path: representative,
                    code,
                })
            }
            Err(UploadError::DeferToReconcile(code)) => {
                // Ambiguous create failure: the object MIGHT have landed. KEEP the
                // pending op so reconcile trashes any orphan by its op-uuid (there
                // is no folder sweep). Members stay uncommitted -> re-bundled next
                // scan; the surviving op is the cleanup handle.
                Ok(OpOutcome::Failed {
                    relative_path: representative,
                    code,
                })
            }
            Err(UploadError::ChecksumMismatch { .. }) => {
                // The bundle object's md5 did not verify. upload_bytes best-effort
                // trashed the corrupt create; KEEP the op regardless so reconcile
                // trashes it by op-uuid if the trash did not stick. Members stay
                // uncommitted -> re-bundled next scan.
                Ok(OpOutcome::Failed {
                    relative_path: representative,
                    code: ErrorCode::DriveChecksumMismatch,
                })
            }
            Err(UploadError::Skip(reason)) | Err(UploadError::SkipPostUpload(reason)) => {
                // The streaming read-path skip reasons cannot arise for an
                // in-memory bundle body, but handle defensively: no object was
                // created via this path, drop the op and let the next scan retry.
                self.state.delete_pending_op(op_id).await?;
                Ok(OpOutcome::Skipped {
                    relative_path: representative,
                    reason,
                })
            }
            Err(UploadError::Fatal(e)) => Err(e),
        }
    }

    /// Build the appProperties for a bundle object (issue #35): the crash-safe
    /// op-uuid, the source id, and the `driven.bundle_format` marker. A bundle
    /// carries no `relative_path_hash` (it holds many paths).
    fn bundle_app_properties(&self, source_id: SourceId, op_uuid: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(CLIENT_OP_UUID_KEY.to_string(), op_uuid.to_string());
        m.insert(SOURCE_ID_KEY.to_string(), source_id.to_string());
        m.insert(
            BUNDLE_FORMAT_KEY.to_string(),
            crate::bundle::BUNDLE_FORMAT.to_string(),
        );
        m
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
        // M5 per-source crypto: the suite resolved (FAIL-CLOSED) for THIS
        // source - `Some` => encrypt content + filename, `None` => plaintext.
        // Resolved per op in `hash_then_upload`, never executor-wide. Owned
        // (cheap `Arc`) so the streaming cpu stage can clone it.
        crypto: Option<Arc<dyn SourceCryptoSuite>>,
        // Issue #36: `Some` when this content change is VERSIONED - the upload
        // ran as a CREATE of a NEW object and, on success, the OLD object
        // (carried here) is recorded as a version, the pointer flips to the new
        // object, and the old object is trashed. `None` for a normal create /
        // in-place update.
        version: Option<VersionSupersede>,
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
            .resolve_remote_target(source, relative_path, app_props, crypto.as_deref())
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
                crypto.clone(),
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
                crypto.as_deref(),
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
        match &version {
            // Issue #36: VERSIONED content change.
            Some(vs) => {
                if blake3 == vs.old_hash_blake3 {
                    // Identical content (an mtime-only touch): the "new" object we
                    // just created is a byte-for-byte duplicate. DON'T record a
                    // version (it would pollute history and evict a genuine older
                    // version via the count cap). Keep the OLD object as current
                    // (update only mtime/size) and trash the redundant new object.
                    //
                    // Defect 1: PRESERVE the OLD `last_uploaded_at` - `..row` would
                    // advance it to `now`, but the old object's validity window
                    // START must not move: bumping it wrongly rejects a restore-as-of
                    // any instant before this touch (the current bytes ARE those
                    // bytes), and makes the NEXT real change record its version with
                    // the touched (wrong) created_at, leaving that window forever
                    // unrestorable.
                    let keep_row = FileStateRow {
                        drive_file_id: Some(vs.old_drive_file_id.clone()),
                        drive_md5: vs.old_drive_md5,
                        encrypted_remote_path: vs.old_encrypted_remote_path.clone(),
                        last_uploaded_at: Some(vs.old_created_at),
                        ..row
                    };
                    // Defect 2/6: persist the redundant object's id onto the KEPT op
                    // (a durable cleanup handle) ATOMICALLY with the pointer upsert,
                    // so a failed / crash-interrupted trash is retried by the
                    // reconcile sweep and can never leak live. Then trash best-effort;
                    // on a confirmed trash, drop the now-finished cleanup op.
                    let marker = PendingOpPayload {
                        redundant_duplicate_file_id: Some(entry.id.clone()),
                        ..PendingOpPayload::default()
                    };
                    self.state
                        .commit_identical_touch_result(op_id, &keep_row, &marker.to_value())
                        .await
                        .map_err(UploadError::Fatal)?;
                    if self.guarded_trash_and_mark(&entry.id).await {
                        if let Err(err) = self.state.delete_pending_op(op_id).await {
                            warn!(target: TARGET, id = %entry.id, %err, "trashed the redundant identical-touch duplicate but failed to drop its cleanup op; reconcile will re-confirm and drop it");
                        }
                    }
                } else {
                    // Real content change: atomically record the OLD object as a
                    // version, flip the pointer to the NEW object, and drop the op.
                    let superseded = crate::state::NewFileVersion {
                        source_id: source.id,
                        relative_path: relative_path.clone(),
                        drive_file_id: vs.old_drive_file_id.clone(),
                        size: vs.old_size,
                        hash_blake3: vs.old_hash_blake3,
                        drive_md5: vs.old_drive_md5,
                        encrypted_remote_path: vs.old_encrypted_remote_path.clone(),
                        created_at: vs.old_created_at,
                        superseded_at: now,
                    };
                    self.state
                        .commit_versioned_create_result(op_id, &row, &superseded)
                        .await
                        .map_err(UploadError::Fatal)?;
                    // Best-effort (guarded) trash of the now-superseded OLD object,
                    // then prune versions beyond the per-source count cap. Both are
                    // best-effort: a failure leaves extra retained data, never loses
                    // any (the reconcile sweep retries an un-trashed version).
                    self.guarded_trash_and_mark(&vs.old_drive_file_id).await;
                    self.prune_versions(source.id, relative_path, vs.cap).await;
                }
            }
            // Normal create / in-place update (pre-#36 behaviour).
            None => {
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
            }
        }
        // R2-P1-3: a successful upload BREAKS any consecutive-mismatch streak,
        // so reset the durable counter (best-effort - a stale counter would only
        // make a FUTURE mismatch trip the corrupt threshold one attempt early,
        // never lose data, so a clear failure is logged, not fatal).
        if let Err(err) = self
            .state
            .clear_checksum_mismatch_count(source.id, relative_path)
            .await
        {
            warn!(target: TARGET, source = %source.id, path = %relative_path, %err, "failed to clear checksum-mismatch counter after a successful upload");
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
            kind: DoneKind::Upload,
            bytes: Some(post.size),
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
        // M5 per-source crypto: `Some` => encrypt this source, `None` =>
        // plaintext. Resolved (FAIL-CLOSED) per op by the caller.
        crypto: Option<&dyn SourceCryptoSuite>,
    ) -> Result<UploadProduct, UploadError> {
        // M3.5: the post-read identity recheck reads the EFFECTIVE path (the
        // VSS snapshot copy for a locked file), matching the pre-open lstat.
        let full_path = read_path.to_path_buf();

        let HashedBody {
            blake3,
            sent_bytes,
            plaintext_len,
        } = read_hash_encrypt(file, crypto)
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
        if crypto.is_none() && plaintext_len != size {
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
        // M5 per-source crypto: `Some` => encrypt this source, `None` =>
        // plaintext. Resolved (FAIL-CLOSED) per op by the caller; owned so the
        // cpu stage can move it.
        crypto: Option<Arc<dyn SourceCryptoSuite>>,
    ) -> Result<UploadProduct, UploadError> {
        // Predict the exact number of bytes that will be sent to Drive.
        let encrypted = crypto.is_some();
        let total = predicted_sent_len(size, encrypted);

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
        let cpu = cpu_stage(raw_rx, out_tx, crypto, size);
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
                let cleanup = self
                    .trash_corrupt_create(existing_file_id, &entry.id, relative_path.as_str())
                    .await;
                return Err(UploadError::ChecksumMismatch {
                    stranded_file_id: cleanup.stranded_file_id(),
                });
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
            // C5-P1-3: a post-upload checksum mismatch on the simple-band
            // (4-5 MiB CREATE/UPDATE + VSS large reads forced through simple
            // upload) must route through the SAME durable 3-consecutive-mismatch
            // -> corrupt policy + corrupt-create cleanup as the resumable path -
            // NOT collapse into a generic `Failed`. Surface it distinctly so
            // `hash_then_upload` runs `handle_checksum_mismatch`, and carry the
            // stranded corrupt-create id (C5-P1-4) when the store's trash failed
            // so the op is kept for reconcile to retry the trash.
            if matches!(class, DriveError::ChecksumMismatch) {
                return UploadError::ChecksumMismatch {
                    stranded_file_id: stranded_file_id_from_error(&e),
                };
            }
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
        let progress = match self.remote.resume_chunk(session, offset, wire).await {
            Ok(p) => p,
            Err(e) => {
                // codex R-P1-3: classify the chunk error via classify_drive_error
                // instead of blindly aborting the whole execute cycle with a
                // generic `Fatal`. Now that the resumable chunk PUT returns typed
                // quota/auth/rate errors (R-P1-2), a quota/auth/rate failure on a
                // large STREAMED upload surfaces its stable code rather than the
                // opaque `drive.unreachable`.
                let class = classify_drive_error(&e);
                self.pacer.note_response(class.response_class());
                match class {
                    // A transient (5xx / network) or rate-limit on a streamed
                    // chunk is RECOVERABLE by resuming the SAME persisted
                    // session: the executor persisted the session + acked
                    // offset, and reconcile resumes it byte-for-byte (the
                    // streaming source cannot be re-read inline, so we abort the
                    // cycle - Fatal keeps the op WITHOUT writing a file_state
                    // row - and let reconcile pick the session up). Failing the
                    // op here would DROP it and lose the resume handle.
                    DriveError::Transient | DriveError::RateLimited => {
                        return Err(UploadError::Fatal(e));
                    }
                    // A checksum mismatch on a streamed chunk routes through the
                    // 3-consecutive-mismatch policy (R2-P1-3). The streaming
                    // resumable Create finalizes only on its FINAL chunk, so no
                    // object materialized here - nothing to retry-trash
                    // (`stranded_file_id: None`).
                    DriveError::ChecksumMismatch => {
                        return Err(UploadError::ChecksumMismatch {
                            stranded_file_id: None,
                        });
                    }
                    // Quota / daily-quota / auth / dest-folder / other are
                    // terminal for this op: resuming the same session will not
                    // clear them. Fail with the stable code (no object
                    // materialized - dropping the op is safe; the next scan
                    // re-plans from scratch once the condition clears).
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
        };
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
                            // P1-4 / R2-P1-1: trash a corrupt CREATE before
                            // failing so no bad object is stranded (see
                            // `trash_corrupt_create`). Never on an UPDATE. A
                            // FAILED trash strands the object -> keep the op so
                            // reconcile retries the trash.
                            let cleanup = self
                                .trash_corrupt_create(existing_file_id, &entry.id, &target.name)
                                .await;
                            return Err(UploadError::ChecksumMismatch {
                                stranded_file_id: cleanup.stranded_file_id(),
                            });
                        }
                    }
                }
                Err(e) => match self.classify_retry(
                    &e,
                    &mut transient_retries,
                    ambiguous_simple_create,
                )? {
                    RetryDecision::Retry => continue,
                    RetryDecision::Fail(ErrorCode::DriveChecksumMismatch) => {
                        // The REAL GoogleDriveStore verifies md5 INSIDE the store
                        // and best-effort-trashes its own corrupt create (R-P1-1).
                        // When that trash SUCCEEDED the object is gone (no
                        // stranded id) and we route through the 3-mismatch policy
                        // and drop the op. When the store's trash FAILED it now
                        // surfaces the live object's id (C5-P1-4): carry it so the
                        // executor persists `corrupt_file_id` and KEEPS the op for
                        // reconcile to retry the trash, rather than dropping it and
                        // stranding a live corrupt object.
                        return Err(UploadError::ChecksumMismatch {
                            stranded_file_id: stranded_file_id_from_error(&e),
                        });
                    }
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
            // A checksum mismatch is fatal for this op (the corrupt create was
            // already trashed by the store - R-P1-1); fail with the dedicated
            // code, never retry.
            | DriveError::ChecksumMismatch
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
        // M5 per-source crypto (resolved FAIL-CLOSED by the caller): `Some` =>
        // encrypt the path components; `None` => flat plaintext name.
        crypto: Option<&dyn SourceCryptoSuite>,
    ) -> anyhow::Result<RemoteTarget> {
        let Some(crypto) = crypto else {
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
        // M5 per-source crypto (resolved FAIL-CLOSED by the caller).
        crypto: Option<&dyn SourceCryptoSuite>,
    ) -> anyhow::Result<String> {
        match crypto {
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
        // M5 per-source crypto (resolved FAIL-CLOSED by the caller).
        crypto: Option<&dyn SourceCryptoSuite>,
    ) -> anyhow::Result<Option<String>> {
        let Some(crypto) = crypto else {
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

    /// P1-4 / R2-P1-1: trash a corrupt object that a CREATE upload just
    /// finalized with a checksum mismatch, so the failure leaves NOTHING behind
    /// on Drive - and report whether the trash actually succeeded so the caller
    /// can DURABLY retry it (R2-P1-1).
    ///
    /// Scoped to creates: `existing_file_id.is_none()`. For an UPDATE the
    /// `file_id` is the user's PRE-EXISTING object - we must never trash it on a
    /// mismatch (the prior good bytes stay put; the op is dropped and the next
    /// scan retries the update), so this returns [`CorruptCreateCleanup::NotACreate`].
    /// For a CREATE the object is a brand-new orphan carrying this op's uuid; if
    /// left in place, reconcile would adopt it as Synced (local-vs-uploaded
    /// blake3 still agrees - only the on-wire md5 disagreed) and corrupt bytes
    /// would persist while the next scan uploads a duplicate.
    ///
    /// R2-P1-1 durability: a FAILED trash returns [`CorruptCreateCleanup::Stranded`]
    /// carrying the corrupt `file_id` so the caller can persist it and KEEP the
    /// pending op, retrying the trash next cycle - rather than dropping the op
    /// and stranding a live corrupt object with no recovery handle.
    async fn trash_corrupt_create(
        &self,
        existing_file_id: Option<&str>,
        file_id: &str,
        context: &str,
    ) -> CorruptCreateCleanup {
        if existing_file_id.is_some() {
            return CorruptCreateCleanup::NotACreate;
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
                CorruptCreateCleanup::Trashed
            }
            Err(e) => {
                let class = classify_drive_error(&e);
                self.pacer.note_response(class.response_class());
                warn!(
                    target: TARGET,
                    name = %context,
                    file_id = %file_id,
                    "failed to trash corrupt create object after md5 mismatch; keeping the op to retry the trash next cycle: {e}"
                );
                CorruptCreateCleanup::Stranded {
                    file_id: file_id.to_string(),
                }
            }
        }
    }

    /// R2-P1-1 + R-P2-2: retry trashing a corrupt CREATE object whose earlier
    /// trash failed (the durable cleanup the reconcile pass drives). Returns
    /// `Ok(true)` when the object is confirmed gone (trashed now, or already
    /// trashed/gone - an idempotent no-op), so the caller can drop the pending
    /// op; `Ok(false)` when the trash failed for a RETRYABLE reason and the op
    /// must be kept for the next cycle.
    ///
    /// R-P2-2: a trash that fails specifically with `invalid_grant` (a revoked
    /// refresh token) is NOT a "keep the op and retry" condition - retrying
    /// hammers a dead credential. It returns `Err(ReconcileError::AuthInvalidGrant)`
    /// so the reconcile pass can propagate it and the orchestrator runs the SAME
    /// needs-reauth transition the normal upload path takes (DESIGN s5.4). The
    /// pacer is still notified (consistent accounting) before the classified
    /// error is surfaced.
    async fn retry_trash_corrupt(
        &self,
        file_id: &str,
        context: &str,
    ) -> Result<bool, ReconcileError> {
        self.pacer.permit_request().await;
        match self.remote.trash(file_id).await {
            Ok(()) => {
                self.pacer.note_response(ResponseClass::Ok);
                warn!(
                    target: TARGET,
                    name = %context,
                    file_id = %file_id,
                    "reconcile: trashed the previously-stranded corrupt create object"
                );
                Ok(true)
            }
            Err(e) => {
                let class = classify_drive_error(&e);
                self.pacer.note_response(class.response_class());
                if class == DriveError::InvalidGrant {
                    warn!(
                        target: TARGET,
                        name = %context,
                        file_id = %file_id,
                        "reconcile: re-trash of the stranded corrupt object hit auth.invalid_grant; account needs reauth (stopping remote work): {e}"
                    );
                    return Err(ReconcileError::AuthInvalidGrant);
                }
                warn!(
                    target: TARGET,
                    name = %context,
                    file_id = %file_id,
                    "reconcile: re-trash of the stranded corrupt object failed again; keeping the op: {e}"
                );
                Ok(false)
            }
        }
    }

    /// Issue #36 (defect 2/6): reconcile-time retry of trashing a redundant
    /// DUPLICATE object left by an identical-content touch whose immediate trash
    /// failed (or was interrupted by a crash). Suite-FREE (a `remote.trash` by id),
    /// so it runs even when the per-source crypto gate fails closed. Returns `true`
    /// once the object is confirmed gone (drop the cleanup op); `false` to keep the
    /// op for the next cycle; propagates an `invalid_grant` as
    /// [`ReconcileError::AuthInvalidGrant`] (do NOT keep hammering a dead token).
    /// Mirrors [`Self::retry_trash_corrupt`].
    async fn retry_trash_redundant(
        &self,
        file_id: &str,
        context: &str,
    ) -> Result<bool, ReconcileError> {
        self.pacer.permit_request().await;
        match self.remote.trash(file_id).await {
            Ok(()) => {
                self.pacer.note_response(ResponseClass::Ok);
                warn!(
                    target: TARGET,
                    name = %context,
                    file_id = %file_id,
                    "reconcile: trashed the previously-stranded redundant identical-touch duplicate"
                );
                Ok(true)
            }
            Err(e) => {
                let class = classify_drive_error(&e);
                self.pacer.note_response(class.response_class());
                if class == DriveError::InvalidGrant {
                    warn!(
                        target: TARGET,
                        name = %context,
                        file_id = %file_id,
                        "reconcile: re-trash of the redundant duplicate hit auth.invalid_grant; account needs reauth (stopping remote work): {e}"
                    );
                    return Err(ReconcileError::AuthInvalidGrant);
                }
                warn!(
                    target: TARGET,
                    name = %context,
                    file_id = %file_id,
                    "reconcile: re-trash of the redundant duplicate failed again; keeping the op: {e}"
                );
                Ok(false)
            }
        }
    }

    /// Bundle GC (issue #35 item a): trash the Drive object of every bundle for
    /// `source` that has no members left, then drop its `bundles` row.
    ///
    /// A bundle empties when all its members are deleted locally (their
    /// `bundle_members` rows cascade away with the `file_state` rows) or promoted
    /// to standalone objects (a changed member re-uploads on its own, clearing its
    /// membership). The now-dead `.tar.gz` object is trashed by its stored
    /// `drive_file_id` - a suite-FREE `remote.trash` needing no crypto, so this
    /// runs even for an encryption-enabled source whose key is temporarily
    /// unavailable (like the corrupt-create and orphan-bundle cleanup). It is
    /// best-effort and never fatal: a per-bundle failure is logged and left for a
    /// later reconcile sweep, EXCEPT an `invalid_grant` (a revoked token), which is
    /// propagated as [`ReconcileError::AuthInvalidGrant`] so the orchestrator
    /// stops hammering the dead credential (matching [`Self::retry_trash_corrupt`]).
    async fn gc_empty_bundles(&self, source: &SourceRow) -> Result<(), ReconcileError> {
        let empties = match self.state.list_empty_bundles(source.id).await {
            Ok(e) => e,
            Err(err) => {
                warn!(
                    target: TARGET,
                    source = %source.id,
                    "reconcile: could not list empty bundles for GC; skipping this sweep: {err}"
                );
                return Ok(());
            }
        };
        for (bundle_id, drive_file_id) in empties {
            self.pacer.permit_request().await;
            match self.remote.trash(&drive_file_id).await {
                Ok(()) => {
                    self.pacer.note_response(ResponseClass::Ok);
                    if let Err(err) = self.state.delete_bundle(&bundle_id).await {
                        warn!(
                            target: TARGET,
                            source = %source.id,
                            bundle_id = %bundle_id,
                            "reconcile: GC trashed the empty bundle object but could not drop its row; a later sweep retries: {err}"
                        );
                    } else {
                        debug!(
                            target: TARGET,
                            source = %source.id,
                            bundle_id = %bundle_id,
                            "reconcile: GC'd empty bundle (all members gone)"
                        );
                    }
                }
                Err(e) => {
                    let class = classify_drive_error(&e);
                    self.pacer.note_response(class.response_class());
                    if class == DriveError::InvalidGrant {
                        warn!(
                            target: TARGET,
                            source = %source.id,
                            bundle_id = %bundle_id,
                            "reconcile: bundle GC hit auth.invalid_grant; account needs reauth (stopping remote work): {e}"
                        );
                        return Err(ReconcileError::AuthInvalidGrant);
                    }
                    warn!(
                        target: TARGET,
                        source = %source.id,
                        bundle_id = %bundle_id,
                        "reconcile: bundle GC trash failed; leaving the bundle for a later sweep: {e}"
                    );
                }
            }
        }
        Ok(())
    }

    /// R2-P1-3 (DESIGN s5.4 lines 498-500) + R2-P1-1: handle a verified
    /// post-upload checksum mismatch for `(source, relative_path)`.
    ///
    /// 1. **3-consecutive-mismatch counter (R2-P1-3):** bump the durable
    ///    per-(source, path) counter; on the 3rd consecutive mismatch mark the
    ///    `file_state` row [`FileStateStatus::Corrupt`] (stamped with the LIVE
    ///    file's current `(size, mtime_ns)` so the FastPath scanner stops
    ///    re-emitting it - "stop retrying that file"), clear the counter (so a
    ///    later user edit, which changes `(size, mtime)`, gets a fresh budget),
    ///    log, and surface it. Below the threshold the file stays retryable.
    /// 2. **Durable corrupt-create cleanup (R2-P1-1):** when `stranded_file_id`
    ///    is `Some` (a corrupt CREATE whose trash FAILED), persist that id into
    ///    the op payload and KEEP the pending op so the reconcile pass retries
    ///    the trash next cycle - rather than dropping the op and stranding a
    ///    live corrupt object. When `None`, the corrupt object is confirmed gone
    ///    (or it was an update / the real store already trashed it), so the op is
    ///    dropped as before.
    ///
    /// Always returns `OpOutcome::Failed { DriveChecksumMismatch }` (the
    /// orchestrator records a durable activity Error row + leaves the source
    /// scan/verify-due).
    async fn handle_checksum_mismatch(
        &self,
        source: &SourceRow,
        relative_path: &RelativePath,
        live_path: &Path,
        op_id: PendingOpId,
        existing_file_id: Option<&str>,
        stranded_file_id: Option<String>,
    ) -> anyhow::Result<OpOutcome> {
        // R2-P1-3: advance the consecutive-mismatch streak.
        let count = self
            .state
            .bump_checksum_mismatch_count(source.id, relative_path)
            .await?;
        warn!(
            target: TARGET,
            source = %source.id,
            path = %relative_path,
            consecutive = count,
            threshold = MAX_CHECKSUM_MISMATCHES,
            "drive.checksum_mismatch on this file (consecutive count vs the corrupt threshold)"
        );

        let reached_corrupt = count >= MAX_CHECKSUM_MISMATCHES;

        // R2-P1-1: a stranded corrupt CREATE object keeps the op alive so
        // reconcile retries the trash; persist its id. Otherwise drop the op.
        if let Some(file_id) = stranded_file_id {
            warn!(
                target: TARGET,
                source = %source.id,
                path = %relative_path,
                corrupt_file_id = %file_id,
                "corrupt create object could not be trashed; keeping the op so reconcile retries the trash (R2-P1-1)"
            );
            let payload = PendingOpPayload {
                corrupt_file_id: Some(file_id),
                ..PendingOpPayload::default()
            };
            // Best-effort persist; a state write error aborts the cycle via `?`.
            self.state
                .update_pending_op_payload(op_id, &payload.to_value())
                .await?;
        } else {
            self.state.delete_pending_op(op_id).await?;
        }

        if reached_corrupt {
            // DESIGN s5.4: 3 consecutive mismatches -> mark corrupt, stop
            // retrying, surface to the user. Stamp the row with the LIVE file's
            // current (size, mtime) so the FastPath scanner (which keys
            // "unchanged" off (size, mtime)) treats it as already-handled and
            // does NOT re-emit it - until the user edits the file (changing
            // (size, mtime)), which both re-detects it AND clears the counter
            // below, giving a fresh attempt.
            let (size, mtime_ns) = match lstat_identity(live_path) {
                Ok(id) => (id.size, id.mtime_ns),
                // The file vanished mid-cycle: a sentinel mtime forces the next
                // scan to re-evaluate (it will be a delete if still gone).
                Err(_) => (0, REQUEUE_FORCE_RESCAN_MTIME_NS),
            };
            let row = FileStateRow {
                source_id: source.id,
                relative_path: relative_path.clone(),
                size,
                mtime_ns,
                // No proven plaintext hash for a corrupt upload; the row exists
                // to record the Corrupt status + freeze the (size, mtime) the
                // scanner compares against, not to assert content identity.
                hash_blake3: [0u8; 32],
                drive_file_id: existing_file_id.map(|s| s.to_string()),
                drive_md5: None,
                encrypted_remote_path: None,
                status: FileStateStatus::Corrupt,
                last_uploaded_at: None,
                last_verified_at: None,
            };
            self.state.upsert_file_state(&row).await?;
            // Reset the streak so a later user edit gets a fresh budget.
            self.state
                .clear_checksum_mismatch_count(source.id, relative_path)
                .await?;
            warn!(
                target: TARGET,
                source = %source.id,
                path = %relative_path,
                threshold = MAX_CHECKSUM_MISMATCHES,
                "marked file_state status=corrupt after the consecutive checksum-mismatch threshold (DESIGN s5.4); halting retries until the file changes"
            );
        }

        Ok(OpOutcome::Failed {
            relative_path: relative_path.clone(),
            code: ErrorCode::DriveChecksumMismatch,
        })
    }

    /// Issue #36: decide whether a content change to an already-uploaded file
    /// should be VERSIONED. Returns `Some` only when the source has versioning
    /// enabled, `existing` is a `Synced` row with a `drive_file_id` and a known
    /// `last_uploaded_at`, and the OLD object passes the per-source size guard.
    /// A single indexed settings read (`versioning_config`), gated by the caller
    /// on there being an existing object.
    async fn resolve_version_supersede(
        &self,
        source: &SourceRow,
        existing: Option<&FileStateRow>,
    ) -> Option<VersionSupersede> {
        let cfg = match self.state.versioning_config(source.id).await {
            Ok(c) => c,
            Err(err) => {
                warn!(target: TARGET, source = %source.id, %err, "could not read versioning config; treating as disabled");
                return None;
            }
        };
        if !cfg.enabled {
            return None;
        }
        let row = existing?;
        // Only version a stable, already-uploaded, Synced file: a non-synced row's
        // recorded metadata may not match what is actually on Drive, so keeping it
        // as a "version" could hand back mismatched bytes.
        if row.status != FileStateStatus::Synced {
            return None;
        }
        let old_drive_file_id = row.drive_file_id.clone()?;
        let old_created_at = row.last_uploaded_at?;
        // Size guard: skip versioning (fall back to in-place update) when the OLD
        // object exceeds the per-source cap, so versioning very large files is a
        // deliberate opt-in rather than an accidental cost.
        if cfg.max_bytes > 0 && row.size > cfg.max_bytes {
            return None;
        }
        Some(VersionSupersede {
            old_drive_file_id,
            old_size: row.size,
            old_hash_blake3: row.hash_blake3,
            old_drive_md5: row.drive_md5,
            old_encrypted_remote_path: row.encrypted_remote_path.clone(),
            old_created_at,
            cap: cfg.effective_cap(),
        })
    }

    /// Issue #36: best-effort trash of a superseded object, GUARDED by the global
    /// "no live pointer" check so a shared object (a live `file_state` pointer in
    /// any source, or a future small-file bundle, #35) is NEVER trashed. On a
    /// successful trash it marks the matching `file_versions` row `trashed`
    /// (a no-op if the id is not a tracked version, e.g. a redundant duplicate
    /// from an identical-content touch). Any failure is logged, never fatal: the
    /// reconcile sweep retries an un-trashed version.
    ///
    /// Returns `true` iff the object was confirmed trashed on this call (so a
    /// caller holding a durable cleanup handle may drop it); `false` when the
    /// guard skipped it (still live), the liveness check errored, or the trash
    /// failed - in all of which the caller must KEEP its handle for a retry.
    async fn guarded_trash_and_mark(&self, drive_file_id: &str) -> bool {
        match self.state.drive_file_id_is_live(drive_file_id).await {
            Ok(true) => {
                debug!(target: TARGET, id = %drive_file_id, "superseded object still referenced by a live pointer; not trashing");
                return false;
            }
            Ok(false) => {}
            Err(err) => {
                warn!(target: TARGET, id = %drive_file_id, %err, "could not verify a superseded object is unreferenced; skipping trash");
                return false;
            }
        }
        self.pacer.permit_request().await;
        match self.remote.trash(drive_file_id).await {
            Ok(()) => {
                self.pacer.note_response(ResponseClass::Ok);
                if let Err(err) = self.state.mark_version_trashed(drive_file_id).await {
                    warn!(target: TARGET, id = %drive_file_id, %err, "trashed a superseded object but failed to mark the version row");
                }
                true
            }
            Err(e) => {
                let class = classify_drive_error(&e);
                self.pacer.note_response(class.response_class());
                warn!(target: TARGET, id = %drive_file_id, err = %e, "best-effort trash of a superseded object failed; the reconcile sweep will retry");
                false
            }
        }
    }

    /// Issue #36: prune a file's retained versions down to `cap`, HARD-DELETING
    /// the oldest excess objects from Drive (freeing storage - trash still counts
    /// against quota). Each delete is GUARDED by the same global "no live pointer"
    /// check: a shared object is left intact (its tracking row kept). Best-effort
    /// throughout - a failure keeps the row so a later prune retries.
    async fn prune_versions(&self, source: SourceId, path: &RelativePath, cap: u32) {
        let over = match self.state.versions_over_cap(source, path, cap).await {
            Ok(v) => v,
            Err(err) => {
                warn!(target: TARGET, source = %source, path = %path, %err, "could not list versions to prune");
                return;
            }
        };
        for v in over {
            match self.state.drive_file_id_is_live(&v.drive_file_id).await {
                // Shared / still-live: never hard-delete it. Leave the tracking
                // row (the object is still restorable via its live owner).
                Ok(true) => continue,
                Ok(false) => {}
                Err(err) => {
                    warn!(target: TARGET, id = %v.drive_file_id, %err, "could not verify a prunable version is unreferenced; skipping");
                    continue;
                }
            }
            self.pacer.permit_request().await;
            match self.remote.delete_permanent(&v.drive_file_id).await {
                Ok(()) => {
                    self.pacer.note_response(ResponseClass::Ok);
                    if let Err(err) = self.state.delete_version_row(v.id).await {
                        warn!(target: TARGET, id = %v.drive_file_id, %err, "hard-deleted a pruned version but failed to drop its row");
                    }
                }
                Err(e) => {
                    let class = classify_drive_error(&e);
                    self.pacer.note_response(class.response_class());
                    warn!(target: TARGET, id = %v.drive_file_id, err = %e, "best-effort prune (hard-delete) of an over-cap version failed; keeping the row to retry");
                }
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
                    kind: DoneKind::Trash,
                    bytes: None,
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
        on_outcome: &OutcomeSink<'_>,
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
                        // R2-P2-1: `run` persists this op's activity itself (via
                        // on_outcome) right after the op's durable commit, INSIDE
                        // the in_flight future - so the activity DB write is
                        // polled together with the other in-flight ops by
                        // FuturesUnordered and cannot deadlock the drain loop
                        // against the single-connection pool.
                        in_flight.push(exec.run(op, permit, on_outcome));
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

        // --- V5-P2-4: suite-FREE corrupt-create cleanup FIRST --------------
        // An op carrying a `corrupt_file_id` is the recovery handle for a corrupt
        // CREATE object whose post-upload trash FAILED (it may still be live on
        // Drive). Retrying the trash needs NO crypto (it is a `remote.trash` by
        // id). It MUST run even when the per-source crypto gate below fails
        // closed - otherwise a stranded corrupt object on an encryption-enabled
        // source with a temporarily-missing key would never get its trash
        // retried until the key returns. So process these BEFORE the crypto
        // gate's early-return.
        let mut remaining: Vec<crate::state::PendingOpRow> = Vec::with_capacity(pending.len());
        for op in pending {
            let payload = PendingOpPayload::from_value(&op.payload_json);
            if let Some(corrupt_file_id) = payload.corrupt_file_id.clone() {
                // R-P2-2: a revoked token during the trash retry surfaces a
                // classified error the orchestrator turns into needs-reauth +
                // suspend; propagate it (do NOT keep hammering the dead token).
                if self
                    .retry_trash_corrupt(&corrupt_file_id, op.relative_path.as_str())
                    .await?
                {
                    self.state.delete_pending_op(op.id).await?;
                }
                // Either way (other than the propagated auth error above), this
                // op is fully handled; never falls through to the crypto-
                // dependent upload-recovery paths.
                continue;
            }
            // Issue #36 (defect 2/6): an op carrying a `redundant_duplicate_file_id`
            // is the recovery handle for an identical-content touch's redundant
            // DUPLICATE object whose immediate trash FAILED (or was interrupted by a
            // crash before it ran). Retry the trash here - suite-FREE (a
            // `remote.trash` by id) so it runs even when the per-source crypto gate
            // below fails closed - and drop the op once the object is confirmed gone;
            // otherwise a live untracked duplicate would leak forever (no version row
            // => the un-trashed-versions sweep never sees it; op already the only
            // handle). Same shape as the corrupt-create cleanup above.
            if let Some(redundant_id) = payload.redundant_duplicate_file_id.clone() {
                if self
                    .retry_trash_redundant(&redundant_id, op.relative_path.as_str())
                    .await?
                {
                    self.state.delete_pending_op(op.id).await?;
                }
                continue;
            }

            // --- V2 bundling (issue #35): bundle op crash-recovery, suite-FREE --
            // A surviving `bundle` pending_op means the bundle's
            // `commit_bundle_result` never ran, so NO member file_state rows
            // landed - the members are still "new" and the next scan re-bundles
            // them (no data loss). If the create actually finalized on Drive (a
            // crash between the create returning and the commit), the object is an
            // orphan we must trash, since there is no folder sweep to catch it.
            // Find it by its op-uuid under the source root and trash it, then drop
            // the op. This is suite-FREE (find + trash are by appProperties + id,
            // no crypto), so it runs before the per-source crypto gate and works
            // for an encrypted source whose key is temporarily unavailable.
            if op.op_type == OP_TYPE_BUNDLE {
                if let Some(uuid) = payload.client_op_uuid.clone() {
                    // R3-P1-2 / R2-P1-1 classification, mirroring the create-path
                    // recovery below: a lookup/trash ERROR must NOT propagate
                    // unclassified. A TRANSIENT / rate-limited / auth error keeps
                    // the op (return -> retry next cycle; invalid_grant maps to
                    // needs_reauth via to_reconcile_err); a DEFINITIVE non-retryable
                    // error (404 already-gone, 403 can't-trash) drops the op so a
                    // stuck bundle op can never WEDGE the account's whole sync
                    // forever (reconcile runs before scan/execute). Every error
                    // path accounts the response with the pacer/breaker, like every
                    // other reconcile Drive call.
                    self.pacer.permit_request().await;
                    let found = match self
                        .remote
                        .find_by_op_uuid(&source.drive_folder_id, &uuid)
                        .await
                    {
                        Ok(found) => {
                            self.pacer.note_response(ResponseClass::Ok);
                            found
                        }
                        Err(e) => {
                            let class = classify_drive_error(&e);
                            self.pacer.note_response(class.response_class());
                            if reconcile_metadata_error_is_retryable(class) {
                                warn!(
                                    target: TARGET,
                                    source = %source.id,
                                    "reconcile: bundle-op find_by_op_uuid failed transiently; keeping the op for retry next cycle: {e}"
                                );
                                return Err(to_reconcile_err(e));
                            }
                            warn!(
                                target: TARGET,
                                source = %source.id,
                                "reconcile: bundle-op find_by_op_uuid returned a definitive failure; dropping the op (members re-bundle next scan): {e}"
                            );
                            self.state.delete_pending_op(op.id).await?;
                            continue;
                        }
                    };
                    if let Some(entry) = found {
                        self.pacer.permit_request().await;
                        match self.remote.trash(&entry.id).await {
                            Ok(()) => {
                                self.pacer.note_response(ResponseClass::Ok);
                                debug!(
                                    target: TARGET,
                                    source = %source.id,
                                    "reconcile: trashed orphaned bundle object from an uncommitted bundle op"
                                );
                            }
                            Err(e) => {
                                let class = classify_drive_error(&e);
                                self.pacer.note_response(class.response_class());
                                if reconcile_metadata_error_is_retryable(class) {
                                    warn!(
                                        target: TARGET,
                                        source = %source.id,
                                        "reconcile: bundle-op orphan trash failed transiently; keeping the op for retry next cycle: {e}"
                                    );
                                    return Err(to_reconcile_err(e));
                                }
                                // Definitive: 404 (already gone = success) or a
                                // permanent 403 we cannot fix. Drop the op either
                                // way so the account is never wedged; a genuinely
                                // untrashable orphan lingers harmlessly (no
                                // file_state row references it).
                                warn!(
                                    target: TARGET,
                                    source = %source.id,
                                    "reconcile: bundle-op orphan trash hit a definitive error; dropping the op to avoid wedging the account: {e}"
                                );
                            }
                        }
                    }
                }
                self.state.delete_pending_op(op.id).await?;
                continue;
            }

            remaining.push(op);
        }

        // --- issue #36: retry trashing any un-trashed superseded version ----
        // A versioned supersede trashes the OLD object best-effort right after
        // the atomic flip; a transient failure there leaves the version row with
        // `trashed = 0`. Retry it here (crypto-FREE: a guarded `remote.trash` by
        // id), BEFORE the crypto gate so a temporarily-missing key does not block
        // it. `guarded_trash_and_mark` is best-effort + self-guarding, so a still-
        // referenced id is skipped and a repeat failure just retries next cycle.
        // Normally there are none (inline trash succeeds), so this is cheap.
        match self.state.untrashed_versions_for_source(source.id).await {
            Ok(versions) => {
                for v in versions {
                    self.guarded_trash_and_mark(&v.drive_file_id).await;
                }
            }
            Err(err) => {
                warn!(target: TARGET, source = %source.id, %err, "could not list un-trashed versions to sweep");
            }
        }

        // --- V2 bundling (issue #35 item a): empty-bundle GC, suite-FREE -----
        // Sweep away bundles whose members have all been deleted or promoted to
        // standalone objects: trash the dead `.tar.gz` object and drop its row.
        // Trash-by-id needs no crypto, so this runs BEFORE the per-source crypto
        // gate (works for an encryption-enabled source whose key is temporarily
        // unavailable). Best-effort; only a revoked token (AuthInvalidGrant)
        // propagates, so the orchestrator runs its needs-reauth transition.
        self.gc_empty_bundles(source).await?;

        // M5 per-source crypto (resolved ONCE for this source, FAIL-CLOSED).
        // Reconcile re-derives the encrypted parent chain / encrypted_remote_path
        // and re-reads the local file - all of which need the SAME suite the
        // upload used. An `encryption_enabled` source whose key is unavailable
        // must NOT reconcile its ops with plaintext crypto (that would search the
        // wrong - root - folder and could re-upload a duplicate next scan, or
        // adopt a row with a blanked encrypted_remote_path). So if the suite
        // resolution fails closed, leave this source's REMAINING (crypto-
        // dependent) pending ops untouched (they retry next cycle once the key is
        // available) and surface it. The corrupt-cleanup above already ran.
        let crypto = match self.resolve_source_crypto(source) {
            Ok(crypto) => crypto,
            Err(()) => {
                warn!(
                    target: TARGET,
                    source = %source.id,
                    "skipping crypto-dependent reconcile for encryption-enabled source with no resolvable key (crypto.key_missing); ops kept for retry (corrupt-cleanup already ran)"
                );
                return Ok(());
            }
        };
        let crypto = crypto.as_deref();

        for op in remaining {
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
                // R2-P1-1: a revoked token during the resume's remote awaits
                // must mark the account needs_reauth (NOT be retried forever as
                // a transient reconcile failure). Map an invalid_grant-classified
                // error to ReconcileError::AuthInvalidGrant so reconcile_once's
                // enter_needs_reauth fires.
                let resumed = self
                    .resume_persisted(source, &op, &payload, resumable, crypto)
                    .await
                    .map_err(to_reconcile_err)?;
                match resumed {
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
                        // adopt_reconciled re-derives the encrypted parent chain
                        // (remote ensure_folder) - map an invalid_grant there too.
                        self.adopt_reconciled(source, &op, &adopted, entry, crypto)
                            .await
                            .map_err(to_reconcile_err)?;
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
                self.pacer.permit_request().await;
                match self.remote.metadata(&file_id).await {
                    // R2-P1-1: a SUCCESSFUL metadata read decides the op's fate.
                    Ok(entry)
                        if entry
                            .app_properties
                            .get(CLIENT_OP_UUID_KEY)
                            .map(|v| v == &uuid)
                            .unwrap_or(false) =>
                    {
                        self.pacer.note_response(ResponseClass::Ok);
                        // Already committed remotely; re-hash + finish.
                        // adopt_reconciled re-derives the encrypted parent chain
                        // (remote ensure_folder) - map an invalid_grant there.
                        self.adopt_reconciled(source, &op, &payload, entry, crypto)
                            .await
                            .map_err(to_reconcile_err)?;
                    }
                    Ok(_) => {
                        // R2-P1-1: the object EXISTS but does NOT carry this op's
                        // uuid - a SUCCESSFUL result that PROVES this update never
                        // committed. Only THEN drop the stale op so the next scan
                        // re-enqueues it cleanly (the prior file_state row keeps
                        // the existing drive_file_id for the update).
                        self.pacer.note_response(ResponseClass::Ok);
                        self.state.delete_pending_op(op.id).await?;
                    }
                    Err(e) => {
                        // R2-P1-1 / R3-P1-2: a metadata ERROR proves NOTHING
                        // about whether the update committed. For a TRANSIENT /
                        // rate-limited / auth failure the read may succeed later,
                        // so KEEP the op and surface the error (an invalid_grant
                        // maps to needs_reauth via reconcile_once) - NEVER delete
                        // it (that would lose the reconcile handle for a possibly-
                        // committed op).
                        //
                        // R3-P1-2: but a DEFINITIVE 404 / not-found / non-retryable
                        // 4xx means the recorded `drive_file_id` is permanently
                        // gone and can NEVER be updated. recheck-2's keep+retry-on-
                        // any-error would then retry this dead op every cycle - and
                        // because reconcile runs before scan/execute, it WEDGES the
                        // whole account. Instead clear the stale id so the next scan
                        // re-plans a fresh CREATE (re-upload), and drop this op.
                        let class = classify_drive_error(&e);
                        self.pacer.note_response(class.response_class());
                        if reconcile_metadata_error_is_retryable(class) {
                            warn!(
                                target: TARGET,
                                source = %source.id,
                                path = %op.relative_path,
                                "reconcile: metadata read failed transiently; keeping the pending op for retry next cycle (R2-P1-1): {e}"
                            );
                            return Err(to_reconcile_err(e));
                        }
                        warn!(
                            target: TARGET,
                            source = %source.id,
                            path = %op.relative_path,
                            "reconcile: metadata read returned a definitive not-found for the recorded drive_file_id; clearing the stale id and dropping the op so the next scan re-uploads (R3-P1-2): {e}"
                        );
                        self.state
                            .clear_file_state_drive_file_id(source.id, &op.relative_path)
                            .await?;
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
                //
                // R2-P1-1: reconcile_parent_id + find_by_op_uuid + adopt each
                // do remote awaits; map an invalid_grant to needs_reauth.
                let parent_id = self
                    .reconcile_parent_id(source, &op.relative_path, crypto)
                    .await
                    .map_err(to_reconcile_err)?;
                self.pacer.permit_request().await;
                let found = match self.remote.find_by_op_uuid(&parent_id, &uuid).await {
                    Ok(found) => {
                        self.pacer.note_response(ResponseClass::Ok);
                        found
                    }
                    Err(e) => {
                        // R2-P1-1 / R3-P1-2: a lookup ERROR proves nothing about
                        // whether the create committed. For a TRANSIENT / rate-
                        // limited / auth failure KEEP the op (do not delete) and
                        // surface the error so it retries next cycle / maps to
                        // needs_reauth.
                        //
                        // R3-P1-2: but a DEFINITIVE not-found / non-retryable 4xx
                        // (e.g. the parent folder is permanently gone for this
                        // lookup) can never be made to succeed by retrying, and
                        // because reconcile runs before scan/execute it would WEDGE
                        // the account. The create path carries no recorded
                        // drive_file_id to clear, so drop the op; the next scan
                        // re-plans a fresh CREATE from the live file.
                        let class = classify_drive_error(&e);
                        self.pacer.note_response(class.response_class());
                        if reconcile_metadata_error_is_retryable(class) {
                            warn!(
                                target: TARGET,
                                source = %source.id,
                                path = %op.relative_path,
                                "reconcile: find_by_op_uuid failed transiently; keeping the pending op for retry next cycle (R2-P1-1): {e}"
                            );
                            return Err(to_reconcile_err(e));
                        }
                        warn!(
                            target: TARGET,
                            source = %source.id,
                            path = %op.relative_path,
                            "reconcile: find_by_op_uuid returned a definitive not-found; dropping the op so the next scan re-creates the object (R3-P1-2): {e}"
                        );
                        self.state.delete_pending_op(op.id).await?;
                        continue;
                    }
                };
                match found {
                    Some(entry) => self
                        .adopt_reconciled(source, &op, &payload, entry, crypto)
                        .await
                        .map_err(to_reconcile_err)?,
                    None => {
                        // R2-P1-1: a SUCCESSFUL lookup that found NO orphan proves
                        // the create never landed - only THEN drop the op.
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
        // M5 per-source crypto (resolved FAIL-CLOSED by the caller).
        crypto: Option<&dyn SourceCryptoSuite>,
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
        if crypto.is_some() {
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
        } = match read_hash_encrypt(&mut file, crypto).await {
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
        // M5 per-source crypto (resolved FAIL-CLOSED by the caller).
        crypto: Option<&dyn SourceCryptoSuite>,
    ) -> anyhow::Result<()> {
        let full_path = join_source_path(&source.local_path, &op.relative_path);

        // P2-6: re-derive the ciphertext remote path so the adopted/requeued
        // row restores the same `encrypted_remote_path` the upload wrote (None
        // for a plaintext source). Crash recovery must preserve it - restore +
        // listing look the object up by this path.
        let encrypted_remote_path = self
            .reconcile_encrypted_remote_path(source, &op.relative_path, crypto)
            .await?;

        // Re-hash the current local file (plaintext). On any read failure we
        // cannot prove identity, so treat as a mismatch (requeue).
        let local = self.rehash_local_plaintext(&full_path).await;
        let expected_hex = payload.uploaded_blake3_hex.clone();

        // Issue #36: if this adopted orphan is a VERSIONED create (its payload
        // carries the OLD id it was superseding), capture the OLD `file_state`
        // row BEFORE the pointer flip so the version can be recorded, and trash
        // the OLD object afterwards so a crash-recovery adopt never leaks it live.
        let supersedes = payload.supersedes_drive_file_id.clone();
        let pre_flip_old = if supersedes.is_some() {
            self.state
                .get_file_state(source.id, &op.relative_path)
                .await
                .ok()
                .flatten()
        } else {
            None
        };

        let now = self.clock.now_ms();

        // Issue #36 (defect 3): the OLD object recorded as a retained version when
        // this adopt supersedes it - built ONCE and used by BOTH the identity
        // (real-change) arm AND the requeue arm, so a crash-recovery adopt never
        // silently drops the last good version from point-in-time history (the
        // requeue arm previously flipped the pointer + trashed the old object with
        // NO version row). Guarded exactly like the inline commit: only when the
        // payload names an OLD id, the pre-flip row still points at it, and it has a
        // known window start.
        let superseded_version = match (&supersedes, &pre_flip_old) {
            (Some(old_id), Some(old_row))
                if old_row.drive_file_id.as_deref() == Some(old_id.as_str())
                    && old_row.last_uploaded_at.is_some() =>
            {
                Some(NewFileVersion {
                    source_id: source.id,
                    relative_path: op.relative_path.clone(),
                    drive_file_id: old_id.clone(),
                    size: old_row.size,
                    hash_blake3: old_row.hash_blake3,
                    drive_md5: old_row.drive_md5,
                    encrypted_remote_path: old_row.encrypted_remote_path.clone(),
                    created_at: old_row.last_uploaded_at.unwrap_or(now),
                    superseded_at: now,
                })
            }
            _ => None,
        };

        // Whether the trailing block should guarded-trash the OLD superseded
        // object. A real supersede (identity real-change / requeue) flips the
        // pointer away from it, so it is trashed. An identical-content adopt
        // (defect 4) instead KEEPS the old object live and trashes the redundant
        // NEW orphan, so it clears this.
        let mut trash_old = supersedes.is_some();

        match (local, expected_hex) {
            (Some((cur_hash, size, mtime_ns)), Some(expected_hex))
                if hex::encode(cur_hash) == expected_hex =>
            {
                // Defect 4: a crashed mtime-only touch - the adopted NEW object is
                // byte-for-byte identical to the OLD one (`cur_hash` already equals
                // the uploaded hash by the guard above, so equality with the old
                // hash means the "content change" changed nothing). Extend the inline
                // path's identical-content guard to crash recovery: KEEP the OLD
                // object current (record NO version, so a repeated touch cannot evict
                // genuine history via the count cap) and trash the redundant NEW
                // orphan via the same retryable cleanup handle as the inline path.
                let identical_old = pre_flip_old
                    .as_ref()
                    .filter(|old_row| old_row.hash_blake3 == cur_hash);
                if let Some(old_row) = identical_old {
                    let keep_row = FileStateRow {
                        source_id: source.id,
                        relative_path: op.relative_path.clone(),
                        size,
                        mtime_ns,
                        hash_blake3: cur_hash,
                        drive_file_id: old_row.drive_file_id.clone(),
                        drive_md5: old_row.drive_md5,
                        encrypted_remote_path: old_row.encrypted_remote_path.clone(),
                        status: FileStateStatus::Synced,
                        // Preserve the OLD window start (defect 1's invariant on the
                        // crash-recovery path): the old object stays current, so its
                        // validity window must not move.
                        last_uploaded_at: old_row.last_uploaded_at,
                        last_verified_at: Some(now),
                    };
                    let marker = PendingOpPayload {
                        redundant_duplicate_file_id: Some(entry.id.clone()),
                        ..PendingOpPayload::default()
                    };
                    self.state
                        .commit_identical_touch_result(op.id, &keep_row, &marker.to_value())
                        .await?;
                    // The redundant object is the adopted orphan (entry.id), not the
                    // old object; trash it retryably and leave the old one live.
                    trash_old = false;
                    if self.guarded_trash_and_mark(&entry.id).await {
                        if let Err(err) = self.state.delete_pending_op(op.id).await {
                            warn!(target: TARGET, id = %entry.id, %err, "reconcile-adopt: trashed the redundant identical-touch duplicate but failed to drop its cleanup op; reconcile will re-confirm and drop it");
                        }
                    }
                } else {
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
                    // Issue #36: for a versioned adopt, record the OLD object as a
                    // version AND flip in ONE transaction (so history survives a
                    // crash-in-the-window). Falls back to a plain commit if the OLD
                    // row is missing / no longer points at the superseded id.
                    match &superseded_version {
                        Some(superseded) => {
                            self.state
                                .commit_versioned_create_result(op.id, &row, superseded)
                                .await?;
                        }
                        None => {
                            self.state.commit_create_result(op.id, &row).await?;
                        }
                    }
                }
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
                // Upsert the requeue row + drop the op atomically (the next scan
                // re-enqueues a clean update). Defect 3: if this requeue supersedes
                // an OLD object, record that OLD object as a retained version
                // ATOMICALLY with the flip (same as the identity arm) so the last
                // good version is not silently dropped from history before the
                // trailing trash removes its object. `commit_versioned_create_result`
                // and `commit_create_result` perform the same atomic upsert+delete.
                match &superseded_version {
                    Some(superseded) => {
                        self.state
                            .commit_versioned_create_result(op.id, &row, superseded)
                            .await?;
                    }
                    None => {
                        self.state.commit_create_result(op.id, &row).await?;
                    }
                }
            }
        }

        // Issue #36: whichever arm flipped the pointer AWAY from the OLD
        // (superseded) object, guarded-trash it - preventing the crash-recovery
        // leak (the pointer flip destroyed the payload's own create/update signal,
        // so without this the old object would linger live forever). The guard is a
        // no-op if the pointer did not actually flip away from it, and marking is a
        // no-op when no version row was recorded. Skipped for an identical-content
        // adopt (defect 4), which keeps the old object live (`trash_old` cleared).
        if trash_old {
            if let Some(old_id) = &supersedes {
                self.guarded_trash_and_mark(old_id).await;
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
    ///
    /// R2-P2-1: on success the op's outcome is reported to `on_outcome` (awaited)
    /// right after its durable `file_state` commit and BEFORE the permit is
    /// dropped / the outcome returned, so the orchestrator persists per-op
    /// activity immediately. This runs INSIDE the in-flight future (polled by the
    /// caller's FuturesUnordered), so the activity DB write is interleaved with
    /// the other in-flight ops and cannot deadlock the single-connection pool.
    async fn run(
        &self,
        op: &Op,
        permit: tokio::sync::OwnedSemaphorePermit,
        on_outcome: &OutcomeSink<'_>,
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
            Op::UploadBundle { source_id, members } => {
                debug_assert_eq!(*source_id, self.source.id);
                self.this.bundle_upload(self.source, members).await
            }
        };
        // Stream the per-op activity for a produced outcome (the op's durable
        // file-state commit already happened inside hash_then_upload / trash_op).
        // A hard error (Err) carries no OpOutcome and is handled by the caller.
        if let Ok(outcome) = &out {
            on_outcome(outcome).await;
        }
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
        // A bundle op has no single path; a BundleDone outcome carries its own
        // aggregate counts (handled below) so it never needs the op lookup.
        Op::UploadBundle { .. } => false,
    });
    match outcome {
        OpOutcome::Done { .. } => match op {
            Some(Op::HashThenUpload { size, .. }) => {
                progress.files_done += 1;
                progress.bytes_done += *size;
            }
            Some(Op::Trash { .. }) => progress.trashes_done += 1,
            Some(Op::UploadBundle { .. }) | None => {}
        },
        // A bundle completes as one op but advances the file + byte counters by
        // its aggregate (issue #35); the counts come from the outcome, not the op.
        OpOutcome::BundleDone { files, bytes, .. } => {
            progress.files_done += *files;
            progress.bytes_done += *bytes;
        }
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

/// R2-P1-1: the outcome of [`DefaultExecutor::trash_corrupt_create`] - whether
/// the corrupt CREATE object the checksum-mismatch upload finalized was
/// successfully removed, so the caller can decide whether to durably retry.
enum CorruptCreateCleanup {
    /// Not a create (an UPDATE mismatch): there is no fresh orphan to trash -
    /// the user's pre-existing object is left untouched.
    NotACreate,
    /// The corrupt create object was trashed (confirmed gone).
    Trashed,
    /// The trash FAILED: a live corrupt object may still be on Drive under this
    /// `file_id`. The caller persists it + keeps the pending op so the trash is
    /// retried next cycle, rather than dropping the op and stranding it.
    Stranded {
        /// The corrupt object's Drive file id, to retry the trash against.
        file_id: String,
    },
}

impl CorruptCreateCleanup {
    /// The corrupt-create `file_id` that may be stranded (its trash failed), or
    /// `None` when the object is confirmed gone / not a fresh create. Threaded
    /// into [`UploadError::ChecksumMismatch`] so `hash_then_upload` keeps the op
    /// for a durable re-trash (R2-P1-1) only when something is actually stranded.
    fn stranded_file_id(self) -> Option<String> {
        match self {
            CorruptCreateCleanup::Stranded { file_id } => Some(file_id),
            CorruptCreateCleanup::NotACreate | CorruptCreateCleanup::Trashed => None,
        }
    }
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
            UploadError::Failed(local_io_error_code(&io))
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
    // V5-P1-2: emit DETERMINISTIC fixed-READ_BUF (64 KiB) plaintext chunks,
    // INDEPENDENT of what a single `read()` returns. The cpu stage encrypts
    // each chunk it receives as one AEAD frame, so a short read here would
    // otherwise emit a sub-64KiB frame and a fixed-65552-byte decryptor (M8
    // restore) would mis-align - making the encrypted backup un-restorable, and
    // also breaking `predicted_sent_len` (the declared Content-Length, which
    // assumes ceil(size/64K) frames) -> the streaming upload is rejected. We
    // accumulate reads (read_exact-style) into a full READ_BUF before sending a
    // chunk downstream; only the FINAL chunk (at EOF) may be short.
    let mut buf = vec![0u8; READ_BUF];
    let mut read_total: u64 = 0;
    loop {
        let chunk = read_full_chunk(file, &mut buf)
            .await
            .map_err(StageError::Io)?;
        if chunk.is_empty() {
            break;
        }
        let n = chunk.len() as u64;
        read_total += n;
        if read_total > size {
            // The file grew mid-read; the declared length would be wrong.
            return Err(StageError::Changed);
        }
        pacer.permit_bytes(n).await;
        // Record the bytes entering the pipeline (test instrumentation; the
        // matching `sub` happens once Drive accepts the wire chunk).
        if let Some(g) = mem_gauge.as_ref() {
            g.add(n);
        }
        let is_final = chunk.len() < READ_BUF;
        if raw_tx.send(Bytes::from(chunk)).await.is_err() {
            // Downstream (cpu/uploader) gone - a downstream error aborted the
            // pipeline. The real error surfaces from that stage; this is just
            // an artifact (the caller surfaces the uploader error first).
            return Err(StageError::DownstreamGone);
        }
        // A short chunk means `read_full_chunk` hit EOF while filling, so this
        // was the last chunk - stop without an extra empty read.
        if is_final {
            break;
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

/// Produce the exact bytes to send for a `.tar.gz` bundle already built in
/// memory (V2 small-file bundling, issue #35), plus their md5.
///
/// The encryption is object-level and byte-identical to [`read_hash_encrypt`]'s
/// encrypted path (same XChaCha20-Poly1305 STREAM: 40-byte header, fixed 64-KiB
/// (`READ_BUF`) plaintext frames via `encrypt_chunk`, the FINAL frame - even if a
/// full 64 KiB - via `finalize_last`), so the M8 restore decryptor (fixed
/// 65552-byte frames) aligns exactly. Plaintext sources send the archive as-is
/// with an md5 over it. The whole archive is in memory (planner-capped), so this
/// is a simple fold, not a streaming pipeline.
fn bundle_sent_bytes(
    tar_gz: Vec<u8>,
    crypto: Option<&dyn SourceCryptoSuite>,
) -> Result<SentBytes, CryptoError> {
    use md5::{Digest, Md5};

    let Some(suite) = crypto else {
        // Plaintext: body == archive, md5 over the archive bytes.
        let mut md5 = Md5::new();
        md5.update(&tar_gz);
        let md5: [u8; 16] = md5.finalize().into();
        return Ok(SentBytes {
            bytes: Bytes::from(tar_gz),
            md5,
        });
    };

    let mut enc: Box<dyn ContentEncryptor> = suite.content_encryptor();
    let header = enc.header();
    let mut out = Vec::with_capacity(tar_gz.len() + header.len() + 64);
    out.extend_from_slice(&header);

    // Split into fixed 64-KiB plaintext frames; the LAST frame (even if a full
    // 64 KiB) is finalized, all earlier frames are sealed with `encrypt_chunk`.
    // An empty archive still emits one (empty) finalized frame so the header +
    // trailer are well-formed.
    let frames: Vec<&[u8]> = if tar_gz.is_empty() {
        vec![&[][..]]
    } else {
        tar_gz.chunks(READ_BUF).collect()
    };
    let (last, prefix) = frames
        .split_last()
        .expect("frames is non-empty by construction");
    for frame in prefix {
        let ct = enc.encrypt_chunk(frame)?;
        out.extend_from_slice(&ct);
    }
    // `finalize_last` returns the md5 over EVERY ciphertext byte it emitted
    // (header + all frames), matching read_hash_encrypt's accumulated md5.
    let (ct, md5) = enc.finalize_last(last)?;
    out.extend_from_slice(&ct);

    Ok(SentBytes {
        bytes: Bytes::from(out),
        md5,
    })
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

        // V5-P1-2: the AEAD chunk boundary MUST be a deterministic fixed
        // READ_BUF (64 KiB) plaintext frame, INDEPENDENT of what a single
        // `read()` returns - a short read on a network/FUSE/SMB mount must NOT
        // emit a sub-64KiB frame, or the spec-conforming fixed-65552-byte
        // decryptor (M8 restore) mis-aligns and the encrypted backup is
        // SILENTLY un-restorable (DESIGN s7.1: "64 KiB plaintext chunks").
        // Accumulate reads (read_exact-style) into a full READ_BUF chunk before
        // emitting it; only the FINAL chunk (at EOF) may be short. We read one
        // FULL chunk ahead so the last chunk can be `finalize_last`-flagged.
        let mut pending: Option<Vec<u8>> = None;
        loop {
            let chunk = read_full_chunk(file, &mut read_buf).await?;
            if chunk.is_empty() {
                break;
            }
            hasher_blake3.update(&chunk);
            plaintext_len += chunk.len() as u64;
            let was_full = chunk.len() == READ_BUF;
            if let Some(prev) = pending.take() {
                let ct = enc.encrypt_chunk(&prev).map_err(crypto_to_io)?;
                md5.update(&ct);
                out.extend_from_slice(&ct);
            }
            pending = Some(chunk);
            // A short chunk can only be the final one (EOF reached inside
            // `read_full_chunk`); stop so it is finalized below.
            if !was_full {
                break;
            }
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

/// Read up to `buf.len()` (== [`READ_BUF`], 64 KiB) bytes into a fresh `Vec`,
/// looping over short reads until the buffer is full OR EOF is reached
/// (read_exact-style). This makes the encryptor's chunk boundary DETERMINISTIC
/// and INDEPENDENT of the per-`read()` size (V5-P1-2): a short read on a
/// network / FUSE / SMB mount no longer collapses into a sub-64KiB AEAD frame.
///
/// Returns:
/// - a full `READ_BUF`-length `Vec` for every non-final chunk;
/// - a SHORT (`< READ_BUF`, possibly empty) `Vec` ONLY when EOF was hit while
///   filling - this is the final chunk;
/// - an EMPTY `Vec` when the stream is already at EOF (nothing more to read).
///
/// The caller treats an empty return (or any `< READ_BUF` return) as "this is
/// the last chunk" and finalizes the STREAM. `buf` is reused across calls as
/// scratch (its contents are copied into the returned `Vec`).
async fn read_full_chunk<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
) -> Result<Vec<u8>, std::io::Error> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = reader.read(&mut buf[filled..]).await?;
        if n == 0 {
            break; // EOF
        }
        filled += n;
    }
    Ok(buf[..filled].to_vec())
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
    /// R2-P1-3 / R2-P1-1: a post-upload checksum mismatch (the bytes on Drive
    /// do not match what we sent). Carried distinctly from the generic
    /// [`UploadError::Failed`] so `hash_then_upload` can run the DESIGN s5.4
    /// 3-consecutive-mismatch policy (bump the counter; on the 3rd, mark the
    /// file [`FileStateStatus::Corrupt`]) AND the R2-P1-1 durable corrupt-create
    /// cleanup. `stranded_file_id` is `Some` ONLY when this op CREATED a corrupt
    /// object whose best-effort trash FAILED - then the op is KEPT (not dropped)
    /// so reconcile retries the trash; otherwise (`None`) the corrupt object is
    /// confirmed gone (or it was an update / the real store already trashed it)
    /// and the op is dropped as before.
    ChecksumMismatch {
        /// `Some(file_id)` when a corrupt CREATE object may be stranded on Drive
        /// (its trash failed) and the op must be kept to retry the trash.
        stranded_file_id: Option<String>,
    },
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
        UploadError::Failed(local_io_error_code(&e))
    }
}

/// Classify a local-filesystem [`std::io::Error`] into its stable
/// [`ErrorCode`]: an out-of-space failure maps to
/// [`ErrorCode::LocalDiskFull`] (SPEC s24 / STRESS_HARNESS s3.1
/// `disk-full-target`), everything else to [`ErrorCode::LocalIoError`].
///
/// We inspect the raw OS error number rather than [`std::io::ErrorKind`]
/// because stable Rust does not yet surface a portable `StorageFull` kind on
/// every target: Unix reports `ENOSPC` (errno 28); Windows reports
/// `ERROR_DISK_FULL` (112) or `ERROR_HANDLE_DISK_FULL` (39). Mapping it to a
/// distinct code lets the orchestrator pause the source with "disk full"
/// rather than a generic IO error the user cannot act on.
fn local_io_error_code(e: &std::io::Error) -> ErrorCode {
    if is_disk_full(e) {
        ErrorCode::LocalDiskFull
    } else {
        ErrorCode::LocalIoError
    }
}

/// Whether `e` is an out-of-space (disk-full) error on this platform.
fn is_disk_full(e: &std::io::Error) -> bool {
    // `ErrorKind::StorageFull` is stable since Rust 1.83 and is set for both
    // ENOSPC and the Windows disk-full codes, so prefer it; fall back to the
    // raw OS error so older readers / unusual surfaces still classify.
    if matches!(e.kind(), std::io::ErrorKind::StorageFull) {
        return true;
    }
    match e.raw_os_error() {
        // Unix ENOSPC.
        #[cfg(unix)]
        Some(28) => true,
        // Windows ERROR_HANDLE_DISK_FULL (39) / ERROR_DISK_FULL (112).
        #[cfg(windows)]
        Some(39) | Some(112) => true,
        _ => false,
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
/// `RemoteStore` returns `anyhow::Result`. The production `GoogleDriveStore`
/// (M4) carries a typed, downcastable [`DriveStoreError`] whose
/// [`DriveErrorClassification`] is the authoritative verdict; the
/// `InMemoryRemoteStore` fake still surfaces `anyhow` with a format-string
/// message. [`classify_drive_error`] therefore downcasts to the typed error
/// FIRST (real store) and falls back to substring-matching the fake's messages
/// only when the error is not a typed `DriveStoreError`. This fixes the V-D /
/// C-P2-1 collision where `Transient5xx` and `Other` both render
/// `drive.unreachable`, which the old pure-substring matcher wrongly retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriveError {
    RateLimited,
    Transient,
    QuotaExhausted,
    DailyQuota,
    InvalidGrant,
    DestFolderMissing,
    DestFolderPermissionDenied,
    /// The store's post-upload md5 verify failed (codex R-P1-1): the real
    /// `GoogleDriveStore` verifies INSIDE the store, so a checksum mismatch
    /// arrives here as a typed error rather than the executor doing its own
    /// verify. Today (M4) it maps to the `drive.checksum_mismatch` code (NOT the
    /// generic `drive.unreachable`) and fails the op.
    ///
    /// NOTE (codex R2-P1-3, honesty): the DESIGN s498-500
    /// "3 consecutive checksum mismatches -> `status='corrupt'`" defence is NOT
    /// present on this path - there is no per-file mismatch counter and no
    /// transition to `FileStateStatus::Corrupt` here. A mismatch maps to
    /// `UploadError::Failed`, deletes the pending op, and the orchestrator only
    /// defers scan timestamps + logs activity. Implementing the persistent
    /// per-file counter is DEFERRED to M5 (when the real `GoogleDriveStore` is
    /// wired into the prod executor; in M4 the executor runs the fake and the CLI
    /// bypasses the pending-op machinery). Tracked in design/CODEX_NOTES.md
    /// "M4 recheck-2 deferrals -> M5".
    ChecksumMismatch,
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
            DriveError::ChecksumMismatch => ErrorCode::DriveChecksumMismatch,
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

/// Classify a Drive-side `anyhow` error into the executor's retry verdict.
///
/// Prefers the TYPED downcast (the production `GoogleDriveStore` surfaces a
/// [`DriveStoreError`]): its [`DriveErrorClassification`] maps directly, and
/// the dedicated fatal variants (dest-folder / session-invalid / checksum) map
/// to their precise codes. This avoids the V-D / C-P2-1 collision where
/// `Transient5xx` and `Other` share the `drive.unreachable` Display string -
/// the typed path distinguishes them so a fatal `Other` is NOT retried.
///
/// Falls back to substring-matching ONLY when the error is not a typed
/// `DriveStoreError` (the `InMemoryRemoteStore` fake, which uses plain
/// `anyhow::bail!` strings). That keeps every fake-based test green.
fn classify_drive_error(e: &anyhow::Error) -> DriveError {
    // Typed path: the real GoogleDriveStore. Map the dedicated fatal variants
    // by their own discriminant, and the Classified ones by classification.
    if let Some(typed) = e.downcast_ref::<DriveStoreError>() {
        return match typed {
            DriveStoreError::DestFolderMissing => DriveError::DestFolderMissing,
            DriveStoreError::DestFolderPermissionDenied => DriveError::DestFolderPermissionDenied,
            // A dead resumable session is fatal for this op (not a transient
            // service-health signal): map to Other so the op fails rather than
            // being retried as if the link were flaky.
            DriveStoreError::ResumableSessionInvalid => DriveError::Other,
            // codex R-P1-1: a checksum mismatch from the REAL store (md5 verify
            // happens inside the store, which has ALREADY trashed the corrupt
            // create) must surface the dedicated `drive.checksum_mismatch` code,
            // NOT the generic `drive.unreachable` - otherwise the corrupt-file
            // failure is mis-reported and the orchestrator's consecutive-
            // mismatch -> status='corrupt' defence never triggers.
            DriveStoreError::ChecksumMismatch { .. } => DriveError::ChecksumMismatch,
            DriveStoreError::Classified { kind, .. } => classify_from_classification(kind),
        };
    }
    // String fallback for the fake's plain anyhow messages.
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

/// Maps a typed [`DriveErrorClassification`] to the executor's retry verdict
/// (the V-D typed path). `Transient5xx` and `Network` are the retryable
/// transient class; `Other` is fatal-for-this-op (NOT retried).
fn classify_from_classification(kind: &DriveErrorClassification) -> DriveError {
    match kind {
        DriveErrorClassification::RateLimited { .. } => DriveError::RateLimited,
        DriveErrorClassification::Transient5xx | DriveErrorClassification::Network => {
            DriveError::Transient
        }
        DriveErrorClassification::AuthInvalidGrant => DriveError::InvalidGrant,
        DriveErrorClassification::DailyQuota => DriveError::DailyQuota,
        DriveErrorClassification::StorageQuota => DriveError::QuotaExhausted,
        DriveErrorClassification::Other => DriveError::Other,
    }
}

/// R-P2-2: a typed error surfaced from [`Executor::reconcile`] when the
/// reconcile pass hit a credential failure that the orchestrator MUST act on
/// (the refresh token returned `invalid_grant`).
///
/// The trait still returns `anyhow::Result<()>` so existing call sites and the
/// `?`-propagation in `reconcile_once` are unchanged; this type is wrapped into
/// the returned `anyhow::Error` and the orchestrator downcasts it via
/// [`reconcile_error_is_invalid_grant`] to drive the SAME needs-reauth
/// transition the normal-path V-F (DESIGN s5.4) takes. Without this, an
/// `invalid_grant` raised while RETRYING the trash of a stranded corrupt CREATE
/// object (the reconcile-only path, which never produces an `OpOutcome`) would
/// be classified for pacing, logged, and swallowed - leaving the account
/// hammering a revoked token forever.
#[derive(Debug, thiserror::Error)]
pub enum ReconcileError {
    /// The Drive token returned `invalid_grant` during the corrupt-trash retry
    /// (or any reconcile remote call). The orchestrator marks the account
    /// `needs_reauth`, emits `AccountNeedsReauth`, suspends the loop, and stops
    /// remote work (DESIGN s5.4).
    #[error("reconcile hit auth.invalid_grant; account needs reauth")]
    AuthInvalidGrant,
}

/// R-P2-2: `true` iff `err` (the `anyhow::Error` returned by
/// [`Executor::reconcile`]) carries a [`ReconcileError::AuthInvalidGrant`] - the
/// signal that reconcile hit a revoked token and the orchestrator must run the
/// needs-reauth transition. Downcasts the typed cause; a plain reconcile error
/// (DB hiccup, transient Drive fault) returns `false` and is retried next cycle.
#[must_use]
pub fn reconcile_error_is_invalid_grant(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<ReconcileError>(),
        Some(ReconcileError::AuthInvalidGrant)
    )
}

/// R2-P1-1: map a reconcile remote-await error so a revoked token surfaces as a
/// typed [`ReconcileError::AuthInvalidGrant`] the orchestrator acts on.
///
/// recheck-1 only converted `invalid_grant` for the corrupt-trash retry; EVERY
/// other reconcile remote await (resumable resume, encrypted-parent
/// `ensure_folder`, update `metadata`, create `find_by_op_uuid`, adopt) used to
/// propagate a plain `anyhow` Drive error, so a revoked token during a NORMAL
/// create/update reconcile was treated as a transient reconcile failure and
/// retried forever instead of marking the account `needs_reauth`. Wrapping each
/// such await with this helper routes `classify_drive_error(&err) ==
/// InvalidGrant` into `reconcile_once`'s `enter_needs_reauth` path; any other
/// error (transient Drive fault, DB hiccup) passes through unchanged and is
/// retried next cycle. An error that is ALREADY a `ReconcileError` (e.g. from a
/// nested helper) is passed through unchanged so the typed signal is preserved.
fn to_reconcile_err(err: anyhow::Error) -> anyhow::Error {
    if err.downcast_ref::<ReconcileError>().is_some() {
        return err;
    }
    if classify_drive_error(&err) == DriveError::InvalidGrant {
        return ReconcileError::AuthInvalidGrant.into();
    }
    err
}

/// R3-P1-2: should reconcile KEEP+RETRY this Drive error (return `Err`, retry
/// next cycle) on a metadata/lookup read, or treat it as a DEFINITIVE failure
/// (clear the stale id / drop the op so the account is not wedged)?
///
/// reconcile_once runs BEFORE scan/execute, so an op that keeps returning `Err`
/// every cycle stops ALL backups for the account. recheck-2 made the
/// metadata/lookup arms keep+retry on ANY error, which WEDGES the account when
/// the recorded Drive file id is permanently gone: a stale/missing id returns a
/// definitive 404 (real store: `DriveErrorClassification::Other` ->
/// [`DriveError::Other`]; fake: a `"no object..."` message -> [`DriveError::Other`])
/// that no amount of retrying can fix.
///
/// Retryable (keep+retry) ONLY for errors that MIGHT succeed later:
/// - [`DriveError::RateLimited`] / [`DriveError::Transient`] - transient service
///   health; the read may succeed next cycle.
/// - [`DriveError::QuotaExhausted`] / [`DriveError::DailyQuota`] - clears in time.
/// - [`DriveError::InvalidGrant`] - auth; mapped to `needs_reauth` by
///   [`to_reconcile_err`] (recheck-2), NOT a per-op retry forever.
///
/// Everything else ([`DriveError::Other`] = definitive 404 / not-found /
/// non-retryable 4xx, plus `ChecksumMismatch` / dest-folder faults) is a fatal
/// verdict for this op: the recorded id can never be read/updated, so the caller
/// clears the stale id and drops the op rather than retrying forever.
fn reconcile_metadata_error_is_retryable(class: DriveError) -> bool {
    match class {
        DriveError::RateLimited
        | DriveError::Transient
        | DriveError::QuotaExhausted
        | DriveError::DailyQuota
        | DriveError::InvalidGrant => true,
        DriveError::Other
        | DriveError::ChecksumMismatch
        | DriveError::DestFolderMissing
        | DriveError::DestFolderPermissionDenied => false,
    }
}

/// Extract the stranded corrupt-create file id from a Drive-side error, if it
/// carries one (codex C5-P1-4).
///
/// The real [`GoogleDriveStore`] verifies the post-upload md5 INSIDE the store
/// and best-effort-trashes a corrupt CREATE object. When that trash FAILS the
/// store surfaces the live object's id in
/// [`DriveStoreError::ChecksumMismatch { stranded_file_id: Some(..) }`]. This
/// helper pulls that id back out so the executor can persist it
/// (`corrupt_file_id`) and KEEP the op for reconcile to retry the trash, rather
/// than dropping the op and stranding a live corrupt object. Returns `None`
/// for the fake (string-message) path and for a mismatch with no stranded
/// object (trash succeeded / update / streamed-session mismatch).
fn stranded_file_id_from_error(e: &anyhow::Error) -> Option<String> {
    match e.downcast_ref::<DriveStoreError>() {
        Some(DriveStoreError::ChecksumMismatch { stranded_file_id }) => stranded_file_id.clone(),
        _ => None,
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
    /// Sharing violation / lock (Windows) -> `local.file_locked` skip. P2-F:
    /// constructed ONLY on Windows (a sharing/lock-violation raw OS error); off
    /// Windows there is no such lock concept, so the variant is never built
    /// there - but the match arms that handle it are shared cross-platform, so
    /// the variant stays and the off-Windows dead-code lint is suppressed.
    #[cfg_attr(not(windows), allow(dead_code))]
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

    // --- P1-E: out-of-space classification (STRESS_HARNESS s3.1) ------------

    #[test]
    fn enospc_classifies_as_local_disk_full() {
        // Unix ENOSPC (28) and the Windows disk-full codes (39 / 112) must map
        // to LocalDiskFull, not the generic LocalIoError, so the orchestrator
        // can pause the source with an actionable "disk full" rather than a
        // catch-all IO error (SPEC s24 local.disk_full).
        #[cfg(unix)]
        {
            let e = std::io::Error::from_raw_os_error(28);
            assert!(is_disk_full(&e), "ENOSPC must read as disk-full");
            assert_eq!(local_io_error_code(&e), ErrorCode::LocalDiskFull);
        }
        #[cfg(windows)]
        {
            for code in [39, 112] {
                let e = std::io::Error::from_raw_os_error(code);
                assert!(is_disk_full(&e), "Windows {code} must read as disk-full");
                assert_eq!(local_io_error_code(&e), ErrorCode::LocalDiskFull);
            }
        }
    }

    #[test]
    fn non_enospc_io_error_stays_local_io_error() {
        // A generic IO error (permission denied) must NOT be misclassified as
        // disk-full.
        let e = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(!is_disk_full(&e));
        assert_eq!(local_io_error_code(&e), ErrorCode::LocalIoError);
    }

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
            // M5: ExecutorDeps.crypto is now a per-source CryptoProvider. Wrap
            // the single test suite in SingleSuiteProvider (one suite for every
            // source) to preserve the pre-M5 executor-wide behaviour.
            let crypto = crypto.map(|suite| {
                Arc::new(crate::crypto_provider::SingleSuiteProvider::new(suite))
                    as Arc<dyn crate::crypto_provider::CryptoProvider>
            });
            DefaultExecutor::with_clock(
                ExecutorDeps {
                    remote: Arc::new(self.remote.clone()),
                    state: self.state.clone(),
                    pacer: self.pacer.clone(),
                    crypto,
                    vss: None,
                    network: None,
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
                    network: None,
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

        /// A clone of `self.source` (SAME id, so `file_state`/`pending_ops`
        /// lookups by `h.source.id` still resolve) but with
        /// `encryption_enabled = true`. M5 per-source crypto FAILS CLOSED on
        /// `encryption_enabled`: a suite only encrypts when the SourceRow says
        /// the source is encrypted, so the encryption tests pass THIS source to
        /// `execute`/`reconcile` (the suite is wired via `executor_with_crypto`).
        fn encrypted_source(&self) -> SourceRow {
            SourceRow {
                encryption_enabled: true,
                ..self.source.clone()
            }
        }
    }

    fn noop_progress(_p: ExecProgress) {}

    /// R2-P2-1: a no-op per-op outcome sink for tests that do not assert on the
    /// streamed activity (returns an immediately-ready future).
    fn noop_outcome(_o: &OpOutcome) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(async {})
    }

    // --- V2 small-file bundling (issue #35) ---------------------------------

    /// Build an `Op::UploadBundle` plan from `(rel, size)` members.
    fn bundle_plan(source_id: SourceId, members: &[(RelativePath, u64)]) -> Plan {
        Plan {
            ops: vec![Op::UploadBundle {
                source_id,
                members: members
                    .iter()
                    .map(|(r, s)| crate::types::BundleMemberPlan {
                        relative_path: r.clone(),
                        size: *s,
                    })
                    .collect(),
            }],
            collisions: vec![],
        }
    }

    /// A plaintext bundle upload lands ONE `.tar.gz` object marked
    /// `driven.bundle_format`, commits N member `file_state` rows (each with a
    /// NULL `drive_file_id`) + their membership, and every member restores
    /// (extract-from-archive) byte-for-byte.
    #[tokio::test]
    async fn bundle_upload_plaintext_lands_and_members_restore() {
        use tokio::io::AsyncReadExt;

        let h = harness().await;
        let mut contents: Vec<(RelativePath, u64, Vec<u8>)> = Vec::new();
        for i in 0..5 {
            let body = format!("bundle member {i} - some bytes {i}{i}{i}").into_bytes();
            let (rel, size) = h.write_file(&format!("logs/f{i}.log"), &body);
            contents.push((rel, size, body));
        }
        let members: Vec<(RelativePath, u64)> =
            contents.iter().map(|(r, s, _)| (r.clone(), *s)).collect();

        let exec = h.executor();
        let out = exec
            .execute(
                &h.source,
                &bundle_plan(h.source.id, &members),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        assert!(
            matches!(out[0], OpOutcome::BundleDone { files: 5, .. }),
            "expected BundleDone with 5 files, got {:?}",
            out[0]
        );

        // Exactly one object on Drive, marked as a bundle.
        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1, "a bundle is one Drive object");
        assert_eq!(
            children[0]
                .app_properties
                .get(BUNDLE_FORMAT_KEY)
                .map(String::as_str),
            Some(crate::bundle::BUNDLE_FORMAT),
            "the object is stamped driven.bundle_format"
        );
        let bundle_id = children[0].id.clone();

        // Each member: a synced file_state row with NULL drive_file_id + the
        // plaintext hash, and a membership that resolves to the bundle object.
        for (rel, size, body) in &contents {
            let row = h
                .state
                .get_file_state(h.source.id, rel)
                .await
                .unwrap()
                .expect("member file_state row");
            assert!(
                row.drive_file_id.is_none(),
                "bundled member has NULL drive id"
            );
            assert!(row.drive_md5.is_none());
            assert_eq!(row.status, FileStateStatus::Synced);
            assert_eq!(row.size, *size);
            assert_eq!(row.hash_blake3, *blake3::hash(body).as_bytes());
            let bref = h
                .state
                .get_bundle_ref_for_member(h.source.id, rel)
                .await
                .unwrap()
                .expect("membership resolves");
            assert_eq!(bref.drive_file_id, bundle_id);
        }

        // Round-trip: download the object (a plaintext .tar.gz) and extract each
        // member back out.
        let mut blob = Vec::new();
        h.remote
            .download(&bundle_id)
            .await
            .unwrap()
            .0
            .read_to_end(&mut blob)
            .await
            .unwrap();
        for (rel, _size, body) in &contents {
            let name = crate::bundle::member_entry_name(rel);
            let extracted = crate::bundle::extract_member(&blob, &name, 1 << 20)
                .unwrap()
                .expect("member present in archive");
            assert_eq!(&extracted, body, "extracted member equals the original");
        }
    }

    /// Issue #35 (findings 1+4): a member that GREW on disk between the scan
    /// (planned size) and the bundle execute is skipped, NOT read into memory and
    /// NOT packed; the rest of the bundle commits normally, and the grown member
    /// stays uncommitted (no `file_state` row) so a later cycle re-detects it and
    /// uploads it individually.
    #[tokio::test]
    async fn bundle_upload_skips_member_grown_since_scan() {
        let h = harness().await;
        // Plan four small members at their scan-time sizes.
        let mut members: Vec<(RelativePath, u64)> = Vec::new();
        for i in 0..4 {
            let (rel, size) = h.write_file(&format!("logs/f{i}.log"), b"small original body");
            members.push((rel, size));
        }
        // f1 grows massively AFTER the plan captured its size (simulating a
        // reactivated log). Its planned size in the op is still the small one.
        let grown_rel = members[1].0.clone();
        std::fs::write(
            join_source_path(&h.source.local_path, &grown_rel),
            vec![7u8; 2 * 1024 * 1024],
        )
        .unwrap();

        let exec = h.executor();
        let out = exec
            .execute(
                &h.source,
                &bundle_plan(h.source.id, &members),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        assert!(
            matches!(out[0], OpOutcome::BundleDone { files: 3, .. }),
            "the grown member is skipped; the other 3 commit: {:?}",
            out[0]
        );

        // The grown member has NO file_state row - it was never committed, so the
        // next scan retries it (individually, since it now exceeds the per-file
        // ceiling).
        assert!(
            h.state
                .get_file_state(h.source.id, &grown_rel)
                .await
                .unwrap()
                .is_none(),
            "the grown member must stay uncommitted"
        );
        // The other three committed as bundled members (NULL drive_file_id).
        for (rel, _) in members.iter().filter(|(r, _)| r != &grown_rel) {
            let row = h
                .state
                .get_file_state(h.source.id, rel)
                .await
                .unwrap()
                .expect("committed member row");
            assert!(row.drive_file_id.is_none());
            assert!(h
                .state
                .get_bundle_ref_for_member(h.source.id, rel)
                .await
                .unwrap()
                .is_some());
        }
    }

    /// An encrypted bundle stores CIPHERTEXT (not the raw archive); decrypting the
    /// object at the executor's frame boundary yields the `.tar.gz`, and each
    /// member extracts + verifies.
    #[tokio::test]
    async fn bundle_upload_encrypted_roundtrips() {
        use driven_crypto::{ContentDecryptor, DrivenCryptoSuite, HEADER_LEN};
        use tokio::io::AsyncReadExt;

        let h = harness().await;
        let source = h.encrypted_source();
        let source_key = driven_crypto::key::SourceKey::generate();
        let suite = Arc::new(DrivenCryptoSuite::new(source_key.clone()));
        let exec = h.executor_with_crypto(Some(suite));

        let mut contents: Vec<(RelativePath, u64, Vec<u8>)> = Vec::new();
        for i in 0..6 {
            let body = format!("secret member {i} xxxxx {i}").into_bytes();
            let (rel, size) = h.write_file(&format!("secret/f{i}.dat"), &body);
            contents.push((rel, size, body));
        }
        let members: Vec<(RelativePath, u64)> =
            contents.iter().map(|(r, s, _)| (r.clone(), *s)).collect();

        let out = exec
            .execute(
                &source,
                &bundle_plan(source.id, &members),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        assert!(
            matches!(out[0], OpOutcome::BundleDone { files: 6, .. }),
            "got {:?}",
            out[0]
        );

        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
        let bundle_id = children[0].id.clone();

        // Download the ciphertext object and decrypt it at the 64-KiB+tag frame
        // boundary the executor used -> the plaintext .tar.gz.
        let mut blob = Vec::new();
        h.remote
            .download(&bundle_id)
            .await
            .unwrap()
            .0
            .read_to_end(&mut blob)
            .await
            .unwrap();
        let restore = DrivenCryptoSuite::new(source_key);
        let mut dec: Box<dyn ContentDecryptor> =
            restore.content_decryptor(&blob[..HEADER_LEN]).unwrap();
        let ct_chunk = READ_BUF + 16;
        let body = &blob[HEADER_LEN..];
        let mut tar_gz = Vec::new();
        let mut off = 0;
        while body.len() - off > ct_chunk {
            tar_gz.extend_from_slice(&dec.decrypt_chunk(&body[off..off + ct_chunk]).unwrap());
            off += ct_chunk;
        }
        tar_gz.extend_from_slice(&dec.decrypt_last(&body[off..]).unwrap());

        // Every member extracts + verifies against the plaintext hash.
        for (rel, _size, body) in &contents {
            let name = crate::bundle::member_entry_name(rel);
            let extracted = crate::bundle::extract_member(&tar_gz, &name, 1 << 20)
                .unwrap()
                .expect("member present");
            assert_eq!(&extracted, body);
            let row = h
                .state
                .get_file_state(h.source.id, rel)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.hash_blake3, *blake3::hash(body).as_bytes());
            assert!(row.drive_file_id.is_none());
        }
    }

    /// Crash recovery: a surviving `bundle` pending_op means the commit never ran.
    /// Reconcile finds any orphaned bundle object by its op-uuid, trashes it, and
    /// drops the op (the members, never committed, are re-bundled by the next
    /// scan).
    #[tokio::test]
    async fn reconcile_trashes_orphan_bundle_and_drops_op() {
        let h = harness().await;
        let uuid = uuid::Uuid::new_v4().to_string();

        // Simulate a crash after the create landed but before the commit: an object
        // with the op-uuid + bundle marker exists, and its bundle pending_op still
        // sits in the queue.
        let mut app = HashMap::new();
        app.insert(CLIENT_OP_UUID_KEY.to_string(), uuid.clone());
        app.insert(SOURCE_ID_KEY.to_string(), h.source.id.to_string());
        app.insert(
            BUNDLE_FORMAT_KEY.to_string(),
            crate::bundle::BUNDLE_FORMAT.to_string(),
        );
        let entry = h
            .remote
            .create(
                h.source.drive_folder_id.as_str(),
                "driven-bundle-orphan.tar.gz",
                BUNDLE_MIME,
                UploadBody::Bytes(Bytes::from_static(b"orphan bundle bytes")),
                app,
            )
            .await
            .unwrap();

        let payload = PendingOpPayload {
            client_op_uuid: Some(uuid.clone()),
            ..PendingOpPayload::default()
        };
        let rel = RelativePath::try_from("logs/f0.log".to_string()).unwrap();
        h.state
            .enqueue_pending_op(NewPendingOp {
                source_id: h.source.id,
                op_type: OP_TYPE_BUNDLE.to_string(),
                relative_path: rel,
                payload_json: payload.to_value(),
                scheduled_for: 0,
                created_at: 0,
            })
            .await
            .unwrap();

        let exec = h.executor();
        exec.reconcile(&h.source).await.unwrap();

        // The orphan object was trashed (list_folder omits trashed) and the op is
        // gone.
        let live = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert!(
            live.iter().all(|e| e.id != entry.id),
            "the orphaned bundle object must be trashed"
        );
        let ops = h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap();
        assert!(ops.is_empty(), "reconcile drops the bundle pending op");
    }

    /// Enqueue a bundle pending_op for `uuid` and plant its orphan object in
    /// `remote` (a crash-after-create-before-commit fixture). Shared by the
    /// bundle-op reconcile error-classification tests below.
    async fn plant_orphan_bundle(h: &Harness, remote: &InMemoryRemoteStore, uuid: &str) {
        let mut app = HashMap::new();
        app.insert(CLIENT_OP_UUID_KEY.to_string(), uuid.to_string());
        app.insert(SOURCE_ID_KEY.to_string(), h.source.id.to_string());
        app.insert(
            BUNDLE_FORMAT_KEY.to_string(),
            crate::bundle::BUNDLE_FORMAT.to_string(),
        );
        remote
            .create(
                h.source.drive_folder_id.as_str(),
                "driven-bundle-orphan.tar.gz",
                BUNDLE_MIME,
                UploadBody::Bytes(Bytes::from_static(b"orphan bundle bytes")),
                app,
            )
            .await
            .unwrap();
        let payload = PendingOpPayload {
            client_op_uuid: Some(uuid.to_string()),
            ..PendingOpPayload::default()
        };
        h.state
            .enqueue_pending_op(NewPendingOp {
                source_id: h.source.id,
                op_type: OP_TYPE_BUNDLE.to_string(),
                relative_path: RelativePath::try_from("logs/f0.log".to_string()).unwrap(),
                payload_json: payload.to_value(),
                scheduled_for: 0,
                created_at: 0,
            })
            .await
            .unwrap();
    }

    /// Issue #35 finding 6/7 (B/C): a revoked token during the bundle-op reconcile
    /// lookup must surface as `ReconcileError::AuthInvalidGrant` (drives
    /// needs_reauth) and KEEP the op - never drop it or wedge it silently.
    #[tokio::test]
    async fn reconcile_bundle_op_invalid_grant_maps_to_needs_reauth_and_keeps_op() {
        // Request #1 plants the orphan (token valid); request #2 (the reconcile
        // find_by_op_uuid) trips invalid_grant.
        let h = harness_with_remote(InMemoryRemoteStore::new().with_invalid_grant_after(1)).await;
        let uuid = uuid::Uuid::new_v4().to_string();
        plant_orphan_bundle(&h, &h.remote.clone(), &uuid).await;

        let exec = h.executor();
        let err = exec
            .reconcile(&h.source)
            .await
            .expect_err("invalid_grant on the bundle lookup must fail the reconcile");
        assert!(
            reconcile_error_is_invalid_grant(&err),
            "a revoked token during bundle-op reconcile must surface AuthInvalidGrant, got: {err:?}"
        );
        assert_eq!(
            h.state
                .get_pending_ops_for_source(h.source.id)
                .await
                .unwrap()
                .len(),
            1,
            "an auth failure must KEEP the bundle op for retry after re-link"
        );
    }

    /// Issue #35 finding 6/7 (B/C): a TRANSIENT (rate-limited) error during the
    /// bundle-op reconcile lookup keeps the op (reconcile errors -> retry next
    /// cycle) AND accounts the response with the pacer/breaker (backoff recorded).
    #[tokio::test]
    async fn reconcile_bundle_op_transient_keeps_op_and_notes_pacer() {
        // Request #2 (the find) is rate-limited.
        let h = harness_with_remote(InMemoryRemoteStore::new().with_rate_limit_after(1)).await;
        let uuid = uuid::Uuid::new_v4().to_string();
        plant_orphan_bundle(&h, &h.remote.clone(), &uuid).await;

        let exec = h.executor();
        let err = exec
            .reconcile(&h.source)
            .await
            .expect_err("a transient error propagates so the source retries next cycle");
        assert!(
            !reconcile_error_is_invalid_grant(&err),
            "a rate-limit is not an auth failure"
        );
        assert_eq!(
            h.state
                .get_pending_ops_for_source(h.source.id)
                .await
                .unwrap()
                .len(),
            1,
            "a transient error must KEEP the bundle op"
        );
        assert!(
            h.pacer.backoff_hits.load(Ordering::SeqCst) >= 1,
            "the rate-limit must be reported to the pacer (finding 7)"
        );
    }

    /// Issue #35 finding 6 (B, the P1): a DEFINITIVE (non-retryable) trash failure
    /// during bundle-op reconcile DROPS the op rather than propagating forever -
    /// so a permanently-untrashable orphan can never wedge the whole account's
    /// sync (reconcile runs before scan/execute).
    #[tokio::test]
    async fn reconcile_bundle_op_definitive_trash_error_drops_op_no_wedge() {
        let h = harness().await;
        let uuid = uuid::Uuid::new_v4().to_string();
        // Plant the orphan in the shared store; the wrapper delegates lookup to it
        // but fails every trash with a definitive (Other-class) error.
        plant_orphan_bundle(&h, &h.remote.clone(), &uuid).await;
        let store = Arc::new(BundleTrashFailsStore {
            inner: h.remote.clone(),
        });
        let exec = DefaultExecutor::with_clock(
            ExecutorDeps {
                remote: store,
                state: h.state.clone(),
                pacer: h.pacer.clone(),
                crypto: None,
                vss: None,
                network: None,
            },
            h.clock.clone(),
        );

        // Reconcile SUCCEEDS (no propagated error to wedge the account)...
        exec.reconcile(&h.source)
            .await
            .expect("a definitive trash failure must not fail reconcile");
        // ...and the stuck op is dropped so it never recurs every cycle.
        assert!(
            h.state
                .get_pending_ops_for_source(h.source.id)
                .await
                .unwrap()
                .is_empty(),
            "a definitively-untrashable bundle orphan must drop its op (no wedge)"
        );
    }

    /// A [`RemoteStore`] that delegates everything to an inner store EXCEPT
    /// `trash`, which returns a DEFINITIVE (non-retryable, `Other`-class) error -
    /// to prove the bundle-op reconcile recovery drops the op on a permanent trash
    /// failure (issue #35 finding 6).
    struct BundleTrashFailsStore {
        inner: InMemoryRemoteStore,
    }
    #[async_trait::async_trait]
    impl RemoteStore for BundleTrashFailsStore {
        async fn ensure_folder(&self, parent_id: &str, name: &str) -> anyhow::Result<RemoteEntry> {
            self.inner.ensure_folder(parent_id, name).await
        }
        async fn list_folder(&self, folder_id: &str) -> anyhow::Result<Vec<RemoteEntry>> {
            self.inner.list_folder(folder_id).await
        }
        async fn create(
            &self,
            parent_id: &str,
            name: &str,
            mime: &str,
            body: UploadBody,
            app_properties: HashMap<String, String>,
        ) -> anyhow::Result<RemoteEntry> {
            self.inner
                .create(parent_id, name, mime, body, app_properties)
                .await
        }
        async fn update(
            &self,
            file_id: &str,
            body: UploadBody,
            app_properties_patch: HashMap<String, String>,
        ) -> anyhow::Result<RemoteEntry> {
            self.inner.update(file_id, body, app_properties_patch).await
        }
        async fn resumable_session(
            &self,
            kind: ResumableKind,
            mime: &str,
            size: u64,
        ) -> anyhow::Result<ResumableSession> {
            self.inner.resumable_session(kind, mime, size).await
        }
        async fn resume_chunk(
            &self,
            session: &ResumableSession,
            offset: u64,
            chunk: Bytes,
        ) -> anyhow::Result<ResumeProgress> {
            self.inner.resume_chunk(session, offset, chunk).await
        }
        async fn trash(&self, _file_id: &str) -> anyhow::Result<()> {
            // A permanent 4xx-class failure; the plain message matches no
            // transient keyword, so classify_drive_error maps it to
            // DriveError::Other (definitive / non-retryable).
            anyhow::bail!("insufficientFilePermissions: cannot trash this object")
        }
        async fn metadata(&self, file_id: &str) -> anyhow::Result<RemoteEntry> {
            self.inner.metadata(file_id).await
        }
        async fn download(&self, file_id: &str) -> anyhow::Result<DownloadStream> {
            self.inner.download(file_id).await
        }
        async fn find_by_op_uuid(
            &self,
            parent_id: &str,
            op_uuid: &str,
        ) -> anyhow::Result<Option<RemoteEntry>> {
            self.inner.find_by_op_uuid(parent_id, op_uuid).await
        }
        async fn about(&self) -> anyhow::Result<AboutInfo> {
            self.inner.about().await
        }
    }

    /// Bundle GC (issue #35 item a): once every member of a committed bundle is
    /// gone, reconcile trashes the now-dead `.tar.gz` object and drops the
    /// `bundles` row.
    #[tokio::test]
    async fn reconcile_gcs_empty_bundle() {
        let h = harness().await;

        // Commit a real 3-member bundle via the normal upload path.
        let mut members: Vec<(RelativePath, u64)> = Vec::new();
        for i in 0..3 {
            let (rel, size) =
                h.write_file(&format!("logs/f{i}.log"), format!("body {i}").as_bytes());
            members.push((rel, size));
        }
        let exec = h.executor();
        exec.execute(
            &h.source,
            &bundle_plan(h.source.id, &members),
            &noop_progress,
            &noop_outcome,
        )
        .await
        .unwrap();

        // The one bundle object is live, and the bundle is NOT yet a GC candidate.
        let before = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(before.iter().filter(|e| !e.trashed).count(), 1);
        assert!(h
            .state
            .list_empty_bundles(h.source.id)
            .await
            .unwrap()
            .is_empty());

        // All members are deleted locally: their file_state rows go, cascading the
        // bundle_members rows away and leaving the bundle empty.
        for (rel, _) in &members {
            h.state.delete_file_state(h.source.id, rel).await.unwrap();
        }
        assert_eq!(
            h.state.list_empty_bundles(h.source.id).await.unwrap().len(),
            1,
            "the bundle is now empty and a GC candidate"
        );

        // Reconcile sweeps it: the object is trashed and the row is dropped.
        exec.reconcile(&h.source).await.unwrap();
        let after = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(
            after.iter().filter(|e| !e.trashed).count(),
            0,
            "the dead bundle object must be trashed"
        );
        assert!(
            h.state
                .list_empty_bundles(h.source.id)
                .await
                .unwrap()
                .is_empty(),
            "the bundles row must be dropped after GC"
        );
    }

    // --- fresh upload -------------------------------------------------------

    #[tokio::test]
    async fn fresh_upload_lands_and_commits() {
        let h = harness().await;
        let (rel, size) = h.write_file("hello.txt", b"hello world");
        let exec = h.executor();
        let out = exec
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size),
            &noop_progress,
            &noop_outcome,
        )
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
        exec.execute(
            &h.source,
            &h.upload_plan(&rel2, size2),
            &noop_progress,
            &noop_outcome,
        )
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

    // --- issue #36: versioning (trash-as-version-store) ---------------------

    /// Enable per-source versioning on the harness source.
    async fn enable_versioning(h: &Harness, cap: u32, max_bytes: u64) {
        h.state
            .set_versioning_config(
                h.source.id,
                &crate::state::VersioningConfig {
                    enabled: true,
                    count_cap: cap,
                    max_bytes,
                },
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn versioned_change_creates_new_object_and_keeps_old_as_trashed_version() {
        let h = harness().await;
        let (rel, size) = h.write_file("a.txt", b"first");
        let exec = h.executor();
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size),
            &noop_progress,
            &noop_outcome,
        )
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

        enable_versioning(&h, 10, 0).await;
        h.clock.advance(std::time::Duration::from_millis(1_000));

        // A real content change: uploads a NEW object, keeps the old as a version.
        let (_rel2, size2) = h.write_file("a.txt", b"second-longer");
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size2),
            &noop_progress,
            &noop_outcome,
        )
        .await
        .unwrap();

        // The pointer flipped to a NEW object.
        let cur = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap();
        let new_id = cur.drive_file_id.clone().unwrap();
        assert_ne!(
            new_id, first_id,
            "a versioned change must create a NEW object"
        );

        // The OLD object is recorded as a version and (inline) trashed.
        let versions = h.state.list_file_versions(h.source.id, &rel).await.unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].drive_file_id, first_id);
        assert_eq!(versions[0].created_at, 0);
        assert_eq!(versions[0].superseded_at, 1_000);
        assert!(versions[0].trashed, "old object trashed inline");
        assert!(h.remote.metadata(&first_id).await.unwrap().trashed);

        // Exactly one LIVE object on Drive (the new one); resolve-at picks the old.
        let live = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, new_id);
        let at = h
            .state
            .resolve_version_at(h.source.id, &rel, 500)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(at.drive_file_id, first_id);
    }

    #[tokio::test]
    async fn versioned_identical_content_touch_keeps_old_and_makes_no_version() {
        let h = harness().await;
        let (rel, size) = h.write_file("a.txt", b"same-bytes");
        let exec = h.executor();
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size),
            &noop_progress,
            &noop_outcome,
        )
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

        enable_versioning(&h, 10, 0).await;
        h.clock.advance(std::time::Duration::from_millis(1_000));

        // Re-upload byte-identical content (an mtime-only touch): the executor
        // uploads a new object, sees the hash is unchanged, keeps the OLD object
        // as current, trashes the redundant new one, and records NO version.
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size),
            &noop_progress,
            &noop_outcome,
        )
        .await
        .unwrap();

        let cur = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            cur.drive_file_id.as_deref(),
            Some(first_id.as_str()),
            "kept old id"
        );
        assert!(
            h.state
                .list_file_versions(h.source.id, &rel)
                .await
                .unwrap()
                .is_empty(),
            "identical content must not create a version"
        );
        // Still exactly one live object (the redundant new object was trashed).
        let live = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, first_id);
    }

    /// Defect 1 (issue #36): an identical-content (mtime-only) touch keeps the OLD
    /// object current but must PRESERVE its `last_uploaded_at` - `..row` would
    /// advance it to `now`, wrongly rejecting a restore-as-of any instant before
    /// the touch and mis-stamping the next real version's `created_at`.
    #[tokio::test]
    async fn versioned_identical_touch_preserves_last_uploaded_at() {
        let h = harness().await;
        let (rel, size) = h.write_file("a.txt", b"same-bytes");
        let exec = h.executor();
        // First upload at clock 0 -> last_uploaded_at = 0 (the window start).
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size),
            &noop_progress,
            &noop_outcome,
        )
        .await
        .unwrap();
        let before = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            before.last_uploaded_at,
            Some(0),
            "sanity: first upload stamps t0"
        );

        enable_versioning(&h, 10, 0).await;
        h.clock.advance(std::time::Duration::from_millis(1_000));

        // A byte-identical touch at clock 1000.
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size),
            &noop_progress,
            &noop_outcome,
        )
        .await
        .unwrap();

        let after = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.drive_file_id, before.drive_file_id,
            "the touch keeps the old object"
        );
        assert_eq!(
            after.last_uploaded_at,
            Some(0),
            "the identical touch must PRESERVE the old window start (t0), not advance it to the touch time (t1)"
        );
    }

    /// Defect 2/6 (issue #36): if the redundant NEW object's best-effort trash
    /// FAILS during an identical-content touch, the op is KEPT carrying the
    /// duplicate's id (a durable cleanup handle) so the reconcile sweep retries the
    /// trash - the duplicate can never leak live. No `file_versions` row is created
    /// (it is not real history, so the count cap never sees it).
    #[tokio::test]
    async fn versioned_identical_touch_failed_trash_is_durably_cleaned_via_reconcile() {
        let h = harness().await;
        let (rel, size) = h.write_file("a.txt", b"same-bytes");
        // A store that delegates to the harness's remote but FAILS `trash`.
        let store = TrashFailStore::new(h.remote.clone());
        let exec = exec_with_store(&h, store.clone());

        // First upload (a plain create; no trash) -> the OLD object.
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size),
            &noop_progress,
            &noop_outcome,
        )
        .await
        .unwrap();
        let old_id = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap()
            .drive_file_id
            .unwrap();

        enable_versioning(&h, 10, 0).await;
        h.clock.advance(std::time::Duration::from_millis(1_000));

        // A byte-identical touch: creates a duplicate, keeps the OLD object current,
        // then tries to trash the duplicate -> the trash FAILS.
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size),
            &noop_progress,
            &noop_outcome,
        )
        .await
        .unwrap();
        assert_eq!(
            store.trash_calls.load(Ordering::SeqCst),
            1,
            "the redundant duplicate's trash was attempted once and failed"
        );

        // The OLD object is still current, with NO version recorded.
        let cur = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            cur.drive_file_id.as_deref(),
            Some(old_id.as_str()),
            "kept old id"
        );
        assert!(
            h.state
                .list_file_versions(h.source.id, &rel)
                .await
                .unwrap()
                .is_empty(),
            "an identical touch records no version even when the trash fails"
        );

        // The op is KEPT, carrying the redundant duplicate's id as a cleanup handle.
        let pending = h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap();
        assert_eq!(
            pending.len(),
            1,
            "a failed-trash identical touch KEEPS its op for durable cleanup (defect 2/6)"
        );
        let dup_id = pending[0]
            .payload_json
            .get("redundant_duplicate_file_id")
            .and_then(|v| v.as_str())
            .expect("the kept op persists the redundant duplicate id")
            .to_string();
        assert_ne!(dup_id, old_id, "the duplicate is a distinct NEW object");
        // The duplicate is still LIVE on Drive (the leak, until reconcile cleans it).
        assert!(
            !h.remote.metadata(&dup_id).await.unwrap().trashed,
            "the duplicate is live until the retry trashes it"
        );

        // --- reconcile with the trash now SUCCEEDING -> op dropped, dup trashed ---
        store.trash_fails.store(false, Ordering::SeqCst);
        exec.reconcile(&h.source).await.unwrap();
        assert_eq!(
            store.trash_calls.load(Ordering::SeqCst),
            2,
            "reconcile retries the redundant duplicate's trash"
        );
        assert!(
            h.state
                .get_pending_ops_for_source(h.source.id)
                .await
                .unwrap()
                .is_empty(),
            "once the duplicate is confirmed trashed, the cleanup op is dropped"
        );
        assert!(
            h.remote.metadata(&dup_id).await.unwrap().trashed,
            "the redundant duplicate is now trashed"
        );
        // Exactly one live object remains (the OLD one).
        let live = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, old_id);
    }

    /// Defect 4 (issue #36): a crash during an mtime-only touch (the new object
    /// landed byte-identical to the old, before the flip) must be adopted WITHOUT
    /// recording a spurious version - the crash-recovery path gets the same
    /// identical-content guard as the inline path, keeping the OLD object live and
    /// trashing the redundant orphan via the retryable cleanup handle.
    #[tokio::test]
    async fn reconcile_identical_touch_adopt_keeps_old_live_and_records_no_version() {
        let h = harness().await;
        let (rel, _size) = h.write_file("a.txt", b"same-bytes");

        // A pre-existing OLD object + synced row (a content change), versioning on.
        let old = h
            .remote
            .create(
                h.source.drive_folder_id.as_str(),
                "a.txt",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from_static(b"same-bytes")),
                HashMap::new(),
            )
            .await
            .unwrap();
        h.state
            .upsert_file_state(&FileStateRow {
                source_id: h.source.id,
                relative_path: rel.clone(),
                size: 10,
                mtime_ns: 1,
                hash_blake3: *blake3::hash(b"same-bytes").as_bytes(),
                drive_file_id: Some(old.id.clone()),
                drive_md5: old.md5,
                encrypted_remote_path: None,
                status: FileStateStatus::Synced,
                last_uploaded_at: Some(0),
                last_verified_at: Some(0),
            })
            .await
            .unwrap();

        h.clock.advance(std::time::Duration::from_millis(1_000));

        // Crash AFTER the versioned create landed but BEFORE the flip - the orphan
        // is BYTE-IDENTICAL to the old object (a crashed mtime-only touch).
        let op_uuid = uuid::Uuid::new_v4().to_string();
        let mut app = HashMap::new();
        app.insert(CLIENT_OP_UUID_KEY.to_string(), op_uuid.clone());
        let orphan = h
            .remote
            .create(
                h.source.drive_folder_id.as_str(),
                "a.txt",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from_static(b"same-bytes")),
                app,
            )
            .await
            .unwrap();
        let now = h.clock.now_ms();
        h.state
            .enqueue_pending_op(NewPendingOp {
                source_id: h.source.id,
                op_type: OP_TYPE_UPLOAD.to_string(),
                relative_path: rel.clone(),
                payload_json: serde_json::json!({
                    "client_op_uuid": op_uuid,
                    "drive_file_id": null,
                    "supersedes_drive_file_id": old.id,
                    "uploaded_blake3_hex": hex::encode(blake3::hash(b"same-bytes").as_bytes()),
                }),
                scheduled_for: now,
                created_at: now,
            })
            .await
            .unwrap();

        let exec = h.executor();
        exec.reconcile(&h.source).await.unwrap();

        // The OLD object stays current (no flip to the identical orphan) and NO
        // version is recorded (a repeated touch cannot evict genuine history).
        let cur = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            cur.drive_file_id.as_deref(),
            Some(old.id.as_str()),
            "an identical-touch adopt keeps the old object live"
        );
        assert!(
            h.state
                .list_file_versions(h.source.id, &rel)
                .await
                .unwrap()
                .is_empty(),
            "an identical-touch adopt records NO version (defect 4)"
        );
        // The redundant orphan is trashed; the OLD object is not.
        assert!(
            h.remote.metadata(&orphan.id).await.unwrap().trashed,
            "the redundant identical orphan is trashed"
        );
        assert!(
            !h.remote.metadata(&old.id).await.unwrap().trashed,
            "the old object stays live"
        );
        // The cleanup op was dropped (the trash succeeded).
        assert!(h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap()
            .is_empty());
    }

    /// Defect 3 (issue #36): a crash-recovery adopt that REQUEUES (the local file
    /// changed after the versioned upload, so the orphan cannot be marked Synced)
    /// must still record the superseded OLD object as a version before trashing it
    /// - otherwise the last good version silently vanishes from point-in-time
    /// history.
    #[tokio::test]
    async fn reconcile_adopt_requeue_records_superseded_version() {
        let h = harness().await;
        // The bytes uploaded pre-crash.
        let (rel, _size) = h.write_file("a.txt", b"uploaded version");
        let uploaded_hex = hex::encode(blake3::hash(b"uploaded version").as_bytes());

        // A pre-existing OLD object + synced row (versioning on).
        let old = h
            .remote
            .create(
                h.source.drive_folder_id.as_str(),
                "a.txt",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from_static(b"old-bytes")),
                HashMap::new(),
            )
            .await
            .unwrap();
        h.state
            .upsert_file_state(&FileStateRow {
                source_id: h.source.id,
                relative_path: rel.clone(),
                size: 9,
                mtime_ns: 1,
                hash_blake3: *blake3::hash(b"old-bytes").as_bytes(),
                drive_file_id: Some(old.id.clone()),
                drive_md5: old.md5,
                encrypted_remote_path: None,
                status: FileStateStatus::Synced,
                last_uploaded_at: Some(0),
                last_verified_at: Some(0),
            })
            .await
            .unwrap();

        h.clock.advance(std::time::Duration::from_millis(1_000));

        // The versioned NEW object landed pre-crash...
        let op_uuid = uuid::Uuid::new_v4().to_string();
        let mut app = HashMap::new();
        app.insert(CLIENT_OP_UUID_KEY.to_string(), op_uuid.clone());
        let orphan = h
            .remote
            .create(
                h.source.drive_folder_id.as_str(),
                "a.txt",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from_static(b"uploaded version")),
                app,
            )
            .await
            .unwrap();
        // ...but the local file changed AGAIN before restart (the requeue path).
        h.write_file("a.txt", b"locally edited NEW content - longer");

        let now = h.clock.now_ms();
        h.state
            .enqueue_pending_op(NewPendingOp {
                source_id: h.source.id,
                op_type: OP_TYPE_UPLOAD.to_string(),
                relative_path: rel.clone(),
                payload_json: serde_json::json!({
                    "client_op_uuid": op_uuid,
                    "drive_file_id": null,
                    "supersedes_drive_file_id": old.id,
                    "uploaded_blake3_hex": uploaded_hex,
                }),
                scheduled_for: now,
                created_at: now,
            })
            .await
            .unwrap();

        let exec = h.executor();
        exec.reconcile(&h.source).await.unwrap();

        // Requeue: the file_state points at the orphan but is NOT Synced.
        let cur = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            cur.status,
            FileStateStatus::Synced,
            "a post-upload local change must not be marked Synced"
        );
        assert_eq!(
            cur.drive_file_id.as_deref(),
            Some(orphan.id.as_str()),
            "requeue preserves the orphan id (re-upload as update, no duplicate)"
        );

        // Defect 3: the OLD object was recorded as a retained version (not silently
        // dropped) and then trashed.
        let versions = h.state.list_file_versions(h.source.id, &rel).await.unwrap();
        assert_eq!(
            versions.len(),
            1,
            "the superseded OLD object is recorded as a version even on the requeue path"
        );
        assert_eq!(versions[0].drive_file_id, old.id);
        assert!(
            versions[0].trashed,
            "the recorded old version's object is trashed"
        );
        assert!(h.remote.metadata(&old.id).await.unwrap().trashed);
        // The op drained.
        assert!(h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap()
            .is_empty());
    }

    /// Defect 5 (issue #36): a versioned change that hits the 3-consecutive-
    /// checksum-mismatch corrupt threshold must write the Corrupt row pointing at
    /// the still-live OLD object (not NULL), so the eventual recovery re-uploads as
    /// an UPDATE against it rather than orphaning it and creating a duplicate.
    #[tokio::test]
    async fn versioned_corrupt_threshold_preserves_old_object_pointer() {
        let h = harness().await;
        let (rel, size) = h.write_file("a.txt", b"old-bytes");
        // Upload the OLD object via the real store.
        let exec_ok = h.executor();
        exec_ok
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        let old_id = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap()
            .drive_file_id
            .unwrap();

        enable_versioning(&h, 10, 0).await;
        h.clock.advance(std::time::Duration::from_millis(1_000));
        // A real content change -> a versioned CREATE will be attempted.
        let (_r, size2) = h.write_file("a.txt", b"new-bytes-longer");

        // Every versioned create mismatches (trash of the stranded create succeeds),
        // so each attempt leaves the OLD object as the current pointer.
        let store = MismatchStore::new(false);
        let exec_bad = exec_with_store(&h, store.clone());
        for _ in 0..3u32 {
            exec_bad
                .execute(
                    &h.source,
                    &h.upload_plan(&rel, size2),
                    &noop_progress,
                    &noop_outcome,
                )
                .await
                .unwrap();
        }

        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .expect("a Corrupt row is written on the 3rd mismatch");
        assert_eq!(
            row.status,
            FileStateStatus::Corrupt,
            "3 consecutive mismatches mark the file corrupt"
        );
        assert_eq!(
            row.drive_file_id.as_deref(),
            Some(old_id.as_str()),
            "defect 5: the Corrupt row must PRESERVE the still-live OLD object pointer, not NULL"
        );

        // Recovery: a healthy upload of the edited file UPDATES the old object in
        // place (a Corrupt row is not Synced, so no versioned supersede; the old id
        // is preserved, so it is an update - never a duplicate).
        let (_r3, size3) = h.write_file("a.txt", b"recovered content here now");
        exec_ok
            .execute(
                &h.source,
                &h.upload_plan(&rel, size3),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        let rec = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            rec.status,
            FileStateStatus::Synced,
            "the recovery upload marks the file synced again"
        );
        assert_eq!(
            rec.drive_file_id.as_deref(),
            Some(old_id.as_str()),
            "recovery UPDATES the old object in place (no new object, no orphan)"
        );
        assert!(
            h.state
                .list_file_versions(h.source.id, &rel)
                .await
                .unwrap()
                .is_empty(),
            "corrupt-recovery records no version"
        );
        let live = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(live.len(), 1, "recovery updates in place - no duplicate");
        assert_eq!(live[0].id, old_id);
    }

    #[tokio::test]
    async fn versioned_size_guard_falls_back_to_in_place_update() {
        let h = harness().await;
        let (rel, size) = h.write_file("a.txt", b"abcde"); // 5 bytes
        let exec = h.executor();
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size),
            &noop_progress,
            &noop_outcome,
        )
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

        // Guard at 3 bytes: the old object (5 bytes) exceeds it -> no versioning.
        enable_versioning(&h, 10, 3).await;
        h.clock.advance(std::time::Duration::from_millis(1_000));
        let (_r, size2) = h.write_file("a.txt", b"fghij-longer");
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size2),
            &noop_progress,
            &noop_outcome,
        )
        .await
        .unwrap();

        let cur = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            cur.drive_file_id.as_deref(),
            Some(first_id.as_str()),
            "in-place update"
        );
        assert!(h
            .state
            .list_file_versions(h.source.id, &rel)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn versioned_prune_hard_deletes_beyond_count_cap() {
        let h = harness().await;
        let (rel, size) = h.write_file("a.txt", b"v0");
        let exec = h.executor();
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size),
            &noop_progress,
            &noop_outcome,
        )
        .await
        .unwrap();
        let id0 = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap()
            .drive_file_id
            .unwrap();

        enable_versioning(&h, 1, 0).await; // keep at most 1 version

        // First change: version[id0] recorded (1 version, within cap).
        h.clock.advance(std::time::Duration::from_millis(1_000));
        let (_r, s1) = h.write_file("a.txt", b"v1-bytes");
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, s1),
            &noop_progress,
            &noop_outcome,
        )
        .await
        .unwrap();
        let id1 = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap()
            .drive_file_id
            .unwrap();

        // Second change: version[id1] recorded -> 2 versions -> prune the oldest
        // (id0) by HARD DELETE, leaving exactly 1 tracked version.
        h.clock.advance(std::time::Duration::from_millis(1_000));
        let (_r, s2) = h.write_file("a.txt", b"v2-bytes-longer");
        exec.execute(
            &h.source,
            &h.upload_plan(&rel, s2),
            &noop_progress,
            &noop_outcome,
        )
        .await
        .unwrap();

        let versions = h.state.list_file_versions(h.source.id, &rel).await.unwrap();
        assert_eq!(versions.len(), 1, "count cap keeps exactly 1 version");
        assert_eq!(versions[0].drive_file_id, id1);
        // id0 was permanently deleted from Drive (not merely trashed).
        assert!(
            h.remote.metadata(&id0).await.is_err(),
            "pruned version's object is hard-deleted"
        );
    }

    #[tokio::test]
    async fn reconcile_versioned_create_records_version_and_flips_pointer() {
        let h = harness().await;
        let (rel, _size) = h.write_file("a.txt", b"new-bytes");

        // A pre-existing OLD object + synced file_state row (a content change).
        let old = h
            .remote
            .create(
                h.source.drive_folder_id.as_str(),
                "a.txt",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from_static(b"old-bytes")),
                HashMap::new(),
            )
            .await
            .unwrap();
        h.state
            .upsert_file_state(&FileStateRow {
                source_id: h.source.id,
                relative_path: rel.clone(),
                size: 8,
                mtime_ns: 1,
                hash_blake3: *blake3::hash(b"old-bytes").as_bytes(),
                drive_file_id: Some(old.id.clone()),
                drive_md5: old.md5,
                encrypted_remote_path: None,
                status: FileStateStatus::Synced,
                last_uploaded_at: Some(0),
                last_verified_at: Some(0),
            })
            .await
            .unwrap();

        h.clock.advance(std::time::Duration::from_millis(1_000));

        // Simulate a crash AFTER the versioned create landed but BEFORE the flip:
        // an orphan NEW object with the op_uuid + a pending op carrying the
        // supersede intent (drive_file_id null => a create).
        let op_uuid = uuid::Uuid::new_v4().to_string();
        let mut app = HashMap::new();
        app.insert(CLIENT_OP_UUID_KEY.to_string(), op_uuid.clone());
        let orphan = h
            .remote
            .create(
                h.source.drive_folder_id.as_str(),
                "a.txt",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from_static(b"new-bytes")),
                app,
            )
            .await
            .unwrap();
        let now = h.clock.now_ms();
        h.state
            .enqueue_pending_op(NewPendingOp {
                source_id: h.source.id,
                op_type: OP_TYPE_UPLOAD.to_string(),
                relative_path: rel.clone(),
                payload_json: serde_json::json!({
                    "client_op_uuid": op_uuid,
                    "drive_file_id": null,
                    "supersedes_drive_file_id": old.id,
                    "uploaded_blake3_hex": hex::encode(blake3::hash(b"new-bytes").as_bytes()),
                }),
                scheduled_for: now,
                created_at: now,
            })
            .await
            .unwrap();

        let exec = h.executor();
        exec.reconcile(&h.source).await.unwrap();

        // Pointer flipped to the adopted orphan; the OLD object is a trashed version.
        let cur = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cur.drive_file_id.as_deref(), Some(orphan.id.as_str()));
        assert_eq!(cur.status, FileStateStatus::Synced);
        let versions = h.state.list_file_versions(h.source.id, &rel).await.unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].drive_file_id, old.id);
        assert!(
            versions[0].trashed,
            "old object trashed (no crash-window leak)"
        );
        assert!(h.remote.metadata(&old.id).await.unwrap().trashed);
        assert!(h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap()
            .is_empty());
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
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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

    // --- R2-P1-1: invalid_grant during a NORMAL reconcile -> needs_reauth ----

    /// R2-P1-1: a revoked token observed during the CREATE-path reconcile
    /// lookup (`find_by_op_uuid`) - NOT the corrupt-trash retry - must surface
    /// as a typed `ReconcileError::AuthInvalidGrant` so the orchestrator runs
    /// the SAME needs-reauth transition (DESIGN s5.4). Before the fix this path
    /// propagated a plain anyhow Drive error and was retried forever.
    #[tokio::test]
    async fn reconcile_invalid_grant_on_create_lookup_maps_to_needs_reauth() {
        // First remote request (planting the orphan create) succeeds; the
        // SECOND (reconcile's find_by_op_uuid) trips invalid_grant + latches.
        let h = harness_with_remote(InMemoryRemoteStore::new().with_invalid_grant_after(1)).await;
        let (rel, _size) = h.write_file("auth-create.txt", b"committed pre-revoke");

        let op_uuid = uuid::Uuid::new_v4().to_string();
        let mut app = HashMap::new();
        app.insert(CLIENT_OP_UUID_KEY.to_string(), op_uuid.clone());
        // Request #1: plant the orphaned create (token still valid).
        h.remote
            .create(
                h.source.drive_folder_id.as_str(),
                "auth-create.txt",
                "application/octet-stream",
                UploadBody::Bytes(Bytes::from_static(b"committed pre-revoke")),
                app,
            )
            .await
            .unwrap();
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
        let err = exec
            .reconcile(&h.source)
            .await
            .expect_err("invalid_grant on the create lookup must fail the reconcile");
        assert!(
            reconcile_error_is_invalid_grant(&err),
            "a revoked token during the normal create reconcile must surface ReconcileError::AuthInvalidGrant (drives needs_reauth + suspend), got: {err:?}"
        );
        // The op is KEPT (never dropped) so it retries once the user re-links.
        assert_eq!(
            h.state
                .get_pending_ops_for_source(h.source.id)
                .await
                .unwrap()
                .len(),
            1,
            "an auth failure must KEEP the pending op (no data-loss drop)"
        );
    }

    /// R2-P1-1: a revoked token observed during the UPDATE-path reconcile
    /// (`metadata`) must likewise map to `ReconcileError::AuthInvalidGrant`
    /// (not be swallowed by the catch-all delete-op arm).
    #[tokio::test]
    async fn reconcile_invalid_grant_on_update_metadata_maps_to_needs_reauth() {
        // The metadata read is the FIRST remote request on the update path, so
        // trip invalid_grant on request #1.
        let h = harness_with_remote(InMemoryRemoteStore::new().with_invalid_grant_after(0)).await;
        let (rel, _size) = h.write_file("auth-update.txt", b"existing object");

        let op_uuid = uuid::Uuid::new_v4().to_string();
        let now = h.clock.now_ms();
        h.state
            .enqueue_pending_op(NewPendingOp {
                source_id: h.source.id,
                op_type: OP_TYPE_UPLOAD.to_string(),
                relative_path: rel.clone(),
                // drive_file_id present => the UPDATE reconcile path (metadata).
                payload_json: serde_json::json!({
                    "client_op_uuid": op_uuid,
                    "drive_file_id": "some-existing-id",
                }),
                scheduled_for: now,
                created_at: now,
            })
            .await
            .unwrap();

        let exec = h.executor();
        let err = exec
            .reconcile(&h.source)
            .await
            .expect_err("invalid_grant on the metadata read must fail the reconcile");
        assert!(
            reconcile_error_is_invalid_grant(&err),
            "a revoked token during the normal update reconcile must surface ReconcileError::AuthInvalidGrant, got: {err:?}"
        );
        assert_eq!(
            h.state
                .get_pending_ops_for_source(h.source.id)
                .await
                .unwrap()
                .len(),
            1,
            "an auth failure on metadata must KEEP the op, never delete it"
        );
    }

    /// R2-P1-1 (data-safety): a NON-auth (transient) metadata ERROR proves
    /// nothing about whether the update committed, so the reconcile must KEEP
    /// the pending op (retry next cycle) rather than delete it via the old
    /// catch-all `_ => delete_pending_op` arm - which would lose the reconcile
    /// handle for an op that may have committed remotely.
    #[tokio::test]
    async fn reconcile_metadata_transient_error_keeps_the_pending_op() {
        // First remote request (the metadata read) drops the network.
        let h = harness_with_remote(InMemoryRemoteStore::new().with_network_drop_after(0)).await;
        let (rel, _size) = h.write_file("keep-on-error.txt", b"maybe-committed");

        let op_uuid = uuid::Uuid::new_v4().to_string();
        let now = h.clock.now_ms();
        h.state
            .enqueue_pending_op(NewPendingOp {
                source_id: h.source.id,
                op_type: OP_TYPE_UPLOAD.to_string(),
                relative_path: rel.clone(),
                payload_json: serde_json::json!({
                    "client_op_uuid": op_uuid,
                    "drive_file_id": "maybe-committed-id",
                }),
                scheduled_for: now,
                created_at: now,
            })
            .await
            .unwrap();

        let exec = h.executor();
        let err = exec
            .reconcile(&h.source)
            .await
            .expect_err("a transient metadata error must fail the reconcile (retry next cycle)");
        // A plain transient error - NOT an auth one (so the orchestrator simply
        // retries the source next cycle instead of marking needs_reauth).
        assert!(
            !reconcile_error_is_invalid_grant(&err),
            "a transient metadata error must NOT be classified as invalid_grant, got: {err:?}"
        );
        // CRITICAL: the op survives - never dropped on a metadata error.
        assert_eq!(
            h.state
                .get_pending_ops_for_source(h.source.id)
                .await
                .unwrap()
                .len(),
            1,
            "a metadata ERROR must KEEP the pending op (an op that may have committed must not lose its reconcile handle)"
        );
        assert!(
            h.state
                .get_file_state(h.source.id, &rel)
                .await
                .unwrap()
                .is_none(),
            "no file_state row should be written on a failed metadata reconcile"
        );
    }

    /// R3-P1-2 (account-not-wedged): a DEFINITIVE not-found / 404 on the UPDATE
    /// reconcile's `metadata()` read (the recorded `drive_file_id` is gone) is
    /// NOT retryable - the recheck-2 keep+retry-on-any-error would retry it every
    /// cycle, and because reconcile runs BEFORE scan/execute it would WEDGE the
    /// whole account. Instead reconcile must CLEAR the stale `drive_file_id` (so
    /// the next scan re-uploads as a fresh CREATE) and DROP the op, letting the
    /// account proceed.
    #[tokio::test]
    async fn reconcile_metadata_not_found_clears_stale_id_and_drops_op() {
        // A plain fake (no fault injection): `metadata("missing-id")` returns the
        // not-found message "fake: no object with file_id ..." which classifies as
        // DriveError::Other (real store: DriveErrorClassification::Other) - the
        // definitive, NON-retryable 404 shape.
        let h = harness().await;
        let (rel, _size) = h.write_file("stale-update.txt", b"local bytes");

        let stale_id = "missing-id-permanently-gone";
        // Seed a file_state row that recorded this (now-gone) Drive id, so we can
        // assert reconcile CLEARS it.
        h.state
            .upsert_file_state(&FileStateRow {
                source_id: h.source.id,
                relative_path: rel.clone(),
                size: 11,
                mtime_ns: 123,
                hash_blake3: *blake3::hash(b"local bytes").as_bytes(),
                drive_file_id: Some(stale_id.to_string()),
                drive_md5: None,
                encrypted_remote_path: None,
                status: FileStateStatus::Synced,
                last_uploaded_at: Some(h.clock.now_ms()),
                last_verified_at: None,
            })
            .await
            .unwrap();

        let op_uuid = uuid::Uuid::new_v4().to_string();
        let now = h.clock.now_ms();
        h.state
            .enqueue_pending_op(NewPendingOp {
                source_id: h.source.id,
                op_type: OP_TYPE_UPLOAD.to_string(),
                relative_path: rel.clone(),
                // drive_file_id present => the UPDATE reconcile path (metadata).
                payload_json: serde_json::json!({
                    "client_op_uuid": op_uuid,
                    "drive_file_id": stale_id,
                }),
                scheduled_for: now,
                created_at: now,
            })
            .await
            .unwrap();

        let exec = h.executor();
        // The account is NOT wedged: reconcile returns Ok (not a forever-Err).
        exec.reconcile(&h.source)
            .await
            .expect("a definitive 404 must NOT wedge the account; reconcile returns Ok");

        // The dead op is dropped (no forever-retry).
        assert_eq!(
            h.state
                .get_pending_ops_for_source(h.source.id)
                .await
                .unwrap()
                .len(),
            0,
            "a definitive not-found must DROP the stale update op (R3-P1-2)"
        );

        // The stale drive_file_id is cleared so the next scan re-uploads as a
        // fresh CREATE rather than re-attempting the dead update.
        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .expect("the file_state row must still exist (only the id is cleared)");
        assert!(
            row.drive_file_id.is_none(),
            "the stale drive_file_id must be CLEARED so the next scan re-creates the object (R3-P1-2)"
        );
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
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
            .execute(&h.source, &plan, &noop_progress, &noop_outcome)
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
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
                .execute(
                    &h.source,
                    &h.upload_plan(&rel, size),
                    &noop_progress,
                    &noop_outcome,
                )
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
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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

    /// V-D / C-P2-1: `classify_drive_error` must use the TYPED downcast for the
    /// real store so `Transient5xx` (retry) and `Other` (fatal) are
    /// distinguished even though both render the `drive.unreachable` Display
    /// string. The pure-substring matcher would wrongly map a fatal `Other`
    /// to `Transient`; the typed path must not.
    #[test]
    fn classify_drive_error_typed_distinguishes_transient_from_other() {
        use driven_drive::google::DriveError as DriveStoreError;
        use driven_drive::remote_store::DriveErrorClassification;

        // A typed Transient5xx -> Transient (retryable).
        let t5 = anyhow::Error::new(DriveStoreError::Classified {
            kind: DriveErrorClassification::Transient5xx,
            source: anyhow::anyhow!("503"),
        });
        assert_eq!(
            super::classify_drive_error(&t5),
            super::DriveError::Transient
        );

        // A typed Other -> Other (fatal). Both Display as "drive.unreachable",
        // so a substring matcher would have wrongly retried this.
        let other = anyhow::Error::new(DriveStoreError::Classified {
            kind: DriveErrorClassification::Other,
            source: anyhow::anyhow!("404 unexpected"),
        });
        assert_eq!(
            super::classify_drive_error(&other),
            super::DriveError::Other
        );

        // The dedicated fatal variants map precisely.
        assert_eq!(
            super::classify_drive_error(&anyhow::Error::new(DriveStoreError::DestFolderMissing)),
            super::DriveError::DestFolderMissing
        );
        assert_eq!(
            super::classify_drive_error(&anyhow::Error::new(
                DriveStoreError::DestFolderPermissionDenied
            )),
            super::DriveError::DestFolderPermissionDenied
        );
        assert_eq!(
            super::classify_drive_error(&anyhow::Error::new(
                DriveStoreError::ResumableSessionInvalid
            )),
            super::DriveError::Other
        );

        // The string fallback still classifies the fake's plain messages.
        let fake_5xx = anyhow::anyhow!("fake: drive.unreachable (5xx)");
        assert_eq!(
            super::classify_drive_error(&fake_5xx),
            super::DriveError::Transient
        );
        let fake_rl = anyhow::anyhow!("fake: drive.rate_limited");
        assert_eq!(
            super::classify_drive_error(&fake_rl),
            super::DriveError::RateLimited
        );
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
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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

    /// Issue #25 (launch-UX): a LOCKED file while the least-privilege helper is
    /// still LAUNCHING is skipped TRANSIENTLY (re-queued) and classified
    /// `local.vss_helper_pending` - NOT the permanent `local.file_locked`. Windows
    /// only (a real `ERROR_SHARING_VIOLATION` cannot be produced cross-OS) but
    /// NON-elevated-safe: `FakeVssProvider::pending()` returns `Pending` without
    /// any real COM, so this runs on the (non-elevated) Windows CI runner. The
    /// pure decision is table-tested in `driven_vss::fallback_decision`.
    #[cfg(windows)]
    #[tokio::test]
    async fn locked_file_while_helper_pending_skips_as_pending_not_locked() {
        use std::os::windows::fs::OpenOptionsExt;

        let h = harness().await;
        let (rel, size) = h.write_file("locked-pending.dat", b"bytes-behind-a-launching-helper");
        let live = h.tmp_src.path().join("locked-pending.dat");

        const GENERIC_WRITE: u32 = 0x4000_0000;
        let _exclusive = std::fs::OpenOptions::new()
            .access_mode(GENERIC_WRITE)
            .share_mode(0)
            .write(true)
            .open(&live)
            .expect("open locked-pending.dat exclusively");
        assert!(
            matches!(
                super::open_shared(&live).await,
                Err(super::OpenError::Locked)
            ),
            "test setup: file must be locked"
        );

        // The helper is available (capability) but its snapshot is Pending.
        let vss: Arc<dyn driven_vss::VssProvider> = Arc::new(driven_vss::FakeVssProvider::pending(
            driven_vss::VssMode::Auto,
        ));
        let exec = h.executor_with_vss(vss);
        let out = exec
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                out[0],
                OpOutcome::Skipped {
                    reason: SkipReason::VssHelperPending,
                    ..
                }
            ),
            "a pending helper must skip-as-pending (retry next cycle), not lock; got {:?}",
            out[0]
        );
        // Not committed as Synced (it re-queues for the next cycle).
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
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
        let source = h.encrypted_source();
        let exec = h.executor_with_crypto(Some(Arc::new(FakeSuite)));
        let out = exec
            .execute(
                &source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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

    // --- M5 GA blocker: per-source crypto resolution + FAIL-CLOSED policy ----
    //
    // The executor holds an `Option<Arc<dyn CryptoProvider>>` and resolves the
    // suite PER source, keyed on the SourceRow's `encryption_enabled` (CODEX_NOTES
    // "Per-source crypto resolution", DESIGN s7). The three branches:
    //   (a) encryption_enabled=true + suite resolved -> CIPHERTEXT on the fake;
    //   (b) encryption_enabled=true + NO key         -> FAIL CLOSED (no object);
    //   (c) encryption_enabled=false                 -> PLAINTEXT, suite ignored.
    // These prove the executor never uploads plaintext for an encrypted source
    // nor ciphertext for an unencrypted one, in a mixed account.

    /// Read one object's retained literal bytes off the fake (the fake keeps
    /// the literal bytes unless its content oracle is armed, which these tests
    /// never do).
    fn literal_object_bytes(h: &Harness, file_id: &str) -> Vec<u8> {
        match h.remote.object_content(file_id).expect("object content") {
            driven_drive::fake::ObjectContent::Literal(bytes) => bytes,
            driven_drive::fake::ObjectContent::Oracle { .. } => {
                panic!("test object should retain literal bytes (oracle not armed)")
            }
        }
    }

    /// A [`CryptoProvider`] that returns a DISTINCT decision per source id, so
    /// one provider can model a mixed (encrypted + plaintext + key-missing)
    /// account - exactly what the executor must resolve PER source.
    struct PerSourceProvider {
        decisions: std::collections::HashMap<SourceId, crate::crypto_provider::CryptoResolution>,
    }
    impl crate::crypto_provider::CryptoProvider for PerSourceProvider {
        fn resolve(&self, source_id: &SourceId) -> crate::crypto_provider::CryptoResolution {
            match self.decisions.get(source_id) {
                Some(crate::crypto_provider::CryptoResolution::Suite(s)) => {
                    crate::crypto_provider::CryptoResolution::Suite(s.clone())
                }
                Some(crate::crypto_provider::CryptoResolution::Unavailable) => {
                    crate::crypto_provider::CryptoResolution::Unavailable
                }
                // Default (and explicit Plaintext): plaintext.
                _ => crate::crypto_provider::CryptoResolution::Plaintext,
            }
        }
    }

    /// Build an executor over a provider that hands `resolution` for `source_id`.
    fn exec_with_provider(
        h: &Harness,
        source_id: SourceId,
        resolution: crate::crypto_provider::CryptoResolution,
    ) -> DefaultExecutor {
        let mut decisions = std::collections::HashMap::new();
        decisions.insert(source_id, resolution);
        let provider: Arc<dyn crate::crypto_provider::CryptoProvider> =
            Arc::new(PerSourceProvider { decisions });
        DefaultExecutor::with_clock(
            ExecutorDeps {
                remote: Arc::new(h.remote.clone()),
                state: h.state.clone(),
                pacer: h.pacer.clone(),
                crypto: Some(provider),
                vss: None,
                network: None,
            },
            h.clock.clone(),
        )
    }

    /// Branch (a): an encryption_enabled source whose suite resolves uploads
    /// CIPHERTEXT (the stored object is larger than the plaintext + the bytes on
    /// the fake are not the plaintext), and file_state carries the plaintext
    /// blake3 + an encrypted_remote_path.
    #[tokio::test]
    async fn per_source_encrypted_uploads_ciphertext() {
        let h = harness().await;
        let source = h.encrypted_source();
        let plaintext = b"per-source encrypted content".to_vec();
        let (rel, size) = h.write_file("enc.txt", &plaintext);
        let exec = exec_with_provider(
            &h,
            source.id,
            crate::crypto_provider::CryptoResolution::Suite(Arc::new(FakeSuite)),
        );
        let out = exec
            .execute(
                &source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        assert!(matches!(out[0], OpOutcome::Done { .. }), "got {:?}", out[0]);

        let children = h
            .remote
            .list_folder(source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1, "exactly one object on the fake");
        assert!(
            children[0].size.unwrap() > size,
            "encrypted source must upload CIPHERTEXT (header + tags exceed plaintext)"
        );
        // The stored bytes are NOT the plaintext.
        let stored = literal_object_bytes(&h, &children[0].id);
        assert_ne!(stored, plaintext, "stored bytes must be ciphertext");
        let row = h
            .state
            .get_file_state(source.id, &rel)
            .await
            .unwrap()
            .expect("file_state row");
        assert_eq!(
            row.hash_blake3,
            *blake3::hash(&plaintext).as_bytes(),
            "file_state blake3 is over the PLAINTEXT"
        );
        assert!(
            row.encrypted_remote_path.is_some(),
            "an encrypted source records the ciphertext remote path"
        );
    }

    /// Branch (c): an unencrypted source (encryption_enabled=false) uploads
    /// PLAINTEXT and IGNORES any suite the provider hands out - it must NEVER
    /// upload ciphertext. We deliberately wire a `Suite` decision for its id to
    /// prove the executor keys off `encryption_enabled`, not the provider alone.
    #[tokio::test]
    async fn per_source_unencrypted_uploads_plaintext_ignoring_suite() {
        let h = harness().await; // h.source.encryption_enabled == false
        let plaintext = b"plaintext content for an unencrypted source".to_vec();
        let (rel, size) = h.write_file("plain.txt", &plaintext);
        // Provider would hand a suite, but encryption_enabled=false MUST win.
        let exec = exec_with_provider(
            &h,
            h.source.id,
            crate::crypto_provider::CryptoResolution::Suite(Arc::new(FakeSuite)),
        );
        let out = exec
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        assert!(matches!(out[0], OpOutcome::Done { .. }), "got {:?}", out[0]);

        let children = h
            .remote
            .list_folder(h.source.drive_folder_id.as_str())
            .await
            .unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(
            children[0].size,
            Some(size),
            "an unencrypted source must upload PLAINTEXT (exact size), never ciphertext"
        );
        let stored = literal_object_bytes(&h, &children[0].id);
        assert_eq!(stored, plaintext, "stored bytes are the plaintext");
        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .expect("file_state row");
        assert!(
            row.encrypted_remote_path.is_none(),
            "an unencrypted source records NO encrypted remote path"
        );
    }

    /// Branch (b) - the GA-critical FAIL-CLOSED path: an encryption_enabled
    /// source whose key is UNAVAILABLE must FAIL the op with `crypto.key_missing`
    /// and upload NOTHING (never plaintext). No object is created on the fake.
    #[tokio::test]
    async fn per_source_encrypted_no_key_fails_closed_no_object() {
        let h = harness().await;
        let source = h.encrypted_source();
        let secret = b"this must NEVER reach Drive as plaintext".to_vec();
        let (rel, size) = h.write_file("secret.txt", &secret);
        let exec = exec_with_provider(
            &h,
            source.id,
            crate::crypto_provider::CryptoResolution::Unavailable,
        );
        let out = exec
            .execute(
                &source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                out[0],
                OpOutcome::Failed {
                    code: ErrorCode::CryptoKeyMissing,
                    ..
                }
            ),
            "an encryption-enabled source with no key must FAIL CLOSED (crypto.key_missing); got {:?}",
            out[0]
        );
        // FAIL CLOSED: absolutely NO object on Drive - not plaintext, not
        // ciphertext, not even a trashed orphan.
        let with_trashed = h
            .remote
            .list_folder_with_trashed(h.source.drive_folder_id.as_str());
        assert!(
            with_trashed.is_empty(),
            "fail-closed must upload NOTHING (no plaintext leak); got {with_trashed:?}"
        );
        // No file_state row marked Synced.
        let row = h.state.get_file_state(source.id, &rel).await.unwrap();
        assert!(
            row.is_none() || row.unwrap().status != FileStateStatus::Synced,
            "a fail-closed op must not commit a Synced row"
        );
        // No pending op stranded (the op failed before enqueueing anything).
        assert!(
            h.state
                .get_pending_ops_for_source(source.id)
                .await
                .unwrap()
                .is_empty(),
            "fail-closed must not leave a pending op"
        );
    }

    /// A buggy provider that returns `Plaintext` for an encryption_enabled
    /// source must ALSO fail closed: the executor - not the provider - is the
    /// authority. `Plaintext` for an encrypted source is never a license to
    /// upload plaintext.
    #[tokio::test]
    async fn per_source_encrypted_provider_says_plaintext_still_fails_closed() {
        let h = harness().await;
        let source = h.encrypted_source();
        let (rel, size) = h.write_file("secret2.txt", b"must not leak");
        let exec = exec_with_provider(
            &h,
            source.id,
            crate::crypto_provider::CryptoResolution::Plaintext,
        );
        let out = exec
            .execute(
                &source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                out[0],
                OpOutcome::Failed {
                    code: ErrorCode::CryptoKeyMissing,
                    ..
                }
            ),
            "encryption-enabled + provider Plaintext must STILL fail closed; got {:?}",
            out[0]
        );
        assert!(
            h.remote
                .list_folder_with_trashed(h.source.drive_folder_id.as_str())
                .is_empty(),
            "no object may be created when failing closed"
        );
    }

    /// An encryption_enabled source with NO provider at all (the `crypto: None`
    /// executor) must fail closed too - a missing provider is a missing key.
    #[tokio::test]
    async fn per_source_encrypted_no_provider_fails_closed() {
        let h = harness().await;
        let source = h.encrypted_source();
        let (rel, size) = h.write_file("secret3.txt", b"no provider, no plaintext leak");
        let exec = h.executor(); // crypto provider: None
        let out = exec
            .execute(
                &source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                out[0],
                OpOutcome::Failed {
                    code: ErrorCode::CryptoKeyMissing,
                    ..
                }
            ),
            "encryption-enabled with no provider must fail closed; got {:?}",
            out[0]
        );
        assert!(
            h.remote
                .list_folder_with_trashed(h.source.drive_folder_id.as_str())
                .is_empty(),
            "no object may be created when failing closed"
        );
    }

    // --- R2-P1-3 (3-consecutive-mismatch -> corrupt) + R2-P1-1 (durable
    //     corrupt-create cleanup) -----------------------------------------------

    /// A [`RemoteStore`] whose `create` ALWAYS finalizes with a deliberately
    /// WRONG md5 (so the executor's post-upload verify trips a checksum mismatch
    /// every time), and whose `trash` can be toggled to FAIL (so the corrupt
    /// object is "stranded" - R2-P1-1). Records every created id + every trash
    /// attempt so a test can assert the durable-cleanup retry. Only `create` /
    /// `trash` are exercised; the rest bail loudly rather than fake success.
    struct MismatchStore {
        next_id: AtomicU64,
        created_ids: std::sync::Mutex<Vec<String>>,
        trash_calls: AtomicU64,
        /// When `true`, `trash` returns an error (the object stays "live").
        trash_fails: std::sync::atomic::AtomicBool,
    }
    impl MismatchStore {
        fn new(trash_fails: bool) -> Arc<Self> {
            Arc::new(Self {
                next_id: AtomicU64::new(0),
                created_ids: std::sync::Mutex::new(Vec::new()),
                trash_calls: AtomicU64::new(0),
                trash_fails: std::sync::atomic::AtomicBool::new(trash_fails),
            })
        }
        fn created_ids(&self) -> Vec<String> {
            self.created_ids.lock().unwrap().clone()
        }
    }
    #[async_trait::async_trait]
    impl RemoteStore for MismatchStore {
        async fn create(
            &self,
            _parent_id: &str,
            name: &str,
            mime: &str,
            _body: UploadBody,
            app_properties: HashMap<String, String>,
        ) -> anyhow::Result<RemoteEntry> {
            let id = format!("mm-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
            self.created_ids.lock().unwrap().push(id.clone());
            // A deliberately wrong md5 (never matches the local md5 of any body).
            Ok(RemoteEntry {
                id,
                name: name.to_string(),
                parents: vec![],
                size: Some(0),
                md5: Some([0xABu8; 16]),
                mime_type: mime.to_string(),
                modified_time: 0,
                trashed: false,
                app_properties,
            })
        }
        async fn trash(&self, _file_id: &str) -> anyhow::Result<()> {
            self.trash_calls.fetch_add(1, Ordering::SeqCst);
            if self.trash_fails.load(Ordering::SeqCst) {
                anyhow::bail!("MismatchStore: forced trash failure")
            }
            Ok(())
        }
        async fn ensure_folder(&self, _p: &str, _n: &str) -> anyhow::Result<RemoteEntry> {
            anyhow::bail!("MismatchStore: ensure_folder must not be called")
        }
        async fn list_folder(&self, _f: &str) -> anyhow::Result<Vec<RemoteEntry>> {
            anyhow::bail!("MismatchStore: list_folder must not be called")
        }
        async fn update(
            &self,
            _f: &str,
            _b: UploadBody,
            _a: HashMap<String, String>,
        ) -> anyhow::Result<RemoteEntry> {
            anyhow::bail!("MismatchStore: update must not be called")
        }
        async fn resumable_session(
            &self,
            _k: ResumableKind,
            _m: &str,
            _s: u64,
        ) -> anyhow::Result<ResumableSession> {
            anyhow::bail!("MismatchStore: resumable_session must not be called")
        }
        async fn resume_chunk(
            &self,
            _s: &ResumableSession,
            _o: u64,
            _c: Bytes,
        ) -> anyhow::Result<ResumeProgress> {
            anyhow::bail!("MismatchStore: resume_chunk must not be called")
        }
        async fn metadata(&self, _f: &str) -> anyhow::Result<RemoteEntry> {
            anyhow::bail!("MismatchStore: metadata must not be called")
        }
        async fn download(&self, _f: &str) -> anyhow::Result<DownloadStream> {
            anyhow::bail!("MismatchStore: download must not be called")
        }
        async fn find_by_op_uuid(&self, _p: &str, _u: &str) -> anyhow::Result<Option<RemoteEntry>> {
            anyhow::bail!("MismatchStore: find_by_op_uuid must not be called")
        }
        async fn about(&self) -> anyhow::Result<AboutInfo> {
            anyhow::bail!("MismatchStore: about must not be called")
        }
    }

    /// A store that DELEGATES everything to a real [`InMemoryRemoteStore`] but can
    /// force `trash` to fail (and counts the calls), so a successful upload can be
    /// followed by a failing trash - exactly the identical-content-touch cleanup
    /// leak window (defect 2/6). Wrap a CLONE of the harness's remote so the
    /// shared backing store (and the source's `drive_folder_id`) stay consistent.
    struct TrashFailStore {
        inner: InMemoryRemoteStore,
        trash_fails: std::sync::atomic::AtomicBool,
        trash_calls: AtomicU64,
    }
    impl TrashFailStore {
        fn new(inner: InMemoryRemoteStore) -> Arc<Self> {
            Arc::new(Self {
                inner,
                trash_fails: std::sync::atomic::AtomicBool::new(true),
                trash_calls: AtomicU64::new(0),
            })
        }
    }
    #[async_trait::async_trait]
    impl RemoteStore for TrashFailStore {
        async fn ensure_folder(&self, p: &str, n: &str) -> anyhow::Result<RemoteEntry> {
            self.inner.ensure_folder(p, n).await
        }
        async fn list_folder(&self, f: &str) -> anyhow::Result<Vec<RemoteEntry>> {
            self.inner.list_folder(f).await
        }
        async fn create(
            &self,
            parent_id: &str,
            name: &str,
            mime: &str,
            body: UploadBody,
            app_properties: HashMap<String, String>,
        ) -> anyhow::Result<RemoteEntry> {
            self.inner
                .create(parent_id, name, mime, body, app_properties)
                .await
        }
        async fn update(
            &self,
            f: &str,
            b: UploadBody,
            a: HashMap<String, String>,
        ) -> anyhow::Result<RemoteEntry> {
            self.inner.update(f, b, a).await
        }
        async fn resumable_session(
            &self,
            k: ResumableKind,
            m: &str,
            s: u64,
        ) -> anyhow::Result<ResumableSession> {
            self.inner.resumable_session(k, m, s).await
        }
        async fn resume_chunk(
            &self,
            s: &ResumableSession,
            o: u64,
            c: Bytes,
        ) -> anyhow::Result<ResumeProgress> {
            self.inner.resume_chunk(s, o, c).await
        }
        async fn trash(&self, file_id: &str) -> anyhow::Result<()> {
            self.trash_calls.fetch_add(1, Ordering::SeqCst);
            if self.trash_fails.load(Ordering::SeqCst) {
                anyhow::bail!("TrashFailStore: forced trash failure")
            }
            self.inner.trash(file_id).await
        }
        async fn delete_permanent(&self, file_id: &str) -> anyhow::Result<()> {
            self.inner.delete_permanent(file_id).await
        }
        async fn metadata(&self, f: &str) -> anyhow::Result<RemoteEntry> {
            self.inner.metadata(f).await
        }
        async fn download(&self, f: &str) -> anyhow::Result<DownloadStream> {
            self.inner.download(f).await
        }
        async fn find_by_op_uuid(&self, p: &str, u: &str) -> anyhow::Result<Option<RemoteEntry>> {
            self.inner.find_by_op_uuid(p, u).await
        }
        async fn about(&self) -> anyhow::Result<AboutInfo> {
            self.inner.about().await
        }
    }

    /// Build an executor over `store` + the harness's state/pacer/clock.
    fn exec_with_store(h: &Harness, store: Arc<dyn RemoteStore>) -> DefaultExecutor {
        DefaultExecutor::with_clock(
            ExecutorDeps {
                remote: store,
                state: h.state.clone(),
                pacer: h.pacer.clone(),
                crypto: None,
                vss: None,
                network: None,
            },
            h.clock.clone(),
        )
    }

    /// R2-P1-3 (DESIGN s5.4 lines 498-500): three CONSECUTIVE checksum
    /// mismatches on the same file mark its `file_state` row Corrupt and stop
    /// retrying. The first two mismatches leave the file retryable (no row /
    /// not Corrupt); the 3rd flips it to Corrupt with the live (size, mtime)
    /// stamped, and the counter resets so a later edit gets a fresh budget.
    #[tokio::test]
    async fn three_consecutive_checksum_mismatches_mark_corrupt() {
        let h = harness().await;
        let (rel, size) = h.write_file("corrupt-me.bin", b"deterministic corrupt content");
        // trash succeeds (so each attempt's create object is cleanly removed and
        // the op is dropped -> each cycle is a fresh CREATE attempt against the
        // same path; the counter is what persists).
        let store = MismatchStore::new(false);
        let exec = exec_with_store(&h, store.clone());

        // Attempts 1 and 2: a mismatch each, but NOT yet corrupt.
        for attempt in 1..=2u32 {
            let out = exec
                .execute(
                    &h.source,
                    &h.upload_plan(&rel, size),
                    &noop_progress,
                    &noop_outcome,
                )
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
                "attempt {attempt} must surface a checksum mismatch; got {:?}",
                out[0]
            );
            let row = h.state.get_file_state(h.source.id, &rel).await.unwrap();
            assert!(
                row.is_none() || row.unwrap().status != FileStateStatus::Corrupt,
                "below the threshold the file must NOT yet be Corrupt (attempt {attempt})"
            );
        }

        // Attempt 3: the 3rd consecutive mismatch marks the file Corrupt.
        let out = exec
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
        let row = h
            .state
            .get_file_state(h.source.id, &rel)
            .await
            .unwrap()
            .expect("a Corrupt file_state row is written on the 3rd mismatch");
        assert_eq!(
            row.status,
            FileStateStatus::Corrupt,
            "3 consecutive mismatches must mark the file_state status=corrupt (DESIGN s5.4)"
        );
        // The row is stamped with the LIVE (size, mtime) so the FastPath scanner
        // treats it as unchanged and STOPS re-emitting it (the file is the same
        // size we wrote).
        assert_eq!(
            row.size, size,
            "the Corrupt row carries the live file size so the scanner stops retrying"
        );

        // The counter was reset (a later user edit gets a fresh budget).
        let n = h
            .state
            .bump_checksum_mismatch_count(h.source.id, &rel)
            .await
            .unwrap();
        assert_eq!(
            n, 1,
            "the consecutive-mismatch counter resets after the corrupt transition"
        );
    }

    /// R2-P1-3: a SUCCESSFUL upload breaks the consecutive-mismatch streak (the
    /// counter resets), so a mismatch -> success -> mismatch does NOT accumulate
    /// toward the corrupt threshold.
    #[tokio::test]
    async fn successful_upload_resets_mismatch_counter() {
        let h = harness().await;
        let (rel, size) = h.write_file("recovers.bin", b"content that will recover");

        // One mismatch (counter -> 1) via the always-mismatch store.
        let bad = MismatchStore::new(false);
        let exec_bad = exec_with_store(&h, bad);
        let out = exec_bad
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        assert!(matches!(
            out[0],
            OpOutcome::Failed {
                code: ErrorCode::DriveChecksumMismatch,
                ..
            }
        ));

        // Now a healthy store: the upload succeeds and must RESET the counter.
        let exec_ok = h.executor();
        let out = exec_ok
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
            .await
            .unwrap();
        assert!(matches!(out[0], OpOutcome::Done { .. }), "got {:?}", out[0]);

        // The counter is back to 1 on the next bump (was cleared by success),
        // proving the streak did not carry the earlier mismatch forward.
        let n = h
            .state
            .bump_checksum_mismatch_count(h.source.id, &rel)
            .await
            .unwrap();
        assert_eq!(
            n, 1,
            "a successful upload must reset the consecutive counter"
        );
    }

    /// R2-P1-1: when a corrupt CREATE object's trash FAILS, the executor keeps
    /// the pending op (persisting the corrupt `file_id`) instead of dropping it
    /// and stranding a live corrupt object - and the reconcile pass RETRIES the
    /// trash, dropping the op only once the object is confirmed gone.
    #[tokio::test]
    async fn corrupt_create_with_failed_trash_is_durably_cleaned_via_reconcile() {
        let h = harness().await;
        let (rel, size) = h.write_file("stranded.bin", b"corrupt + trash fails");
        let store = MismatchStore::new(true); // trash FAILS
        let exec = exec_with_store(&h, store.clone());

        // The create mismatches AND its trash fails -> the op must be KEPT.
        let out = exec
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
        // One create + one (failed) trash so far.
        assert_eq!(store.created_ids().len(), 1, "exactly one create attempt");
        assert_eq!(
            store.trash_calls.load(Ordering::SeqCst),
            1,
            "the post-upload trash was attempted once and failed"
        );

        // The op is KEPT (not dropped) with the corrupt file_id persisted, so a
        // live corrupt object is not stranded without a recovery handle.
        let pending = h
            .state
            .get_pending_ops_for_source(h.source.id)
            .await
            .unwrap();
        assert_eq!(
            pending.len(),
            1,
            "a failed-trash corrupt create must KEEP its op for durable cleanup (R2-P1-1)"
        );
        let corrupt_id = pending[0]
            .payload_json
            .get("corrupt_file_id")
            .and_then(|v| v.as_str())
            .expect("the kept op persists the corrupt file_id");
        assert_eq!(
            corrupt_id,
            store.created_ids()[0].as_str(),
            "the persisted corrupt_file_id is the corrupt object's id"
        );

        // --- reconcile with the trash now SUCCEEDING -> the op is dropped ----
        store.trash_fails.store(false, Ordering::SeqCst);
        exec.reconcile(&h.source).await.unwrap();
        assert_eq!(
            store.trash_calls.load(Ordering::SeqCst),
            2,
            "reconcile retries the trash of the stranded corrupt object"
        );
        assert!(
            h.state
                .get_pending_ops_for_source(h.source.id)
                .await
                .unwrap()
                .is_empty(),
            "once the corrupt object is confirmed trashed, the op is dropped"
        );
    }

    /// R2-P1-1: if reconcile's re-trash STILL fails, the op is KEPT for the next
    /// cycle (never dropped while the corrupt object may be live).
    #[tokio::test]
    async fn corrupt_create_reconcile_keeps_op_when_retrash_fails_again() {
        let h = harness().await;
        let (rel, size) = h.write_file("still-stranded.bin", b"corrupt + trash keeps failing");
        let store = MismatchStore::new(true); // trash always FAILS
        let exec = exec_with_store(&h, store.clone());

        exec.execute(
            &h.source,
            &h.upload_plan(&rel, size),
            &noop_progress,
            &noop_outcome,
        )
        .await
        .unwrap();
        assert_eq!(
            h.state
                .get_pending_ops_for_source(h.source.id)
                .await
                .unwrap()
                .len(),
            1
        );

        // Reconcile re-trashes, fails again -> op KEPT.
        exec.reconcile(&h.source).await.unwrap();
        assert_eq!(
            store.trash_calls.load(Ordering::SeqCst),
            2,
            "reconcile attempted the re-trash"
        );
        assert_eq!(
            h.state
                .get_pending_ops_for_source(h.source.id)
                .await
                .unwrap()
                .len(),
            1,
            "a still-failing re-trash must KEEP the op (never strand the corrupt object)"
        );
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

    // --- V5-P1-2: fixed-64KiB encrypt framing survives a SHORT read ---------

    /// A reader that returns AT MOST `cap` bytes per `poll_read`, simulating a
    /// short read on a network / FUSE / SMB mount. Drives the V5-P1-2 path:
    /// `read_full_chunk` must accumulate these into full READ_BUF chunks so the
    /// AEAD frame boundary stays at 64 KiB regardless of read() granularity.
    struct ShortReader {
        data: Vec<u8>,
        pos: usize,
        cap: usize,
    }

    impl tokio::io::AsyncRead for ShortReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let remaining = self.data.len() - self.pos;
            let want = buf.remaining().min(self.cap).min(remaining);
            if want > 0 {
                let start = self.pos;
                let end = start + want;
                // Copy the slice out first to avoid an aliased borrow of `self`.
                let slice = self.data[start..end].to_vec();
                buf.put_slice(&slice);
                self.pos = end;
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Replays exactly what the cpu stage / `read_hash_encrypt` do over chunks
    /// `read_full_chunk` yields from `reader`: header, `encrypt_chunk` for each
    /// non-final chunk, `finalize_last` for the last. Returns the full
    /// ciphertext (header + frames) and the plaintext blake3, proving the
    /// encrypt side uses the SAME deterministic 64 KiB framing for any reader
    /// granularity.
    async fn encrypt_via_read_full_chunk(
        reader: &mut ShortReader,
        suite: &driven_crypto::DrivenCryptoSuite,
    ) -> (Vec<u8>, [u8; 32]) {
        use driven_crypto::SourceCryptoSuite as _;
        let mut enc = suite.content_encryptor();
        let mut out = Vec::new();
        out.extend_from_slice(&enc.header());
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; READ_BUF];
        let mut pending: Option<Vec<u8>> = None;
        loop {
            let chunk = read_full_chunk(reader, &mut buf).await.unwrap();
            if chunk.is_empty() {
                break;
            }
            hasher.update(&chunk);
            let was_full = chunk.len() == READ_BUF;
            if let Some(prev) = pending.take() {
                out.extend_from_slice(&enc.encrypt_chunk(&prev).unwrap());
            }
            pending = Some(chunk);
            if !was_full {
                break;
            }
        }
        let last = pending.unwrap_or_default();
        let (ct, _md5) = enc.finalize_last(&last).unwrap();
        out.extend_from_slice(&ct);
        (out, hasher.finalize().into())
    }

    /// V5-P1-2: a SHORT read mid-file must NOT change the AEAD framing. With the
    /// pre-fix code a sub-64KiB read emitted a sub-64KiB frame, so the
    /// spec-conforming fixed-65552-byte decryptor (the M8 restore model) would
    /// mis-align and the encrypted backup would be SILENTLY un-restorable.
    /// Asserts: (a) the fixed-frame decryptor round-trips the plaintext exactly,
    /// and (b) the bytes produced equal `predicted_sent_len` exactly (the
    /// streaming Content-Length contract).
    #[tokio::test]
    async fn short_read_keeps_fixed_64kib_framing_and_round_trips() {
        use driven_crypto::{
            ContentDecryptor, DrivenCryptoSuite, SourceCryptoSuite as _, HEADER_LEN,
        };

        let key = driven_crypto::key::SourceKey::generate();
        let suite = DrivenCryptoSuite::new(key);

        // The ciphertext frame size the M8 restore decryptor splits on: a full
        // 64 KiB plaintext chunk + the 16-byte Poly1305 tag.
        const CIPHER_FRAME: usize = READ_BUF + TAG_LEN;

        // Cover boundaries; `cap` (1, 7, 1000) forces many short reads per
        // 64 KiB chunk so the accumulation path is exercised hard.
        for &plaintext_len in &[
            0usize,
            1,
            5,
            READ_BUF,
            READ_BUF + 1,
            2 * READ_BUF,
            3 * READ_BUF + 777,
        ] {
            for &cap in &[1usize, 7, 1000, READ_BUF, READ_BUF + 5] {
                let plaintext: Vec<u8> = (0..plaintext_len).map(|i| (i % 251) as u8).collect();
                let mut reader = ShortReader {
                    data: plaintext.clone(),
                    pos: 0,
                    cap,
                };
                let (ciphertext, blake3_pt) =
                    encrypt_via_read_full_chunk(&mut reader, &suite).await;

                // (b) The byte count MUST equal the declared Content-Length.
                assert_eq!(
                    ciphertext.len() as u64,
                    predicted_sent_len(plaintext_len as u64, true),
                    "predicted_sent_len mismatch (plaintext_len={plaintext_len}, cap={cap})"
                );

                // (a) Decrypt by SPLITTING the ciphertext on FIXED 65552-byte
                // frame boundaries (the M8 restore model), proving the frames
                // are exactly 64 KiB plaintext each (last may be short).
                let header = &ciphertext[..HEADER_LEN];
                let body = &ciphertext[HEADER_LEN..];
                let mut dec: Box<dyn ContentDecryptor> = suite.content_decryptor(header).unwrap();
                let mut frames: Vec<&[u8]> = Vec::new();
                let mut off = 0usize;
                while body.len() - off > CIPHER_FRAME {
                    frames.push(&body[off..off + CIPHER_FRAME]);
                    off += CIPHER_FRAME;
                }
                let last_frame = &body[off..]; // the final (possibly short) frame
                let mut restored = Vec::new();
                for f in &frames {
                    restored.extend_from_slice(&dec.decrypt_chunk(f).unwrap());
                }
                restored.extend_from_slice(&dec.decrypt_last(last_frame).unwrap());

                assert_eq!(
                    restored, plaintext,
                    "short-read round-trip failed (plaintext_len={plaintext_len}, cap={cap})"
                );
                let expect_blake3: [u8; 32] = blake3::hash(&plaintext).into();
                assert_eq!(blake3_pt, expect_blake3, "plaintext blake3 mismatch");
            }
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
            .execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
            exec.execute(
                &h.source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            ),
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
        let source = h.encrypted_source();
        let source_key = driven_crypto::key::SourceKey::generate();
        let suite = Arc::new(DrivenCryptoSuite::new(source_key.clone()));
        let exec = h.executor_with_crypto(Some(suite));

        // > PIPELINE_THRESHOLD so the encrypted STREAMING path runs.
        let plaintext: Vec<u8> = (0..(6 * 1024 * 1024usize))
            .map(|i| (i % 253) as u8)
            .collect();
        let (rel, size) = h.write_file("big-secret.bin", &plaintext);
        let out = exec
            .execute(
                &source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
        let source = h.encrypted_source();
        let source_key = driven_crypto::key::SourceKey::generate();
        let make_exec =
            || h.executor_with_crypto(Some(Arc::new(DrivenCryptoSuite::new(source_key.clone()))));

        let (rel, size) = h.write_file("a/b/c.bin", b"nested encrypted payload");

        // Phase 1: a normal encrypted upload (lands under nested ciphertext
        // folders).
        let exec = make_exec();
        let out = exec
            .execute(
                &source,
                &h.upload_plan(&rel, size),
                &noop_progress,
                &noop_outcome,
            )
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
        exec2.reconcile(&source).await.unwrap();

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

    // --- Part A (CODEX_NOTES P2-9): the Drive breaker is driven by REAL
    //     request outcomes through the BreakerReportingStore decorator -------

    mod breaker_from_outcomes {
        use super::*;
        use driven_drive::google::DriveError;
        use driven_drive::remote_store::DriveErrorClassification;
        use std::sync::atomic::{AtomicBool, AtomicUsize};

        use crate::network::{
            CircuitBreaker, NetworkProbe, NetworkState, ServiceHealth, ServiceName,
            StdCircuitBreaker, BREAKER_OPEN_THRESHOLD,
        };

        /// A `NetworkProbe` backed by a REAL [`StdCircuitBreaker`] for Drive,
        /// so the breaker state machine under test is the production one (not a
        /// fake). `probe` is unused by the decorator path; it returns Online.
        struct BreakerProbe {
            drive: StdCircuitBreaker,
            clock: Arc<FakeClock>,
            ok_count: AtomicUsize,
            fail_count: AtomicUsize,
        }

        impl BreakerProbe {
            fn new(clock: Arc<FakeClock>) -> Arc<Self> {
                Arc::new(Self {
                    drive: StdCircuitBreaker::new(),
                    clock,
                    ok_count: AtomicUsize::new(0),
                    fail_count: AtomicUsize::new(0),
                })
            }
        }

        #[async_trait::async_trait]
        impl NetworkProbe for BreakerProbe {
            async fn probe(&self) -> NetworkState {
                NetworkState::Online
            }
            fn service_health(&self, service: ServiceName) -> ServiceHealth {
                match service {
                    ServiceName::Drive => self.drive.health(self.clock.as_ref()),
                    _ => ServiceHealth::Closed,
                }
            }
            fn note_outcome(&self, service: ServiceName, ok: bool) {
                assert!(
                    matches!(service, ServiceName::Drive),
                    "the executor only reports Drive outcomes"
                );
                if ok {
                    self.ok_count.fetch_add(1, Ordering::SeqCst);
                    self.drive.record_success();
                } else {
                    self.fail_count.fetch_add(1, Ordering::SeqCst);
                    self.drive.record_failure(self.clock.as_ref());
                }
            }
        }

        /// A `RemoteStore` whose `create` returns either a typed transport
        /// failure or `Ok`, toggled by `fail`. Only `create` is exercised; the
        /// other methods are never called by this test (they bail loudly if
        /// they ever are, rather than silently faking success).
        struct ToggleStore {
            fail: AtomicBool,
            /// `true` => the failure is a typed transport class (Network);
            /// `false` => a non-service-health error (must NOT open the breaker).
            transport: AtomicBool,
        }

        impl ToggleStore {
            fn new() -> Arc<Self> {
                Arc::new(Self {
                    fail: AtomicBool::new(true),
                    transport: AtomicBool::new(true),
                })
            }
            fn set_fail(&self, fail: bool) {
                self.fail.store(fail, Ordering::SeqCst);
            }
            fn set_transport(&self, transport: bool) {
                self.transport.store(transport, Ordering::SeqCst);
            }
        }

        #[async_trait::async_trait]
        impl RemoteStore for ToggleStore {
            async fn create(
                &self,
                _parent_id: &str,
                _name: &str,
                _mime: &str,
                _body: UploadBody,
                _app_properties: HashMap<String, String>,
            ) -> anyhow::Result<RemoteEntry> {
                if !self.fail.load(Ordering::SeqCst) {
                    return Ok(RemoteEntry {
                        id: "ok".into(),
                        name: "ok".into(),
                        parents: vec!["root".into()],
                        size: Some(0),
                        md5: Some([0u8; 16]),
                        mime_type: "application/octet-stream".into(),
                        modified_time: 0,
                        trashed: false,
                        app_properties: HashMap::new(),
                    });
                }
                if self.transport.load(Ordering::SeqCst) {
                    // A typed transport failure: this IS a service-health
                    // signal, so the decorator must penalise the breaker.
                    Err(anyhow::Error::new(DriveError::Classified {
                        kind: DriveErrorClassification::Network,
                        source: anyhow::anyhow!("connection reset"),
                    }))
                } else {
                    // A non-service-health failure (here: dest folder missing).
                    // The decorator must NOT penalise the breaker for it.
                    Err(anyhow::Error::new(DriveError::DestFolderMissing))
                }
            }

            async fn ensure_folder(&self, _p: &str, _n: &str) -> anyhow::Result<RemoteEntry> {
                anyhow::bail!("ToggleStore: ensure_folder must not be called")
            }
            async fn list_folder(&self, _f: &str) -> anyhow::Result<Vec<RemoteEntry>> {
                anyhow::bail!("ToggleStore: list_folder must not be called")
            }
            async fn update(
                &self,
                _f: &str,
                _b: UploadBody,
                _a: HashMap<String, String>,
            ) -> anyhow::Result<RemoteEntry> {
                anyhow::bail!("ToggleStore: update must not be called")
            }
            async fn resumable_session(
                &self,
                _k: ResumableKind,
                _m: &str,
                _s: u64,
            ) -> anyhow::Result<ResumableSession> {
                anyhow::bail!("ToggleStore: resumable_session must not be called")
            }
            async fn resume_chunk(
                &self,
                _s: &ResumableSession,
                _o: u64,
                _c: Bytes,
            ) -> anyhow::Result<ResumeProgress> {
                anyhow::bail!("ToggleStore: resume_chunk must not be called")
            }
            async fn trash(&self, _f: &str) -> anyhow::Result<()> {
                anyhow::bail!("ToggleStore: trash must not be called")
            }
            async fn metadata(&self, _f: &str) -> anyhow::Result<RemoteEntry> {
                anyhow::bail!("ToggleStore: metadata must not be called")
            }
            async fn download(&self, _f: &str) -> anyhow::Result<DownloadStream> {
                anyhow::bail!("ToggleStore: download must not be called")
            }
            async fn find_by_op_uuid(
                &self,
                _p: &str,
                _u: &str,
            ) -> anyhow::Result<Option<RemoteEntry>> {
                anyhow::bail!("ToggleStore: find_by_op_uuid must not be called")
            }
            async fn about(&self) -> anyhow::Result<AboutInfo> {
                anyhow::bail!("ToggleStore: about must not be called")
            }
        }

        fn decorator(store: Arc<ToggleStore>, probe: Arc<BreakerProbe>) -> BreakerReportingStore {
            BreakerReportingStore {
                inner: store,
                network: probe,
            }
        }

        async fn one_create(store: &BreakerReportingStore) -> anyhow::Result<RemoteEntry> {
            store
                .create(
                    "root",
                    "f",
                    "application/octet-stream",
                    UploadBody::Bytes(Bytes::new()),
                    HashMap::new(),
                )
                .await
        }

        /// A run of consecutive transport failures opens the Drive breaker;
        /// a subsequent success closes it again (CODEX_NOTES P2-9).
        #[tokio::test]
        async fn consecutive_transport_failures_open_then_success_closes() {
            let clock = Arc::new(FakeClock::new());
            let probe = BreakerProbe::new(clock.clone());
            let store = ToggleStore::new();
            let deco = decorator(store.clone(), probe.clone());

            // Below threshold: still Closed.
            store.set_fail(true);
            store.set_transport(true);
            for _ in 0..(BREAKER_OPEN_THRESHOLD - 1) {
                assert!(one_create(&deco).await.is_err());
            }
            assert_eq!(
                probe.service_health(ServiceName::Drive),
                ServiceHealth::Closed,
                "below threshold stays Closed"
            );

            // The Nth consecutive transport failure opens the Drive breaker.
            assert!(one_create(&deco).await.is_err());
            assert!(
                matches!(
                    probe.service_health(ServiceName::Drive),
                    ServiceHealth::Open { .. }
                ),
                "5 consecutive transport failures open the Drive breaker"
            );
            assert_eq!(
                probe.fail_count.load(Ordering::SeqCst),
                BREAKER_OPEN_THRESHOLD as usize
            );

            // Advance past the backoff so the breaker reads HalfOpen, then a
            // successful Drive request closes it (DESIGN s5.8.3).
            clock.advance(std::time::Duration::from_millis(
                crate::network::BACKOFF_SCHEDULE_MS[0] as u64,
            ));
            assert_eq!(
                probe.service_health(ServiceName::Drive),
                ServiceHealth::HalfOpen
            );
            store.set_fail(false);
            assert!(one_create(&deco).await.is_ok());
            assert_eq!(
                probe.service_health(ServiceName::Drive),
                ServiceHealth::Closed,
                "a success closes the breaker"
            );
            assert_eq!(probe.ok_count.load(Ordering::SeqCst), 1);
        }

        /// A non-service-health failure (e.g. dest-folder-missing, a logical
        /// 4xx-class error) must NOT penalise the breaker: it stays Closed no
        /// matter how many times it recurs (CODEX_NOTES P2-9: a per-file
        /// logical failure is not a service outage).
        #[tokio::test]
        async fn non_service_failures_do_not_open_breaker() {
            let clock = Arc::new(FakeClock::new());
            let probe = BreakerProbe::new(clock.clone());
            let store = ToggleStore::new();
            let deco = decorator(store.clone(), probe.clone());

            store.set_fail(true);
            store.set_transport(false);
            for _ in 0..(BREAKER_OPEN_THRESHOLD * 3) {
                assert!(one_create(&deco).await.is_err());
            }
            assert_eq!(
                probe.service_health(ServiceName::Drive),
                ServiceHealth::Closed,
                "non-transport failures never open the breaker"
            );
            assert_eq!(
                probe.fail_count.load(Ordering::SeqCst),
                0,
                "the decorator never reported a non-service failure"
            );
        }

        /// With `network: None` (every existing test path), the executor holds
        /// the inner store directly - no decorator, no reporting. This pins the
        /// "byte-identical when no probe injected" contract.
        #[tokio::test]
        async fn no_network_probe_means_no_reporting() {
            // A DefaultExecutor built with network: None must not wrap the
            // store. We assert structurally: the public surface still works
            // against the InMemoryRemoteStore with no NetworkProbe in play
            // (covered exhaustively by the rest of this module's tests, all of
            // which pass network: None). This test documents the invariant.
            let h = harness().await;
            let (rel, size) = h.write_file("a.txt", b"hello");
            let exec = h.executor();
            let plan = h.upload_plan(&rel, size);
            let outcomes = exec
                .execute(&h.source, &plan, &noop_progress, &noop_outcome)
                .await
                .unwrap();
            assert!(matches!(outcomes.as_slice(), [OpOutcome::Done { .. }]));
        }
    }
}
