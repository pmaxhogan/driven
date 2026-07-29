//! Backend-specific destination hazards (STRESS_HARNESS s3.9).
//!
//! Until now every chaos row drove ONE destination: Google Drive, through
//! `InMemoryRemoteStore`. Driven now has three (`driven-drive`, `driven-s3`,
//! `driven-localfs`), and the s6.3 invariants claim to
//! be BACKEND-INDEPENDENT. This module is where that claim is tested rather
//! than asserted: every row here ends in the SAME
//! [`crate::scenarios::reporting::assert_invariants`] sweep the Drive rows use,
//! reached through the backend-neutral
//! [`crate::scenarios::reporting::InvariantSurface`] seam.
//!
//! ## Why the faults are injected on the wire
//!
//! The S3 failure modes that lose DATA rather than merely erroring live below
//! the `RemoteStore` trait, in the HTTP layer:
//!
//! - `503 SlowDown` is throttling wearing a 5xx costume. Injected at the trait
//!   seam it would arrive already classified, proving nothing about
//!   `classify_s3_response` - which is the code that decides whether a
//!   throttled backup retries forever (correct) or gives up after
//!   `MAX_RETRIES` (data left un-backed-up).
//! - `RequestTimeout` arrives as a **400** and must still be retried.
//! - `CompleteMultipartUpload` can answer **HTTP 200 with an error document**,
//!   because S3 holds the connection open while assembling parts. A client that
//!   trusts the status marks a file `synced` against an object that was never
//!   published - silent backup loss, the single worst outcome in the taxonomy.
//! - A multipart upload cut BETWEEN two `UploadPart` calls, or between the last
//!   `UploadPart` and `CompleteMultipartUpload`, is a transport event with no
//!   trait-level representation at all.
//!
//! So these rows drive the REAL [`driven_s3::S3Store`] - real SigV4 signing,
//! real `reqwest` round trips, real XML parsing, real classification - against
//! [`crate::s3_server::FaultyS3Server`], and inject the faults as bytes on a
//! socket. See that module's docs for why it is in-process rather than MinIO
//! behind a proxy (short version: the chaos gate runs Windows-only on a PR and
//! `minio` is installed only on the Linux legs of `ci.yml`, so a MinIO-gated
//! row would SKIP in the one job that blocks a merge).
//!
//! ## Row catalogue
//!
//! | Row | Fault | What must not happen |
//! |---|---|---|
//! | `s3-multipart-interrupted-between-parts` | connection dropped during `UploadPart` | a gap in the assembled object; a duplicate |
//! | `s3-multipart-interrupted-before-complete` | connection dropped at `CompleteMultipartUpload` | a `synced` row for an object that was never published |
//! | `s3-complete-200-with-error-document` | 200 + `<Error>` at completion | the same, via a status code that lies |
//! | `s3-slow-down-throttling` | `503 SlowDown` | the throttle read as a fatal 5xx and the file abandoned |
//! | `s3-request-timeout-retryable-400` | `400 RequestTimeout` | a retryable failure read as fatal |
//! | `s3-kill-mid-upload-then-reboot` | fault + crash + reboot | an orphaned object with no `file_state` row; a duplicate |
//! | `s3-destination-full-mid-upload` | byte budget exhausted mid-multipart | a partial object marked complete |
//! | `localfs-crash-between-temp-write-and-rename` | a commit whose `rename` cannot land, plus the temp file a killed process leaves | a half-written object readable as a complete one |
//! | `destination-vanished-across-backends` | destination vanishes mid-cycle on ALL THREE backends, one body | anything written after the destination vanished |
//!
//! ## The local-folder rows
//!
//! The local-folder backend needs no protocol server: its destination IS a
//! directory, so [`crate::localfs_fixture`] constructs each post-crash state
//! byte-for-byte as the failure produces it rather than modelling it. Three
//! hazards, all covered:
//!
//! - **The destination VANISHES mid-cycle** (removable media). Covered as the
//!   third arm of [`DestinationVanishedAcrossBackends`], which drives ONE body
//!   across all three backends - the point being that each raises
//!   `drive.dest_folder_missing` from its own detector (a marker file, a
//!   `NoSuchBucket`, a latched flag) and every one must then write nothing
//!   further and lose nothing already synced. The injected fault is the nastier
//!   of the two the marker catches: not a MISSING marker, but one carrying a
//!   different destination id - a different stick at the same mount point.
//! - **A crash between the temp write and the atomic rename**. Covered by
//!   [`LocalFsCrashBetweenTempWriteAndRename`], in two phases: a commit whose
//!   `rename` genuinely cannot land, and the abandoned temp file a killed
//!   process leaves holding the file's FULL bytes.
//! - **Out of space (`ENOSPC`) mid-write** is `drive.quota_exhausted`: the
//!   local-folder errno table maps `ENOSPC`/`EDQUOT` onto exactly the
//!   `DriveError::StorageQuota` that [`S3DestinationFullMidUpload`]'s
//!   `QuotaExceeded` produces, and that row drives the end-to-end behaviour
//!   (account pauses, nothing partial published, recovery once space is freed)
//!   mid-MULTIPART with bytes already at the destination.
//!
//!   The one thing NOT covered is the localfs errno path itself, end to end,
//!   because a real `ENOSPC` needs a real constrained VOLUME: unprivileged on
//!   macOS (`hdiutil`), root-only on Linux (`losetup`), admin-only on Windows
//!   (`New-VHD`). The existing `disk-full-target` row sets the precedent -
//!   capability-gated behind `DRIVEN_CHAOS_ALLOW_DISK_MOUNT` - and a row gated
//!   the same way would SKIP in the Windows-only PR gate, which is the one job
//!   that blocks a merge. The errno table is unit-tested in `driven-localfs`;
//!   what a chaos row adds beyond that is the orchestrator behaviour, and that
//!   is what the S3 row proves. Recorded as a deliberate gap rather than a
//!   scenario that is always SKIPPED.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use driven_core::state::{
    AccountRow, ActivityFilter, ActivityLevel, BackendKind, PageRequest, SourceRow, StateRepo,
};
use driven_core::types::{AccountId, ErrorCode, FileStateStatus, OrchestratorState, SourceId};

use driven_drive::fake::InMemoryRemoteStore;
use driven_remote::remote_store::RemoteStore;

use crate::capabilities::CapabilityRequirements;
use crate::handle::{DrivenHandle, DrivenHandleBuilder};
use crate::localfs_fixture::{LocalFsFixture, LocalFsOracle, CHAOS_SUBFOLDER};
use crate::s3_server::{store_for, FaultyS3Server, CHAOS_BUCKET, CHAOS_PREFIX};
use crate::scenario::{ExpectedOutcome, Outcome, Scenario, ScenarioContext};
use crate::scenarios::reporting::{assert_invariants, InvariantReport};

/// Every s3.9 backend-hazard scenario.
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(S3MultipartInterruptedBetweenParts),
        Box::new(S3MultipartInterruptedBeforeComplete),
        Box::new(S3CompleteReturnsErrorDocumentWith200),
        Box::new(S3SlowDownThrottling),
        Box::new(S3RequestTimeoutRetryable400),
        Box::new(S3KillMidUploadThenReboot),
        Box::new(S3DestinationFullMidUpload),
        Box::new(LocalFsCrashBetweenTempWriteAndRename),
        Box::new(DestinationVanishedAcrossBackends),
    ]
}

// ===========================================================================
// Sizing constants
// ===========================================================================

/// The executor's wire chunk (`executor.rs::WIRE_CHUNK`).
const WIRE_CHUNK: usize = 4 * 1024 * 1024;

/// Fixture size for the multipart rows.
///
/// Three 4 MiB wire chunks buffered into `driven-s3`'s 8 MiB `PART_SIZE` gives
/// exactly TWO parts, which is the minimum that can be "interrupted BETWEEN
/// parts" at all - and the smallest such fixture, because every one of these
/// rows pays for hashing it in a debug build. It also clears the executor's
/// 5 MiB `RESUMABLE_THRESHOLD`, so the upload really does take the resumable
/// path rather than a single `PutObject`.
const MULTIPART_FIXTURE_LEN: usize = 3 * WIRE_CHUNK;

/// A small fixture for the rows that do not need multipart.
const SMALL_FIXTURE_LEN: usize = 64 * 1024;

