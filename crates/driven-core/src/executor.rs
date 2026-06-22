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
//! This module is the I/O-free contract the M3 executor implementer fills.
//! It codes against the injected seams - [`RemoteStore`], [`StateRepo`],
//! [`Pacer`], and the optional [`SourceCryptoSuite`] - so the whole
//! executor is exercisable against `InMemoryRemoteStore` + `FakeClock`
//! with no real Drive (DESIGN s14).

use std::sync::Arc;

use driven_crypto::SourceCryptoSuite;
use driven_drive::remote_store::RemoteStore;

use crate::pacer::Pacer;
use crate::state::{SourceRow, StateRepo};
use crate::types::{ErrorCode, ExecProgress, Plan, RelativePath};

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
/// set once. Not consumed by the [`Executor`] trait methods themselves -
/// they are captured by the impl - but declared here as the canonical
/// dependency surface the executor needs.
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

use serde::{Deserialize, Serialize};
