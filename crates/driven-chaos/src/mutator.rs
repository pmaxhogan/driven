//! Continuous-mutation harness types (STRESS_HARNESS s4).
//!
//! Two mutator flavours feed the soak scenarios (s3.6) and `fuzz` mode
//! (s4.3):
//!
//! The [`FsMutation`] loop runs on a dedicated OS thread (NOT a tokio
//! task) so the mutator's scheduling is independent of Driven's I/O
//! reactor. The [`DriveMutation`] commands drive the fake remote's
//! fault-injection surface (STRESS_HARNESS s5).
//!
//! The interface declares the command enums and the driver entry-point
//! signatures the dispatch calls; Phase-2 fills the mutation loop and the
//! `fuzz` weighted-distribution driver.

use std::path::PathBuf;
use std::time::Duration;

/// A filesystem mutation a soak scenario runs alongside Driven sync
/// (STRESS_HARNESS s4.1). Each variant maps to one s3.6 soak row.
#[derive(Debug, Clone)]
pub enum FsMutation {
    /// Edit a file every `every` (the `frequent-edits` row).
    EditFile {
        /// File to rewrite each tick.
        path: PathBuf,
        /// Interval between edits.
        every: Duration,
    },
    /// Lock then unlock a file every `every` (`frequent-lock-unlock`).
    LockUnlock {
        /// File to lock/unlock.
        path: PathBuf,
        /// Interval between lock cycles.
        every: Duration,
    },
    /// Hold a file write-exclusive for the duration (`constantly-locked-db`).
    HoldLocked {
        /// File to keep locked.
        path: PathBuf,
    },
    /// `O_TRUNC + write` every tick (`truncate-and-rewrite`).
    TruncateRewrite {
        /// File to truncate and rewrite.
        path: PathBuf,
        /// Interval between rewrites.
        every: Duration,
        /// Byte length to rewrite each tick.
        bytes: usize,
    },
    /// Append `chunk` bytes every tick (`append-only-log`).
    AppendOnly {
        /// File to append to.
        path: PathBuf,
        /// Interval between appends.
        every: Duration,
        /// Bytes appended each tick.
        chunk: usize,
    },
    /// Rename files in `dir` every tick (`rename-storm`).
    RenameStorm {
        /// Directory whose files are renamed.
        dir: PathBuf,
        /// Interval between renames.
        every: Duration,
    },
    /// Word/Photoshop tmp-then-rename pattern (`editor-tilde-dance`).
    EditorTildeDance {
        /// The file the editor pattern targets.
        target: PathBuf,
        /// Interval between dance cycles.
        every: Duration,
    },
    /// Atomic replace via `.tmp` + rename (`replace-via-atomic-rename`).
    AtomicReplace {
        /// File replaced atomically each tick.
        path: PathBuf,
        /// Interval between replacements.
        every: Duration,
    },
}

/// A Drive-side fault-injection command (STRESS_HARNESS s4.2). Each maps
/// onto an [`driven_drive::fake::InMemoryRemoteStore`] fault builder (s5).
#[derive(Debug, Clone)]
pub enum DriveMutation {
    /// Trip `drive.rate_limited` after N requests.
    InjectRateLimit {
        /// Requests served before the rate-limit trips.
        after_requests: u64,
    },
    /// Trip a 5xx after N requests.
    InjectFiveHundred {
        /// Requests served before the 5xx trips.
        after_requests: u64,
    },
    /// Latch `auth.invalid_grant`.
    InjectInvalidGrant,
    /// Trip `drive.quota_exhausted` after N committed bytes.
    InjectQuotaExhausted {
        /// Committed-byte budget before quota is exhausted.
        after_bytes: u64,
    },
    /// Invalidate the next resumable session after N accepted chunks.
    InvalidateResumableSession {
        /// Accepted chunks before the session invalidates.
        after_chunks: u32,
    },
    /// Latch an md5 mismatch after N uploads.
    InjectMd5Mismatch {
        /// Uploads before the mismatch latches.
        after_uploads: u32,
    },
    /// Drop the network for a bounded duration.
    DropNetwork {
        /// How long the simulated drop lasts.
        for_duration: Duration,
    },
    /// Delete the destination folder (`drive.dest_folder_missing`).
    DeleteDestFolder,
    /// Trash a Driven-owned file whose name matches the pattern.
    TrashOurFile {
        /// Glob/substring identifying the file to trash.
        name_pattern: String,
    },
}