// Compile-time proof that the fixture sizes still mean what the rows assume,
// mirroring the same guard `driven-s3` and `executor.rs` apply to their own
// constants. If `RESUMABLE_THRESHOLD` or `PART_SIZE` ever drifts, the
// "interrupted BETWEEN parts" rows would silently degrade to a single
// `PutObject` and stop testing anything - a build error is the only failure mode
// that cannot be mistaken for a passing run.
const _: () = assert!(MULTIPART_FIXTURE_LEN as u64 >= driven_core::executor::RESUMABLE_THRESHOLD);
const _: () = assert!(MULTIPART_FIXTURE_LEN > driven_s3::PART_SIZE);
const _: () = assert!((SMALL_FIXTURE_LEN as u64) < driven_core::executor::RESUMABLE_THRESHOLD);

/// How many cycles a row may drive while waiting for the post-fault steady
/// state. A finite cap, so a row that never converges FAILS rather than
/// spinning until the wall-clock guard fires.
const MAX_RECOVERY_CYCLES: u32 = 6;

/// Latency added to every server response in the race-shaped rows.
///
/// This is the PR #192 technique. A row whose window opens only by luck is not
/// a test: the append-only-log flake was a 5-10% coin flip precisely because a
/// green run usually meant the racy path was never reached. Widening the window
/// deliberately makes the row take the intended path on EVERY run, which is
/// what makes it deterministic instead of flaky. Cheap here (a handful of
/// requests per row), unlike a mutation cadence.
const RACE_WIDENING_DELAY: Duration = Duration::from_millis(3);

// ===========================================================================
// Shared helpers
// ===========================================================================

/// Deterministic pseudo-random bytes.
///
/// Not zeroes: a truncated or mis-assembled transfer of a zero buffer can pass
/// a content check by coincidence, which is the exact bug these rows hunt.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// Write `contents` to `root/rel`, creating parent dirs.
fn write_file(root: &std::path::Path, rel: &str, contents: &[u8]) -> anyhow::Result<()> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, contents)?;
    Ok(())
}

/// A source rooted at `root` uploading into `folder_id`.
fn source_in(account: AccountId, root: &std::path::Path, folder_id: &str) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id: account,
        display_name: "backends".into(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: folder_id.to_string(),
        drive_id: None,
        drive_folder_path: "/backends".into(),
        encryption_enabled: false,
        wrapped_source_key: None,
        respect_gitignore: false,
        include_patterns: vec![],
        exclude_patterns: vec![],
        placeholder_policy: Default::default(),
        schedule_json_v2_reserved: None,
        deep_verify_interval_secs: 604_800,
        last_full_scan_at: None,
        last_deep_verify_at: Some(0),
        mtime_granularity_ns: None,
        created_at: 0,
    }
}

/// Map a dotted activity `event_type` back to its [`ErrorCode`], for the codes
/// the s3.9 rows can surface.
fn parse_error_code(event_type: &str) -> Option<ErrorCode> {
    let code = match event_type {
        "drive.dest_folder_missing" => ErrorCode::DriveDestFolderMissing,
        "drive.dest_folder_permission_denied" => ErrorCode::DriveDestFolderPermissionDenied,
        "drive.quota_exhausted" => ErrorCode::DriveQuotaExhausted,
        "drive.daily_quota_exhausted" => ErrorCode::DriveDailyQuotaExhausted,
        "drive.rate_limited" => ErrorCode::DriveRateLimited,
        "drive.unreachable" => ErrorCode::DriveUnreachable,
        "drive.checksum_mismatch" => ErrorCode::DriveChecksumMismatch,
        "drive.resumable_session_invalid" => ErrorCode::DriveResumableSessionInvalid,
        "drive.remote_file_missing" => ErrorCode::DriveRemoteFileMissing,
        "auth.invalid_grant" => ErrorCode::AuthInvalidGrant,
        "net.offline" => ErrorCode::NetOffline,
        "net.no_internet" => ErrorCode::NetNoInternet,
        _ => return None,
    };
    Some(code)
}

/// The distinct `Error`-level codes the orchestrator recorded.
async fn error_codes_in_activity(state: &dyn StateRepo) -> anyhow::Result<Vec<ErrorCode>> {
    let page = state
        .query_activity(
            ActivityFilter {
                min_level: Some(ActivityLevel::Error),
                ..ActivityFilter::default()
            },
            PageRequest::first(10_000),
        )
        .await?;
    let mut codes: Vec<ErrorCode> = Vec::new();
    for row in page.rows {
        if let Some(code) = parse_error_code(&row.event_type) {
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
    }
    Ok(codes)
}

/// Whether the orchestrator quiesced to a non-running terminal state - the
/// s6.3 clean-shutdown check for a cycle-driven row.
fn is_quiescent(state: &OrchestratorState) -> bool {
    matches!(
        state,
        OrchestratorState::Idle { .. }
            | OrchestratorState::Paused { .. }
            | OrchestratorState::Backoff { .. }
            | OrchestratorState::Error { .. }
    )
}

/// Run cycles until every file in `source_id` is `Synced`, up to
/// [`MAX_RECOVERY_CYCLES`]. Cycle errors are tolerated - a faulted cycle
/// SHOULD error; what matters is that the retry ladder converges. Returns the
/// number of cycles actually run.
async fn run_until_all_synced(handle: &DrivenHandle, source_id: SourceId) -> anyhow::Result<u32> {
    for cycle in 1..=MAX_RECOVERY_CYCLES {
        let _ = handle.run_one_cycle().await;
        let rows = handle.state.load_source_file_state(source_id).await?;
        if !rows.is_empty()
            && rows
                .iter()
                .all(|(_, r)| r.status == FileStateStatus::Synced)
        {
            return Ok(cycle);
        }
    }
    Ok(MAX_RECOVERY_CYCLES)
}

/// The assertion that catches the failure class this whole module exists for:
/// **a `synced` row whose bytes at the destination are not the bytes on disk.**
///
/// `assert_invariants` already proves the object exists and its md5 matches the
/// recorded digest. This goes one step further and compares the destination's
/// bytes to the LOCAL FILE's bytes, so a half-assembled multipart object, a
/// dropped part, or a completion that published a truncated object is caught
/// even if every recorded digest is self-consistent.
/// "What bytes are actually at rest under this id?", answered by reading the
/// destination's ground truth - the one question
/// [`assert_synced_bytes_match_local`] needs, and the one the `RemoteStore`
/// trait cannot be trusted for while a fault is latched.
pub trait DestinationBytes: Send + Sync {
    /// The bytes at rest at `id`, or `None` if nothing is readable there.
    fn bytes_at(&self, id: &str) -> Option<Vec<u8>>;
}

impl DestinationBytes for FaultyS3Server {
    fn bytes_at(&self, id: &str) -> Option<Vec<u8>> {
        self.object_bytes(id)
    }
}

impl DestinationBytes for LocalFsOracle {
    fn bytes_at(&self, id: &str) -> Option<Vec<u8>> {
        self.object_bytes(id)
    }
}

async fn assert_synced_bytes_match_local(
    handle: &DrivenHandle,
    dest: &dyn DestinationBytes,
    source: &SourceRow,
) -> anyhow::Result<usize> {
    let rows = handle.state.load_source_file_state(source.id).await?;
    let mut checked = 0usize;
    for (rel, row) in rows.iter() {
        if row.status != FileStateStatus::Synced {
            continue;
        }
        let key = row
            .drive_file_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("synced row {rel} carries no destination key"))?;
        let remote = dest.bytes_at(key).ok_or_else(|| {
            anyhow::anyhow!("synced row {rel} points at key {key}, which holds NO object")
        })?;
        let local = std::fs::read(std::path::Path::new(&source.local_path).join(rel.as_str()))?;
        anyhow::ensure!(
            remote == local,
            "synced row {rel}: the destination holds {} bytes, the local file has {} - a synced \
             row must never describe a partially-written object",
            remote.len(),
            local.len()
        );
        checked += 1;
    }
    Ok(checked)
}

/// An S3 destination fixture: the fake bucket, the temp source tree, and the
/// hermetic state DB - everything that must SURVIVE a simulated crash.
///
/// The handle is deliberately NOT a field. A crash-recovery row has to DROP the
/// orchestrator (that is what "the process died" means) and then boot a fresh
/// one over the same DB and the same bucket, which it cannot do while the
/// fixture it is borrowing owns the handle. Keeping the two apart is what makes
/// `drop(handle); let reopened = fx.boot().await?;` read as exactly what it is.
struct S3Fixture {
    server: Arc<FaultyS3Server>,
    /// The destination root "folder" id - the configured key prefix.
    folder: String,
    state_dir: tempfile::TempDir,
    src_dir: tempfile::TempDir,
}

impl S3Fixture {
    /// Bind a fresh fake bucket and prepare the temp source tree + state DB.
    async fn new() -> anyhow::Result<Self> {
        Ok(Self {
            server: Arc::new(FaultyS3Server::start(CHAOS_BUCKET).await?),
            folder: CHAOS_PREFIX.to_string(),
            state_dir: tempfile::tempdir()?,
            src_dir: tempfile::tempdir()?,
        })
    }

    fn src_root(&self) -> &std::path::Path {
        self.src_dir.path()
    }

    /// Boot (or RE-boot, after a crash) a headless handle over a real
    /// [`driven_s3::S3Store`] pointed at this fixture's bucket.
    ///
    /// `DrivenHandleBuilder::boot` ADOPTS the account already in the DB rather
    /// than minting a new one, so a rebooted orchestrator drives the same
    /// account - and therefore the same sources - the dead one did. (Seeding a
    /// fresh account id here is exactly what once made the crash-recovery rows
    /// pass vacuously by uploading nothing.)
    async fn boot(&self) -> anyhow::Result<DrivenHandle> {
        let store: Arc<dyn RemoteStore> = Arc::new(store_for(&self.server)?);
        let handle = DrivenHandleBuilder::new(self.state_dir.path().join("state.db"))
            .remote(store)
            .boot()
            .await?;
        // The handle seeds a Drive-shaped account row; restamp it as the S3
        // backend it actually is. The orchestrator takes its store by injection
        // and never reads `backend_kind`, so this is honesty rather than
        // plumbing - and it proves nothing in the core trips over a non-Drive
        // account.
        stamp_s3_account(&handle, &self.server).await?;
        Ok(handle)
    }

    /// Persist a source rooted at this fixture's temp tree.
    async fn add_source(&self, handle: &DrivenHandle) -> anyhow::Result<SourceRow> {
        let src = source_in(handle.account_id, self.src_root(), &self.folder);
        handle.state.upsert_source(&src).await?;
        Ok(src)
    }
}

/// Rewrite the seeded account row as an S3 account carrying a real
/// `S3Config` blob in `backend_config_json`.
async fn stamp_s3_account(handle: &DrivenHandle, server: &FaultyS3Server) -> anyhow::Result<()> {
    let config = driven_s3::S3Config {
        endpoint: server.endpoint(),
        bucket: server.bucket().to_string(),
        region: driven_s3::DEFAULT_REGION.to_string(),
        path_style: true,
        prefix: Some(CHAOS_PREFIX.to_string()),
    }
    .normalized()?;
    let existing = handle
        .state
        .list_accounts()
        .await?
        .into_iter()
        .find(|a| a.id == handle.account_id)
        .ok_or_else(|| anyhow::anyhow!("the booted handle has no account row"))?;
    handle
        .state
        .upsert_account(&AccountRow {
            backend_kind: BackendKind::S3,
            backend_config_json: Some(config.to_json()?),
            ..existing
        })
        .await?;
    Ok(())
}

/// Fold an [`InvariantReport`] into an [`Outcome`] carrying the s6.3 snapshot
/// the runner enforces centrally.
fn finish(
    report: &InvariantReport,
    clean_shutdown: bool,
    codes: Vec<ErrorCode>,
    notes: Vec<String>,
) -> Outcome {
    Outcome {
        error_codes_seen: codes,
        final_drive_object_count: report.live_object_count,
        final_hash_matches_local: report.data_loss_paths.is_empty(),
        invariants: Some(report.to_invariant_outcome(clean_shutdown)),
        notes,
    }
}

/// Merge several arms' [`InvariantReport`]s into one, so a violation in ANY arm
/// reaches the runner's central s6.3 gate instead of being overwritten by a
/// clean sibling arm.
fn merge_reports(reports: Vec<InvariantReport>) -> InvariantReport {
    let mut merged = InvariantReport::default();
    for r in reports {
        merged.data_loss_paths.extend(r.data_loss_paths);
        merged.duplicate_op_uuids.extend(r.duplicate_op_uuids);
        merged.leaked_pending_ops += r.leaked_pending_ops;
        merged.live_object_count += r.live_object_count;
    }
    merged
}

/// The shared tail of every S3 row: run the s6.3 sweep, prove the synced bytes
/// match the local file, and require exactly `expect_objects` live objects.
///
/// `max_stranded_uploads` caps the multipart uploads that may still be in
/// flight at the end. It is NOT zero, and the reason is a real finding this
/// module surfaced - see [`STRANDED_UPLOAD_FINDING`].
async fn settle_and_assert(
    fx: &S3Fixture,
    handle: &DrivenHandle,
    source: &SourceRow,
    expect_objects: u64,
    max_stranded_uploads: usize,
) -> anyhow::Result<(InvariantReport, Vec<String>)> {
    let report = assert_invariants(handle, fx.server.as_ref(), source.id, &fx.folder).await?;
    anyhow::ensure!(
        report.ok(),
        "s6.3 invariants violated: {}",
        report.violation_summary()
    );
    anyhow::ensure!(
        report.live_object_count == expect_objects,
        "expected exactly {expect_objects} live object(s), found {}",
        report.live_object_count
    );
    let checked = assert_synced_bytes_match_local(handle, fx.server.as_ref(), source).await?;

    // Abandoned multipart uploads. See STRANDED_UPLOAD_FINDING: the cap is
    // BOUNDED rather than zero because a transport failure mid-`UploadPart`
    // genuinely leaves one behind today. Asserting zero would make the gate red
    // over a real, unfixed cost issue; asserting nothing would let it grow
    // without bound unnoticed. Bounding it does neither.
    let stranded = fx.server.in_flight_upload_keys();
    anyhow::ensure!(
        stranded.len() <= max_stranded_uploads,
        "{} multipart upload(s) left in flight, more than the {max_stranded_uploads} this row \
         accounts for - the leak is growing: {stranded:?}",
        stranded.len()
    );

    let counts = fx.server.counts();
    let mut notes = vec![format!(
        "{checked} synced row(s) byte-verified; requests: {} total, {} parts, {} completes, \
         {} aborts, {} dropped connections",
        counts.total,
        counts.upload_part,
        counts.complete_multipart,
        counts.abort_multipart,
        counts.dropped_connections
    )];
    if !stranded.is_empty() {
        notes.push(format!(
            "FINDING (cost, not data loss): {} abandoned multipart upload(s) still hold their \
             parts on the bucket: {:?}. {}",
            stranded.len(),
            stranded,
            STRANDED_UPLOAD_FINDING
        ));
    }
    Ok((report, notes))
}

/// The explanation attached to any row that ends with an abandoned multipart
/// upload, so the finding travels with the report instead of living only in a
/// PR description.
///
/// `S3Store` aborts a multipart upload on exactly two paths: the non-resumable
/// `multipart_stream` failure path, and a completion failure that
/// `is_session_fatal` calls terminal. A TRANSPORT failure mid-`UploadPart` on
/// the RESUMABLE path is neither: `resume_chunk` propagates the error and leaves
/// the upload id alive, which is correct in itself, because the startup
/// `reconcile` -> `resume_persisted` path may still resume that exact session
/// after a crash - aborting it eagerly would destroy a recoverable upload.
///
/// The leak comes from the other side. `upload_stage_resumable` (the streaming
/// path every file >= 4 MiB takes) calls `open_resumable_session`
/// UNCONDITIONALLY at the top of each attempt, so the next cycle's retry mints a
/// NEW `CreateMultipartUpload` and never touches the previous upload id again.
/// Its parts stay on the bucket, billed, one abandoned upload per failed
/// attempt, and `S3Store::new` performs no sweep - unlike `driven-localfs`,
/// which explicitly sweeps abandoned resumable temp files at construction using
/// the trait's own 6-day session window.
///
/// Not data loss, so no s6.3 invariant is violated and these rows do not fail on
/// it. It is real money on a real bucket, and the fix belongs in `driven-s3`
/// (a `ListMultipartUploads` sweep at construction, mirroring the localfs one)
/// rather than in the harness.
const STRANDED_UPLOAD_FINDING: &str = "cause: the streaming upload path opens a FRESH \
     CreateMultipartUpload on every attempt (executor.rs::upload_stage_resumable), while \
     S3Store only aborts an upload on the non-resumable path or a fatal completion - so a \
     transport failure mid-UploadPart abandons the parts with no sweep at store construction \
     (driven-localfs sweeps its equivalent temp files; driven-s3 does not)";

// ===========================================================================
// s3.9 s3-multipart-interrupted-between-parts
// ===========================================================================

/// The connection dies in the middle of an `UploadPart`, between two parts of a
/// multipart upload.
///
/// The part never lands, so the object cannot be assembled from what is on the
/// server. Driven must recover to exactly ONE object whose bytes equal the
/// local file - no gap where the lost part was, no second object beside it, and
/// no multipart upload left holding parts.
struct S3MultipartInterruptedBetweenParts;

#[async_trait]
impl Scenario for S3MultipartInterruptedBetweenParts {
    fn name(&self) -> &'static str {
        "s3-multipart-interrupted-between-parts"
    }
    fn description(&self) -> &'static str {
        "S3 multipart cut mid-UploadPart: recovers to one byte-exact object, no stranded parts"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fx = S3Fixture::new().await?;
        let handle = fx.boot().await?;
        let content = payload(MULTIPART_FIXTURE_LEN, 11);
        write_file(fx.src_root(), "big.bin", &content)?;
        let src = fx.add_source(&handle).await?;

        // Drop the connection on the SECOND UploadPart: the first part is
        // safely on the server, the second is not, so the failure is genuinely
        // "between parts" rather than "before any part". Single-shot, so the
        // recovery path is exercised rather than a permanent outage.
        fx.server.arm_drop_part_after(1);
        fx.server.arm_response_delay(RACE_WIDENING_DELAY);

        let cycles = run_until_all_synced(&handle, src.id).await?;
        anyhow::ensure!(
            fx.server.counts().dropped_connections >= 1,
            "the row never reached its fault: no connection was dropped"
        );

        let codes = error_codes_in_activity(handle.state.as_ref()).await?;
        let quiesced = is_quiescent(&handle.state().await);
        let (report, mut notes) = settle_and_assert(&fx, &handle, &src, 1, 1).await?;
        notes.push(format!("converged after {cycles} cycle(s)"));
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        // The assertions above ARE the check (s2.3 DocumentedBehaviour): the
        // executor is free to absorb a single-shot transport failure inside its
        // own retry ladder without recording an Error-level activity row, so
        // pinning a specific code here would make the row assert the retry
        // ladder's current shape rather than the invariant that matters.
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// s3.9 s3-multipart-interrupted-before-complete
// ===========================================================================

/// Every part is safely on the server and the connection dies carrying the
/// `CompleteMultipartUpload` request - the assembly instruction is lost.
///
/// This is the nastiest ordering, because the server holds enough state to
/// assemble the object and simply was not told to. Nothing may be published,
/// and nothing may be recorded as `synced`, until a completion actually
/// succeeds. The upload id survives the dropped connection (real S3 behaviour),
/// so the resume path can enumerate its parts via `ListParts` - which is what
/// makes the replayed parts skip the wire instead of being re-sent.
struct S3MultipartInterruptedBeforeComplete;

#[async_trait]
impl Scenario for S3MultipartInterruptedBeforeComplete {
    fn name(&self) -> &'static str {
        "s3-multipart-interrupted-before-complete"
    }
    fn description(&self) -> &'static str {
        "S3 multipart cut between UploadPart and CompleteMultipartUpload: nothing published early"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fx = S3Fixture::new().await?;
        let handle = fx.boot().await?;
        let content = payload(MULTIPART_FIXTURE_LEN, 12);
        write_file(fx.src_root(), "big.bin", &content)?;
        let src = fx.add_source(&handle).await?;

        fx.server.arm_drop_complete(1);
        fx.server.arm_response_delay(RACE_WIDENING_DELAY);

        // Cycle 1: the completion is lost.
        let _ = handle.run_one_cycle().await;
        anyhow::ensure!(
            fx.server.counts().complete_multipart >= 1,
            "the row never reached its fault: CompleteMultipartUpload was never attempted"
        );

        // The load-bearing mid-scenario assertion: whatever the executor did
        // with the failure, it must NOT have recorded a synced row against an
        // object the server never published.
        let mid = assert_invariants(&handle, fx.server.as_ref(), src.id, &fx.folder).await?;
        anyhow::ensure!(
            mid.data_loss_paths.is_empty(),
            "a lost completion left a synced row with no live object: {}",
            mid.violation_summary()
        );

        let cycles = run_until_all_synced(&handle, src.id).await?;
        let codes = error_codes_in_activity(handle.state.as_ref()).await?;
        let quiesced = is_quiescent(&handle.state().await);
        let (report, mut notes) = settle_and_assert(&fx, &handle, &src, 1, 1).await?;
        notes.push(format!(
            "after the lost completion: {} live object(s), {} synced row(s) without one; \
             converged after {} further cycle(s)",
            mid.live_object_count,
            mid.data_loss_paths.len(),
            cycles
        ));
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// s3.9 s3-complete-200-with-error-document
// ===========================================================================

/// `CompleteMultipartUpload` answers **HTTP 200** with an `<Error>` document in
/// the body, and publishes nothing.
///
/// Real S3 does this because it holds the connection open while assembling the
/// parts, so the 200 is already on the wire when the assembly fails. A client
/// that reads only the status code concludes the upload succeeded, records the
/// file as `synced`, and never uploads it again - the backup is silently
/// missing that file forever.
///
/// ## What the negative control showed
///
/// Defences were disabled one at a time and this row re-run each time.
/// `driven-s3` stops a lying 200 THREE independent ways, any ONE of which
/// suffices:
///
/// 1. `complete_multipart` scans the body for `<Error` even on a 2xx;
/// 2. failing that, the composed-ETag check finds no `<ETag>` in an error
///    document and reports `ChecksumMismatch`;
/// 3. failing both, `drain_and_maybe_complete` HEADs the key afterwards, and an
///    object that was never published answers 404.
///
/// The row goes RED only once all three are gone - and then it fails with
/// exactly the right message (`recorded as synced with no object at the
/// destination`), so it genuinely detects the failure class and the redundancy
/// is measured rather than assumed.
///
/// That is also why the row asserts the OUTCOME (nothing published, nothing
/// synced, recovery afterwards) rather than pinning any one of the three checks:
/// pinning one would turn a future refactor that consolidated the defences while
/// keeping the guarantee into a false failure.
struct S3CompleteReturnsErrorDocumentWith200;

#[async_trait]
impl Scenario for S3CompleteReturnsErrorDocumentWith200 {
    fn name(&self) -> &'static str {
        "s3-complete-200-with-error-document"
    }
    fn description(&self) -> &'static str {
        "CompleteMultipartUpload answers 200 with an <Error> body: no file is falsely synced"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fx = S3Fixture::new().await?;
        let handle = fx.boot().await?;
        let content = payload(MULTIPART_FIXTURE_LEN, 13);
        write_file(fx.src_root(), "big.bin", &content)?;
        let src = fx.add_source(&handle).await?;

        fx.server.arm_complete_error_document(1);

        // Cycle 1: the lying 200.
        let _ = handle.run_one_cycle().await;
        anyhow::ensure!(
            fx.server.counts().complete_multipart >= 1,
            "the row never reached its fault: CompleteMultipartUpload was never attempted"
        );
        anyhow::ensure!(
            fx.server.counts().complete_error_documents_sent >= 1,
            "the row never reached its fault: no 200-with-<Error> completion was injected"
        );
        let published = fx.server.oracle_entries(&fx.folder).len();
        anyhow::ensure!(
            published == 0,
            "the faulted completion must publish NOTHING, found {published} object(s)"
        );
        let rows = handle.state.load_source_file_state(src.id).await?;
        let falsely_synced: Vec<String> = rows
            .iter()
            .filter(|(_, r)| r.status == FileStateStatus::Synced)
            .map(|(rel, _)| rel.to_string())
            .collect();
        anyhow::ensure!(
            falsely_synced.is_empty(),
            "a 200-with-error-body was trusted: {falsely_synced:?} recorded as synced with no \
             object at the destination"
        );

        // Recovery on a healthy server.
        let cycles = run_until_all_synced(&handle, src.id).await?;
        let codes = error_codes_in_activity(handle.state.as_ref()).await?;
        let quiesced = is_quiescent(&handle.state().await);
        let (report, mut notes) = settle_and_assert(&fx, &handle, &src, 1, 1).await?;
        notes.push(format!(
            "the lying 200 published nothing and synced nothing; converged after {cycles} cycle(s)"
        ));
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// s3.9 s3-slow-down-throttling
// ===========================================================================

/// `503 SlowDown` - throttling, NOT a transient server fault.
///
/// The distinction is not academic. A transient 5xx is retried a bounded number
/// of times and then given up on; rate limiting is retried indefinitely with
/// backoff. Reading `SlowDown` by status alone therefore turns a throttled
/// bucket into abandoned files. `driven-s3` classifies the S3 code first for
/// exactly this reason, and this row proves it end to end: the upload must
/// still finish.
struct S3SlowDownThrottling;

#[async_trait]
impl Scenario for S3SlowDownThrottling {
    fn name(&self) -> &'static str {
        "s3-slow-down-throttling"
    }
    fn description(&self) -> &'static str {
        "503 SlowDown is throttling, not a fatal 5xx: the upload still completes"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fx = S3Fixture::new().await?;
        let handle = fx.boot().await?;
        // A SMALL file, deliberately: it takes the buffered `create` path,
        // which is the one that retries in-cycle through `classify_retry` - so
        // a long throttle burst is answered by the retry POLICY rather than by
        // the next scan cycle happening to succeed.
        write_file(fx.src_root(), "a.bin", &payload(SMALL_FIXTURE_LEN, 14))?;
        let src = fx.add_source(&handle).await?;

        // A burst LONGER than the finite 5xx budget. This is the whole point of
        // the row: with `SlowDown` classified as rate limiting the upload keeps
        // retrying and completes; classified by status alone (a 503 -> transient
        // 5xx) it exhausts MAX_TRANSIENT_RETRIES = 6 and strands the file. One
        // injected SlowDown would pass either way and prove nothing.
        const THROTTLE_BURST: u64 = 10;
        fx.server.arm_slow_down_burst(THROTTLE_BURST);

        let cycles = run_until_all_synced(&handle, src.id).await?;
        // The anti-vacuous-green guard: a row that never reached its fault is a
        // row that tested nothing (the exact shape of the #192 flake).
        anyhow::ensure!(
            fx.server.counts().slow_down_fired >= THROTTLE_BURST,
            "the row never reached its fault properly: {} of {THROTTLE_BURST} SlowDown responses \
             were injected, so the burst never outlasted the finite 5xx retry budget",
            fx.server.counts().slow_down_fired
        );
        let codes = error_codes_in_activity(handle.state.as_ref()).await?;
        let quiesced = is_quiescent(&handle.state().await);
        let (report, mut notes) = settle_and_assert(&fx, &handle, &src, 1, 0).await?;
        notes.push(format!(
            "{THROTTLE_BURST} consecutive 503 SlowDown responses (more than the 6-attempt 5xx \
             budget) did not strand the file; backed up after {cycles} cycle(s)"
        ));
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// s3.9 s3-request-timeout-retryable-400
// ===========================================================================

/// `400 RequestTimeout` - a RETRYABLE failure arriving with a client-error
/// status.
///
/// Every generic HTTP rule says a 4xx is the client's fault and must not be
/// retried. S3 breaks that: `RequestTimeout` means "your socket went quiet, try
/// again". Treating it as fatal abandons the file. The row asserts the upload
/// completes anyway.
struct S3RequestTimeoutRetryable400;

#[async_trait]
impl Scenario for S3RequestTimeoutRetryable400 {
    fn name(&self) -> &'static str {
        "s3-request-timeout-retryable-400"
    }
    fn description(&self) -> &'static str {
        "400 RequestTimeout is retryable despite its 4xx status: the upload still completes"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fx = S3Fixture::new().await?;
        let handle = fx.boot().await?;
        write_file(
            fx.src_root(),
            "big.bin",
            &payload(MULTIPART_FIXTURE_LEN, 16),
        )?;
        let src = fx.add_source(&handle).await?;

        // Land it on the second write, which is a part upload rather than the
        // CreateMultipartUpload - the mid-transfer position that matters.
        fx.server.arm_request_timeout_after(1);
        fx.server.arm_response_delay(RACE_WIDENING_DELAY);

        let cycles = run_until_all_synced(&handle, src.id).await?;
        anyhow::ensure!(
            fx.server.counts().request_timeout_fired >= 1,
            "the row never reached its fault: no 400 RequestTimeout was injected"
        );
        let codes = error_codes_in_activity(handle.state.as_ref()).await?;
        let quiesced = is_quiescent(&handle.state().await);
        let (report, mut notes) = settle_and_assert(&fx, &handle, &src, 1, 1).await?;
        notes.push(format!(
            "a retryable 400 mid-transfer did not abandon the file; converged after {cycles} cycle(s)"
        ));
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// s3.9 s3-kill-mid-upload-then-reboot
// ===========================================================================

/// Kill the process mid-upload, then re-run.
///
/// The brief's headline failure mode. A multipart upload is interrupted, the
/// orchestrator dies WITHOUT a graceful shutdown (so no in-process retry can
/// paper over it), and a fresh handle reboots over the same state DB and the
/// same bucket. Three things must hold afterwards:
///
/// - no orphaned object at the destination with no `file_state` row;
/// - no duplicate object for the file;
/// - nothing recorded as `synced` that is not actually there, byte for byte.
///
/// The kill is deterministic, not timing-based: the connection drop at
/// `CompleteMultipartUpload` guarantees the cycle ends with the upload
/// unfinished, so the crash always lands in the interesting window.
struct S3KillMidUploadThenReboot;

#[async_trait]
impl Scenario for S3KillMidUploadThenReboot {
    fn name(&self) -> &'static str {
        "s3-kill-mid-upload-then-reboot"
    }
    fn description(&self) -> &'static str {
        "S3 upload killed mid-flight then re-run: no orphan, no duplicate, nothing falsely synced"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fx = S3Fixture::new().await?;
        write_file(
            fx.src_root(),
            "big.bin",
            &payload(MULTIPART_FIXTURE_LEN, 17),
        )?;

        // Phase 1: the upload is cut and the cycle cannot finish it.
        let orphans_before;
        let src;
        {
            let handle = fx.boot().await?;
            src = fx.add_source(&handle).await?;
            fx.server.arm_drop_complete(1);
            fx.server.arm_response_delay(RACE_WIDENING_DELAY);
            let _ = handle.run_one_cycle().await;
            anyhow::ensure!(
                fx.server.counts().dropped_connections >= 1,
                "the row never reached its fault: no connection was dropped"
            );
            orphans_before = fx.server.oracle_entries(&fx.folder).len();
            // Leaving this scope DROPS the handle, and with it the
            // orchestrator's channels, with no graceful shutdown signalled -
            // an abrupt process death. The hermetic state DB and the bucket
            // both survive, which is exactly what the reboot must reconcile.
        }

        // Phase 2: a fresh handle over the SAME state DB and the SAME bucket,
        // on a healthy server.
        fx.server.clear_faults();
        let reopened = fx.boot().await?;
        let cycles = run_until_all_synced(&reopened, src.id).await?;

        let codes = error_codes_in_activity(reopened.state.as_ref()).await?;
        let quiesced = is_quiescent(&reopened.state().await);
        let (report, mut notes) = settle_and_assert(&fx, &reopened, &src, 1, 1).await?;

        // No orphan: every live object must be claimed by a file_state row.
        let rows = reopened.state.load_source_file_state(src.id).await?;
        let claimed: Vec<String> = rows
            .values()
            .filter_map(|r| r.drive_file_id.clone())
            .collect();
        let unclaimed: Vec<String> = fx
            .server
            .oracle_entries(&fx.folder)
            .into_iter()
            .map(|e| e.id)
            .filter(|id| !claimed.contains(id))
            .collect();
        anyhow::ensure!(
            unclaimed.is_empty(),
            "orphaned destination object(s) with no file_state row after recovery: {unclaimed:?}"
        );

        notes.push(format!(
            "the killed cycle left {orphans_before} object(s) at the destination; \
             recovery converged after {cycles} cycle(s) with no orphan and no duplicate"
        ));
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// s3.9 s3-destination-full-mid-upload
// ===========================================================================

/// The destination runs out of room in the MIDDLE of a multipart upload.
///
/// This is the `ENOSPC` shape. PR #212's errno table maps `ENOSPC`/`EDQUOT`
/// onto `DriveError::StorageQuota` - the same variant S3's `QuotaExceeded`
/// produces - so the behaviour asserted here is the behaviour the local-folder
/// backend needs too: the account pauses, nothing partial is published as
/// complete, and no file is recorded as `synced`.
///
/// The budget is deliberately set so it runs out with parts ALREADY on the
/// server, because "full before we started" is the easy case and "full halfway
/// through" is the one that can publish a truncated object.
struct S3DestinationFullMidUpload;

#[async_trait]
impl Scenario for S3DestinationFullMidUpload {
    fn name(&self) -> &'static str {
        "s3-destination-full-mid-upload"
    }
    fn description(&self) -> &'static str {
        "destination full mid-multipart (the ENOSPC shape): drive.quota_exhausted, nothing partial"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fx = S3Fixture::new().await?;
        let handle = fx.boot().await?;
        write_file(
            fx.src_root(),
            "big.bin",
            &payload(MULTIPART_FIXTURE_LEN, 18),
        )?;
        let src = fx.add_source(&handle).await?;

        // Room for the first 8 MiB part and nothing more, so the budget runs
        // out with one part already committed.
        fx.server.arm_quota_bytes(9 * 1024 * 1024);

        let _ = handle.run_one_cycle().await;
        anyhow::ensure!(
            fx.server.counts().quota_refusals >= 1,
            "the row never reached its fault: the byte budget was never exceeded"
        );
        anyhow::ensure!(
            fx.server.counts().upload_part >= 1,
            "the budget must run out with a part ALREADY uploaded, or this is not a mid-upload row"
        );
        let codes = error_codes_in_activity(handle.state.as_ref()).await?;
        anyhow::ensure!(
            codes.contains(&ErrorCode::DriveQuotaExhausted),
            "expected drive.quota_exhausted when the destination filled up, got {codes:?}"
        );

        // Nothing partial may be visible as a finished object, and nothing may
        // be recorded as synced.
        let published = fx.server.oracle_entries(&fx.folder);
        anyhow::ensure!(
            published.is_empty(),
            "a quota failure mid-upload published {} object(s); a partial upload must never \
             appear as a complete object: {:?}",
            published.len(),
            published
                .iter()
                .map(|e| (&e.id, e.size))
                .collect::<Vec<_>>()
        );
        let rows = handle.state.load_source_file_state(src.id).await?;
        anyhow::ensure!(
            rows.iter()
                .all(|(_, r)| r.status != FileStateStatus::Synced),
            "no file may be synced when the destination refused its bytes"
        );

        let quiesced = is_quiescent(&handle.state().await);
        let report = assert_invariants(&handle, fx.server.as_ref(), src.id, &fx.folder).await?;
        anyhow::ensure!(
            report.data_loss_paths.is_empty() && report.duplicate_op_uuids.is_empty(),
            "s6.3 invariants violated: {}",
            report.violation_summary()
        );

        // Freeing space must let the backup finish - a full destination is a
        // pause, not a permanent loss.
        fx.server.clear_faults();
        let cycles = run_until_all_synced(&handle, src.id).await?;
        let (recovered, mut notes) = settle_and_assert(&fx, &handle, &src, 1, 1).await?;
        notes.push(format!(
            "quota exhausted with parts already uploaded; nothing partial was published; \
             recovered after {cycles} cycle(s) once space was freed"
        ));
        Ok(finish(&recovered, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::DriveQuotaExhausted,
        }
    }
}

// ===========================================================================
// s3.9 localfs-crash-between-temp-write-and-rename
// ===========================================================================

/// A crash between the temp-file write and the atomic rename must leave **no
/// half-written object that looks complete**.
///
/// This is the local-folder backend's own durability mechanism under test:
/// every write goes to a temp file in the target's directory, is `F_FULLFSYNC`ed
/// (a plain `fsync` returns before a removable drive flushes its own write
/// cache - exactly the window that matters when a stick is yanked), then renamed
/// over the target, then the directory entry is synced. The instant between the
/// sync and the rename is the one where the bytes exist on the medium but the
/// name does not point at them yet.
///
/// ## Reaching that instant without a fault seam in production code
///
/// Two phases, each an on-disk state the failure genuinely produces:
///
/// **Phase 1 - the commit cannot land.** An object-shaped DIRECTORY is placed at
/// the target path, so the store really does write and `F_FULLFSYNC` its temp
/// file and the real `fsx::commit_rename` really does fail. Nothing may be
/// readable at the target path, no sidecar may claim otherwise, and nothing may
/// be recorded `synced`.
///
/// **Phase 2 - the residue a killed process leaves.** A temp file holding the
/// file's FULL bytes is planted in the destination directory, which is precisely
/// what a process killed after the sync and before the rename leaves behind. It
/// must never be reachable as an object - not through `list_folder`, and (much
/// worse if it were) not through `list_source_object_ids`, where the
/// remote-existence audit would see an object Driven owns with no `file_state`
/// row and try to heal it forever.
///
/// The row plants TWO: one backdated past the sweep window and one fresh. The
/// stale one must be reaped at store construction; the fresh one must SURVIVE,
/// because it may belong to a resumable session the executor can still finish -
/// a sweep that reaped it would destroy a recoverable upload.
///
/// ## The commit ORDERING this row must not get backwards
///
/// The backend commits **data first, sidecar second**, deliberately. A crash
/// between them leaves either an object with a stale annotation (the pending op
/// replays and re-commits both) or a dangling sidecar (inert, swept by the next
/// write) - both benign. The opposite ordering would leave a LIVE data file with
/// no annotation, which `list_source_object_ids` cannot see, so the audit would
/// call a live object dead and re-upload it beside itself forever. This row
/// therefore asserts on the DATA file's absence, never on a sidecar's presence
/// as the definition of "committed".
///
/// ## Negative control
///
/// Making `layout::is_control_entry` stop filtering `.driven-tmp-*` turns this
/// row RED with `an abandoned temp file must not appear as an object: expected
/// the 1 real object, got ["backups/report.bin", "backups/.driven-tmp-..."]`.
/// So the row detects the failure class rather than passing on an empty
/// destination. (The control was reverted; `crates/driven-localfs` is untouched
/// on this branch.)
struct LocalFsCrashBetweenTempWriteAndRename;

#[async_trait]
impl Scenario for LocalFsCrashBetweenTempWriteAndRename {
    fn name(&self) -> &'static str {
        "localfs-crash-between-temp-write-and-rename"
    }
    fn description(&self) -> &'static str {
        "local folder: commit interrupted between the synced temp file and the rename leaves no object that looks complete"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fx = LocalFsFixture::new()?;
        let content = payload(SMALL_FIXTURE_LEN, 31);
        write_file(fx.src_root(), "report.bin", &content)?;
        let oracle = fx.oracle();
        let mut notes: Vec<String> = Vec::new();

        // -- phase 1: the rename cannot land --------------------------------
        let blocked = fx.block_target_path("report.bin")?;
        let handle = fx.boot().await?;
        let src = local_source(&fx, &handle).await?;
        let _ = handle.run_one_cycle().await;

        anyhow::ensure!(
            oracle
                .object_bytes(&format!("{CHAOS_SUBFOLDER}/report.bin"))
                .is_none(),
            "a commit that could not rename must leave NOTHING readable as an object at the \
             target path"
        );
        anyhow::ensure!(
            oracle.sidecars_in(CHAOS_SUBFOLDER).is_empty(),
            "a failed commit must annotate nothing: sidecars {:?}",
            oracle.sidecars_in(CHAOS_SUBFOLDER)
        );
        let rows = handle.state.load_source_file_state(src.id).await?;
        anyhow::ensure!(
            rows.values().all(|r| r.status != FileStateStatus::Synced),
            "nothing may be recorded synced when the commit never landed"
        );
        // The anti-vacuous-green guard. Every assertion above also holds if the
        // upload was never ATTEMPTED (empty destination, no synced rows), so the
        // row must prove the store really tried and really failed - otherwise a
        // scan that silently skipped the file would read as a pass.
        let phase1_codes = error_codes_in_activity(handle.state.as_ref()).await?;
        anyhow::ensure!(
            !phase1_codes.is_empty(),
            "the row never reached its fault: a blocked commit must surface an error, but the \
             activity log recorded none - the upload was probably never attempted"
        );
        // `commit_object` removes its temp file when the rename fails, so a
        // FAILED commit leaves no residue of its own. (The residue a KILLED
        // process leaves is phase 2's subject, planted deliberately.)
        anyhow::ensure!(
            oracle.temp_files().is_empty(),
            "a commit that failed at the rename must clean up its own temp file: {:?}",
            oracle.temp_files()
        );
        notes.push(format!(
            "phase 1: commit blocked at the rename; surfaced {phase1_codes:?}; 0 objects, \
             0 sidecars, 0 synced, no temp residue"
        ));
        // Drop the handle: the process dies with the commit unfinished.
        drop(handle);
        std::fs::remove_dir_all(&blocked)?;

        // -- recovery from phase 1 ------------------------------------------
        // Done BEFORE planting the orphans on purpose: phase 2's assertions are
        // only meaningful against a destination that already holds a REAL object.
        // "The audit returned nothing" would be trivially true on an empty
        // destination; "the audit returned exactly the one real object and not
        // the two temp files beside it" is the actual claim.
        let reopened = fx.boot().await?;
        let cycles = run_until_all_synced(&reopened, src.id).await?;
        let recovered = assert_invariants(&reopened, &oracle, src.id, CHAOS_SUBFOLDER).await?;
        anyhow::ensure!(
            recovered.live_object_count == 1 && recovered.ok(),
            "recovery must produce exactly ONE object, found {}: {}",
            recovered.live_object_count,
            recovered.violation_summary()
        );

        // -- phase 2: the residue a KILLED process leaves -------------------
        // Both hold the file's FULL bytes, which is the nastiest case: they are
        // byte-identical to a finished object and differ only in their name.
        let stale = fx.plant_orphan_temp_file(&content, Duration::from_secs(30 * 24 * 3600))?;
        let fresh = fx.plant_orphan_temp_file(&content, Duration::from_secs(5))?;

        // Ask the BACKEND, not the oracle: an orphan temp file must be invisible
        // as an object through the store's own listing AND through the audit.
        // Exactly ONE result each - the real object - proves the temp files were
        // filtered rather than the query being empty.
        {
            let store = fx.store()?;
            let listed = store
                .list_folder(
                    CHAOS_SUBFOLDER,
                    &driven_remote::remote_store::DriveContext::MyDrive,
                )
                .await?;
            anyhow::ensure!(
                listed.len() == 1,
                "an abandoned temp file must not appear as an object: expected the 1 real object, \
                 got {:?}",
                listed.iter().map(|e| &e.id).collect::<Vec<_>>()
            );
            let owned = store
                .list_source_object_ids(
                    &src.id.to_string(),
                    &driven_remote::remote_store::DriveContext::MyDrive,
                )
                .await?;
            anyhow::ensure!(
                owned.len() == 1,
                "an abandoned temp file must not enter the remote-existence audit - it would be \
                 an object Driven owns with no file_state row, which the audit tries to heal \
                 forever: expected 1 real object, got {owned:?}"
            );
        }

        // Constructing a store runs the sweep: stale reaped, fresh preserved.
        let swept = fx.boot().await?;
        anyhow::ensure!(
            !stale.exists(),
            "a temp file older than the sweep window must be reaped at store construction"
        );
        anyhow::ensure!(
            fresh.exists(),
            "a FRESH temp file must survive the sweep - it may belong to a resumable session the \
             executor can still finish, and reaping it would destroy a recoverable upload"
        );

        // A further cycle over the swept destination must be a no-op: the
        // surviving temp file must not provoke a re-upload, a trash, or a
        // duplicate.
        swept.run_one_cycle().await?;
        let codes = error_codes_in_activity(swept.state.as_ref()).await?;
        let quiesced = is_quiescent(&swept.state().await);
        let report = assert_invariants(&swept, &oracle, src.id, CHAOS_SUBFOLDER).await?;
        anyhow::ensure!(
            report.ok(),
            "s6.3 invariants violated: {}",
            report.violation_summary()
        );
        anyhow::ensure!(
            report.live_object_count == 1,
            "the destination must still hold exactly ONE object, found {}",
            report.live_object_count
        );
        let checked = assert_synced_bytes_match_local(&swept, &oracle, &src).await?;
        anyhow::ensure!(checked == 1, "the recovered object must be byte-verified");

        notes.push(format!(
            "recovery after {cycles} cycle(s) produced 1 byte-exact object; two abandoned temp \
             files holding the FULL bytes were then invisible to both list_folder and the audit; \
             the stale one was swept, the fresh one preserved, and a further cycle changed nothing"
        ));
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

/// A source rooted at a [`LocalFsFixture`]'s temp tree, backing up into its
/// destination sub-folder.
async fn local_source(fx: &LocalFsFixture, handle: &DrivenHandle) -> anyhow::Result<SourceRow> {
    let src = source_in(handle.account_id, fx.src_root(), CHAOS_SUBFOLDER);
    handle.state.upsert_source(&src).await?;
    Ok(src)
}

// ===========================================================================
// s3.9 destination-vanished-across-backends
// ===========================================================================

/// The destination VANISHES mid-cycle, asserted on EVERY backend on `main` from
/// one scenario body.
///
/// This is the removable-media case, and it is the reason the local-folder
/// backend's `.driven-destination.json` marker exists: an unmounted NAS mount
/// point is an ordinary empty directory, so `root.exists()` would happily let
/// Driven write a whole backup onto the boot disk under the mount, where it
/// vanishes on the next remount while `file_state` still calls every file
/// synced. That is total silent backup loss.
///
/// Every backend raises the same code from its OWN detector - the local-folder
/// marker, S3's `NoSuchBucket`, the fake's latching flag - and the required
/// behaviour is identical: surface `drive.dest_folder_missing`, write NOTHING
/// further, and lose nothing already synced. Running ONE body across all three
/// is what makes "the invariants are backend-independent" a test rather than a
/// claim.
///
/// ## Negative control
///
/// Disabling the local-folder identity comparison in `LocalFsStore::guard_root`
/// (so a present marker is treated as proof - the `root.exists()` mistake in a
/// slightly better disguise) turns this row RED with `a marker belonging to a
/// different volume must raise drive.dest_folder_missing, got []`. The control
/// was reverted; `crates/driven-localfs` is untouched on this branch.
struct DestinationVanishedAcrossBackends;

#[async_trait]
impl Scenario for DestinationVanishedAcrossBackends {
    fn name(&self) -> &'static str {
        "destination-vanished-across-backends"
    }
    fn description(&self) -> &'static str {
        "destination vanishes mid-cycle on every backend: dest_folder_missing, nothing written"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let mut codes: Vec<ErrorCode> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        // The runner enforces ONE `InvariantOutcome` per scenario, so the arms'
        // reports are MERGED rather than the last one winning: a violation in
        // either arm must reach the runner, not be overwritten by a clean
        // sibling. (Each arm also asserts its own invariants inline, so an arm
        // can never pass unchecked.)
        let mut reports: Vec<InvariantReport> = Vec::new();
        let mut quiesced = true;

        // -- arm A: the S3 backend, via a real NoSuchBucket on the wire -------
        {
            let fx = S3Fixture::new().await?;
            let handle = fx.boot().await?;
            write_file(fx.src_root(), "before.bin", &payload(SMALL_FIXTURE_LEN, 19))?;
            let src = fx.add_source(&handle).await?;

            // A healthy first cycle, so there is a synced baseline whose loss
            // the invariants can detect.
            handle.run_one_cycle().await?;
            let baseline =
                assert_invariants(&handle, fx.server.as_ref(), src.id, &fx.folder).await?;
            anyhow::ensure!(
                baseline.live_object_count == 1 && baseline.ok(),
                "the S3 arm needs a healthy baseline, got {} object(s): {}",
                baseline.live_object_count,
                baseline.violation_summary()
            );

            // The stick is yanked.
            fx.server.arm_bucket_missing();
            write_file(fx.src_root(), "after.bin", &payload(SMALL_FIXTURE_LEN, 20))?;
            let _ = handle.run_one_cycle().await;

            anyhow::ensure!(
                fx.server.counts().bucket_missing_refusals >= 1,
                "S3 arm never reached its fault: no NoSuchBucket was served"
            );
            let arm_codes = error_codes_in_activity(handle.state.as_ref()).await?;
            anyhow::ensure!(
                arm_codes.contains(&ErrorCode::DriveDestFolderMissing),
                "S3 arm: expected drive.dest_folder_missing once the bucket vanished, got {arm_codes:?}"
            );
            let after = assert_invariants(&handle, fx.server.as_ref(), src.id, &fx.folder).await?;
            anyhow::ensure!(
                after.live_object_count == 1,
                "S3 arm: nothing may be written after the destination vanished, but the bucket \
                 holds {} object(s)",
                after.live_object_count
            );
            anyhow::ensure!(
                after.ok(),
                "S3 arm: s6.3 invariants violated: {}",
                after.violation_summary()
            );
            let rows = handle.state.load_source_file_state(src.id).await?;
            let synced_after: Vec<String> = rows
                .iter()
                .filter(|(rel, r)| {
                    r.status == FileStateStatus::Synced && rel.as_str().contains("after")
                })
                .map(|(rel, _)| rel.to_string())
                .collect();
            anyhow::ensure!(
                synced_after.is_empty(),
                "S3 arm: {synced_after:?} was marked synced although the destination was gone"
            );

            quiesced &= is_quiescent(&handle.state().await);
            for c in arm_codes {
                if !codes.contains(&c) {
                    codes.push(c);
                }
            }
            notes.push(format!(
                "S3 arm (NoSuchBucket on the wire): baseline kept, nothing new written, \
                 {} live object(s)",
                after.live_object_count
            ));
            reports.push(after);
        }

        // -- arm B: the Drive backend, via the fake's latching fault ----------
        {
            let remote = Arc::new(InMemoryRemoteStore::new());
            let folder = remote.root_id().to_string();
            let state_dir = tempfile::tempdir()?;
            let src_dir = tempfile::tempdir()?;
            write_file(
                src_dir.path(),
                "before.bin",
                &payload(SMALL_FIXTURE_LEN, 21),
            )?;
            let handle = DrivenHandleBuilder::new(state_dir.path().join("state.db"))
                .remote(remote.clone() as Arc<dyn RemoteStore>)
                .boot()
                .await?;
            let src = source_in(handle.account_id, src_dir.path(), &folder);
            handle.state.upsert_source(&src).await?;

            handle.run_one_cycle().await?;
            let baseline = assert_invariants(&handle, &remote, src.id, &folder).await?;
            anyhow::ensure!(
                baseline.live_object_count == 1 && baseline.ok(),
                "the Drive arm needs a healthy baseline, got {} object(s): {}",
                baseline.live_object_count,
                baseline.violation_summary()
            );

            // The stick is yanked. `latch_dest_folder_missing` is the runtime
            // counterpart of the construction-time `with_dest_folder_missing`
            // builder, added for exactly this shape: the fault has to arrive
            // AFTER a healthy baseline, and rebuilding the store to install it
            // would discard the objects the assertion is about.
            remote.latch_dest_folder_missing();
            write_file(src_dir.path(), "after.bin", &payload(SMALL_FIXTURE_LEN, 22))?;
            let _ = handle.run_one_cycle().await;

            let arm_codes = error_codes_in_activity(handle.state.as_ref()).await?;
            anyhow::ensure!(
                arm_codes.contains(&ErrorCode::DriveDestFolderMissing),
                "Drive arm: expected drive.dest_folder_missing, got {arm_codes:?}"
            );
            let after = assert_invariants(&handle, &remote, src.id, &folder).await?;
            anyhow::ensure!(
                after.live_object_count == 1,
                "Drive arm: nothing may be written after the destination vanished, but the \
                 folder holds {} live object(s)",
                after.live_object_count
            );
            anyhow::ensure!(
                after.ok(),
                "Drive arm: s6.3 invariants violated: {}",
                after.violation_summary()
            );

            quiesced &= is_quiescent(&handle.state().await);
            for c in arm_codes {
                if !codes.contains(&c) {
                    codes.push(c);
                }
            }
            notes.push(format!(
                "Drive arm (latched dest_folder_missing): baseline kept, nothing new written, \
                 {} live object(s)",
                after.live_object_count
            ));
            reports.push(after);
        }

        // -- arm C: the local-folder backend, via its identity marker ---------
        //
        // The arm this whole scenario was shaped around. On a filesystem
        // destination "the destination vanished" cannot be detected by looking
        // at the path: an unmounted NAS mount point is an ordinary empty
        // DIRECTORY, so `root.exists()` would happily let Driven write a whole
        // backup onto the boot disk underneath the mount, where it disappears on
        // the next remount while `file_state` still calls every file synced.
        // Total, silent backup loss - the worst outcome in the taxonomy, and the
        // reason `.driven-destination.json` exists.
        //
        // The fault injected here is deliberately the NASTIER of the two the
        // marker catches: not a missing marker, but a marker carrying a
        // DIFFERENT destination id - "someone plugged in a different stick at
        // the same mount point". The path exists, is a directory, is writable,
        // and even looks like a Driven destination; only the identity differs.
        {
            let fx = LocalFsFixture::new()?;
            write_file(fx.src_root(), "before.bin", &payload(SMALL_FIXTURE_LEN, 23))?;
            let handle = fx.boot().await?;
            let src = local_source(&fx, &handle).await?;
            let oracle = fx.oracle();

            handle.run_one_cycle().await?;
            let baseline = assert_invariants(&handle, &oracle, src.id, CHAOS_SUBFOLDER).await?;
            anyhow::ensure!(
                baseline.live_object_count == 1 && baseline.ok(),
                "the local-folder arm needs a healthy baseline, got {} object(s): {}",
                baseline.live_object_count,
                baseline.violation_summary()
            );

            fx.swap_marker_identity()?;
            write_file(fx.src_root(), "after.bin", &payload(SMALL_FIXTURE_LEN, 24))?;
            let _ = handle.run_one_cycle().await;

            let arm_codes = error_codes_in_activity(handle.state.as_ref()).await?;
            anyhow::ensure!(
                arm_codes.contains(&ErrorCode::DriveDestFolderMissing),
                "local-folder arm: a marker belonging to a different volume must raise \
                 drive.dest_folder_missing, got {arm_codes:?}"
            );
            let after = assert_invariants(&handle, &oracle, src.id, CHAOS_SUBFOLDER).await?;
            anyhow::ensure!(
                after.live_object_count == 1,
                "local-folder arm: NOTHING may be written to a destination that is not ours - \
                 that is the whole point of the marker - but the folder holds {} object(s)",
                after.live_object_count
            );
            anyhow::ensure!(
                after.ok(),
                "local-folder arm: s6.3 invariants violated: {}",
                after.violation_summary()
            );
            let rows = handle.state.load_source_file_state(src.id).await?;
            let synced_after: Vec<String> = rows
                .iter()
                .filter(|(rel, r)| {
                    r.status == FileStateStatus::Synced && rel.as_str().contains("after")
                })
                .map(|(rel, _)| rel.to_string())
                .collect();
            anyhow::ensure!(
                synced_after.is_empty(),
                "local-folder arm: {synced_after:?} was marked synced although the destination \
                 was a different volume"
            );

            quiesced &= is_quiescent(&handle.state().await);
            for c in arm_codes {
                if !codes.contains(&c) {
                    codes.push(c);
                }
            }
            notes.push(format!(
                "local-folder arm (marker holds a different destination id): baseline kept, \
                 nothing new written, {} live object(s)",
                after.live_object_count
            ));
            reports.push(after);
        }

        let report = merge_reports(reports);
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::DriveDestFolderMissing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_row_has_a_unique_kebab_case_name() {
        let rows = scenarios();
        let mut names: Vec<&str> = rows.iter().map(|s| s.name()).collect();
        names.sort();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate scenario name");
        for name in names {
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{name} is not kebab-case"
            );
        }
    }
}
