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
//! | `sftp-transport-cut-mid-upload` | the TCP connection dies mid-transfer | a WEDGED cycle; a synced row for an interrupted transfer |
//! | `sftp-auth-flap-latches-needs-reauth` | credentials refused, then accepted | anything written while refused; a silent, unreported latch |
//! | `sftp-host-key-swapped-mid-run` | the server presents a different host key | a changed host key retried through instead of refused |
//! | `sftp-destination-full-mid-upload` | ENOSPC-shaped `SSH_FX_FAILURE` mid-transfer | a full disk read as a retryable transient; a partial object published |
//! | `sftp-truncated-listing-is-an-error` | enumeration cut after a partial batch | a short listing read as a complete one - i.e. as a mass deletion |
//! | `sftp-torn-sidecar-residue` | a metadata sidecar truncated by a crash | data loss or a duplicate from an unreadable annotation |
//! | `destination-vanished-across-backends` | destination vanishes mid-cycle on ALL FOUR backends, one body | anything written after the destination vanished |
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
//! ## The SSH (SFTP) rows
//!
//! The SFTP backend sits between the two shapes above. Its faults live BELOW
//! the trait, like S3's - a transport cut, a refused credential, a changed host
//! key, a full remote disk, an abandoned enumeration have no representation at
//! the `RemoteStore` seam at all - but its destination is a directory this
//! process can read, like the local folder's, because
//! [`driven_sftp::test_support::TestSftpServer`] serves a temp directory over a
//! real socket. So the faults go in at the layer each hazard occupies (the TCP
//! stream, the auth handler, the server's host key, the `write` and `readdir`
//! handlers) while the ground truth is read straight off disk. See
//! [`crate::sftp_fixture`].
//!
//! One of these rows found a REAL DEFECT rather than confirming a guarantee:
//! `sftp-transport-cut-mid-upload` did not fail, it HUNG, because
//! `russh-sftp` 2.3 never resolves a pending write acknowledgement when its
//! stream dies. That wedged a sync cycle instead of failing it - an s6.3 "no
//! infinite loop" violation in production code - and is now fixed by
//! `driven_sftp::store::while_connected`, with that row as its regression
//! guard. The row runs the faulted cycle under an explicit timeout, because a
//! reintroduced hang must fail loudly rather than look like CI flake.
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
use crate::sftp_fixture::{SftpFixture, SftpOracle, SFTP_SUBFOLDER};

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
        Box::new(SftpTransportCutMidUpload),
        Box::new(SftpAuthFlapThenRecovers),
        Box::new(SftpHostKeySwappedMidRun),
        Box::new(SftpDestinationFullMidUpload),
        Box::new(SftpTruncatedListingIsAnError),
        Box::new(SftpTornSidecarResidue),
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
pub(crate) fn source_in(account: AccountId, root: &std::path::Path, folder_id: &str) -> SourceRow {
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

impl DestinationBytes for SftpOracle {
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
/// `max_stranded_uploads` caps the multipart uploads that may still be in flight
/// at the end. It is a TIGHT bound rather than zero, and the reason is
/// structural - see [`STRANDED_UPLOAD_BOUND`].
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

    // Abandoned multipart uploads. Issue #222 removed every unbounded source of
    // these; what remains is bounded by the injected faults - see
    // [`STRANDED_UPLOAD_BOUND`] for exactly which case survives a single
    // harness run and why. Asserting zero would make the gate red over that
    // structural case; asserting nothing would let a regression grow unnoticed.
    let stranded = fx.server.in_flight_upload_keys();
    anyhow::ensure!(
        stranded.len() <= max_stranded_uploads,
        "{} multipart upload(s) left in flight, more than the {max_stranded_uploads} this row \
         accounts for: {stranded:?}. {STRANDED_UPLOAD_BOUND}",
        stranded.len()
    );

    let counts = fx.server.counts();
    let notes = vec![format!(
        "{checked} synced row(s) byte-verified; requests: {} total, {} parts, {} completes, \
         {} aborts, {} dropped connections",
        counts.total,
        counts.upload_part,
        counts.complete_multipart,
        counts.abort_multipart,
        counts.dropped_connections
    )];
    Ok((report, notes))
}

/// Why a row may still end with ONE abandoned multipart upload, after issue #222
/// removed every unbounded source of them.
///
/// The fix has three parts, and two of them are absolute. `S3Store` implements
/// `RemoteStore::abandon_resumable_session` as a real `AbortMultipartUpload`,
/// and the executor calls it wherever a session becomes permanently unreachable:
/// a fresh session replacing a persisted one, a discarded invalid session, a
/// streaming upload whose op is about to be DELETED, and every path by which
/// reconcile's `resume_persisted` declines to resume (a changed file, an expired
/// session, an encrypted source, a misbehaving server - all funnelled through
/// one wrapper so a new decline condition cannot forget). `S3Store` also sweeps
/// with `ListMultipartUploads` for anything an earlier build or a hard crash
/// left behind.
///
/// What it deliberately does NOT do is abort a session on a `Fatal` mid-stream
/// error, because that outcome KEEPS the op precisely so the session can be
/// RESUMED - `resume_persisted` re-attaches to that upload id and finishes it
/// instead of re-sending everything already on the wire. That is the behaviour
/// `crash_mid_upload_resumes_persisted_session_byte_for_byte` pins.
///
/// The residual case is the interaction between those two facts: reconcile is a
/// per-process STARTUP pass (`orchestrator.rs::reconcile_once` skips sources it
/// has already done), while a scan later in the SAME run will re-plan the failed
/// path and upload it under a new op. So an upload kept for a resume that the
/// next restart would have performed can be overtaken mid-run, and its parts sit
/// until that restart - where reconcile now either resumes them (no waste at
/// all) or abandons them (aborted). Bounded by restarts, not unbounded in time,
/// and backstopped by the sweep.
const STRANDED_UPLOAD_BOUND: &str = "issue #222 bounded this: the executor aborts a resumable \
     session wherever one becomes unreachable, and S3Store sweeps what a crash left behind. The \
     one upload that can outlive a single run is one kept ON PURPOSE for a crash-resume (a Fatal \
     mid-stream error) that a later scan overtook - the next start resumes or aborts it. More \
     than this row's fault count means one of the abort hooks stopped firing";

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
        let (report, mut notes) = settle_and_assert(&fx, &handle, &src, 1, 1).await?;
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
// s3.9 the SSH (SFTP) rows
// ===========================================================================
//
// Payload size for the SFTP rows that need a transfer long enough to be cut or
// to run out of room part way through. Deliberately BELOW the executor's 5 MiB
// `RESUMABLE_THRESHOLD`: these rows are about the transport and the remote
// filesystem, not about the resumable ladder (which the S3 rows already drive),
// and a debug-build run of every row pays for every byte.
const SFTP_TRANSFER_LEN: usize = 1024 * 1024;
const _: () = assert!((SFTP_TRANSFER_LEN as u64) < driven_core::executor::RESUMABLE_THRESHOLD);

/// The shared tail of an SFTP row: run the s6.3 sweep, prove the synced bytes
/// match the local file, require exactly `expect_objects` live objects, and
/// prove no abandoned temp file is reachable as one.
async fn sftp_settle_and_assert(
    fx: &SftpFixture,
    handle: &DrivenHandle,
    source: &SourceRow,
    expect_objects: u64,
) -> anyhow::Result<(InvariantReport, Vec<String>)> {
    let oracle = fx.oracle();
    let report = assert_invariants(handle, &oracle, source.id, SFTP_SUBFOLDER).await?;
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
    let checked = assert_synced_bytes_match_local(handle, &oracle, source).await?;

    // An interrupted transfer leaves a temp file holding real bytes. It may
    // legitimately still be there (the sweep spares anything younger than its
    // window, because it could belong to a session the executor can resume),
    // but it must never be reachable as an OBJECT - not through the oracle and
    // not through the store's own listing, where the remote-existence audit
    // would see something Driven owns with no `file_state` row and try to heal
    // it forever.
    let residue = oracle.temp_files();
    let store = fx.store()?;
    let listed = store
        .list_folder(
            SFTP_SUBFOLDER,
            &driven_remote::remote_store::DriveContext::MyDrive,
        )
        .await?;
    anyhow::ensure!(
        listed.len() as u64 == expect_objects,
        "the backend's own listing must agree with the oracle: expected {expect_objects}, got {:?}",
        listed.iter().map(|e| &e.id).collect::<Vec<_>>()
    );

    let counts = fx.server().fault_counts();
    let mut notes = vec![format!(
        "{checked} synced row(s) byte-verified; faults fired: {} cut(s), {} auth rejection(s), \
         {} host-key swap(s), {} ENOSPC refusal(s), {} truncated listing(s)",
        counts.disconnects,
        counts.auth_rejections,
        counts.host_key_swaps,
        counts.enospc_refusals,
        counts.truncated_readdirs
    )];
    if !residue.is_empty() {
        notes.push(format!(
            "{} abandoned upload temp file(s) remain on the server and are invisible as objects, \
             as designed: {:?}",
            residue.len(),
            residue
                .iter()
                .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
                .collect::<Vec<_>>()
        ));
    }
    Ok((report, notes))
}

// ---------------------------------------------------------------------------
// sftp-transport-cut-mid-upload
// ---------------------------------------------------------------------------

/// The TCP connection dies in the middle of an upload.
///
/// The single most likely real SFTP failure: a NAS goes to sleep, wifi drops, a
/// NAT table entry expires. Driven must recover to exactly ONE object whose
/// bytes equal the local file, with nothing recorded synced in between.
///
/// ## The bug this row found
///
/// The first version of this row did not fail - it HUNG, past a 420 second
/// wall clock, and it was the harness rather than the fixture that was right.
/// `russh-sftp` 2.3 parks each write's acknowledgement on a `oneshot` with no
/// timeout, and when the stream dies the pending senders are never dropped
/// (the request map is co-owned by the session), so `File::shutdown` waits
/// forever. `SftpStore::write_temp` therefore wedged an entire sync cycle
/// instead of failing it - the one outcome the SPEC s24 taxonomy has no room
/// for, and a direct s6.3 "no infinite loop" violation.
///
/// The fix is `driven_sftp::store::while_connected`, which bounds the write
/// path on SESSION LIVENESS rather than on a clock. This row is its regression
/// guard: without it, the row hangs rather than fails, and a hang in CI reads
/// as infrastructure flake instead of a defect - so the faulted cycle is run
/// under an explicit `timeout` and "it took too long" is asserted as a
/// failure in its own right.
///
/// The cut is single-shot, so what is measured afterwards is RECOVERY from a
/// blip rather than behaviour during a permanent outage.
struct SftpTransportCutMidUpload;

#[async_trait]
impl Scenario for SftpTransportCutMidUpload {
    fn name(&self) -> &'static str {
        "sftp-transport-cut-mid-upload"
    }
    fn description(&self) -> &'static str {
        "SFTP transport cut mid-transfer: fails BOUNDED (never hangs) and recovers byte-exact"
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
        let fx = SftpFixture::new().await?;
        let handle = fx.boot().await?;
        let src = fx.add_source(&handle).await?;

        // A healthy first cycle, so the session is already established and the
        // byte budget below is spent on PAYLOAD rather than on a handshake.
        write_file(fx.src_root(), "before.bin", &payload(SMALL_FIXTURE_LEN, 41))?;
        handle.run_one_cycle().await?;
        let baseline = assert_invariants(&handle, &fx.oracle(), src.id, SFTP_SUBFOLDER).await?;
        anyhow::ensure!(
            baseline.live_object_count == 1 && baseline.ok(),
            "the row needs a healthy baseline, got {} object(s): {}",
            baseline.live_object_count,
            baseline.violation_summary()
        );

        let content = payload(SFTP_TRANSFER_LEN, 42);
        write_file(fx.src_root(), "big.bin", &content)?;
        // Comfortably above a handshake and comfortably below the payload, so
        // the cut lands mid-transfer on every run rather than by luck.
        fx.server().arm_disconnect_after_bytes(256 * 1024);

        // The bounded-failure guard. See the type docs: an unbounded write path
        // makes this cycle never return, and a hang must fail the row loudly
        // rather than time the whole harness out.
        let cut_cycle =
            tokio::time::timeout(Duration::from_secs(120), handle.run_one_cycle()).await;
        anyhow::ensure!(
            cut_cycle.is_ok(),
            "a cut connection must FAIL the cycle, never wedge it - this is the regression guard \
             for the unbounded russh-sftp write-ack wait (driven_sftp::store::while_connected)"
        );
        anyhow::ensure!(
            fx.server().fault_counts().disconnects == 1,
            "the row never reached its fault: {} connection(s) were cut",
            fx.server().fault_counts().disconnects
        );

        // Mid-scenario: whatever the executor did with the failure, it must not
        // have recorded the interrupted file as synced.
        let rows = handle.state.load_source_file_state(src.id).await?;
        let falsely_synced: Vec<String> = rows
            .iter()
            .filter(|(rel, r)| r.status == FileStateStatus::Synced && rel.as_str().contains("big"))
            .map(|(rel, _)| rel.to_string())
            .collect();
        anyhow::ensure!(
            falsely_synced.is_empty(),
            "a cut transfer must not be recorded synced: {falsely_synced:?}"
        );

        let cycles = run_until_all_synced(&handle, src.id).await?;
        let codes = error_codes_in_activity(handle.state.as_ref()).await?;

        // A dropped pipe is a NETWORK fault, and the difference is the whole
        // reason recovery is possible. `sftp-auth-flap-latches-needs-reauth`
        // proves the neighbouring case: one AuthInvalidGrant parks the account
        // and `account_is_runnable` then skips every later cycle in silence. If
        // a cut connection were ever classified that way - a reconnect that
        // lost its pin would do it - `run_until_all_synced` would spin through
        // its whole budget doing NOTHING and the row would report a confusing
        // object count instead of the actual cause. Assert the cause.
        anyhow::ensure!(
            !codes.contains(&ErrorCode::AuthInvalidGrant),
            "a cut connection must classify as a retryable network fault; auth.invalid_grant here \
             would park the account and make every recovery cycle a silent no-op: {codes:?}"
        );
        let account_state = handle
            .state
            .list_accounts()
            .await?
            .into_iter()
            .find(|a| a.id == handle.account_id)
            .map(|a| a.state)
            .ok_or_else(|| anyhow::anyhow!("the handle has no account row"))?;
        anyhow::ensure!(
            account_state == driven_core::types::AccountState::Ok,
            "a transport blip must leave the account usable, got {account_state:?}"
        );

        let quiesced = is_quiescent(&handle.state().await);
        let (report, mut notes) = sftp_settle_and_assert(&fx, &handle, &src, 2).await?;
        notes.push(format!(
            "the transport was cut mid-transfer, the cycle FAILED rather than hanging, the account \
             stayed usable, and the upload converged after {cycles} further cycle(s)"
        ));
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        // The assertions above ARE the check: the executor may absorb a
        // single-shot transport failure inside its own retry ladder without
        // recording an Error-level row, so pinning a code here would assert the
        // ladder's current shape rather than the invariant that matters.
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ---------------------------------------------------------------------------
// sftp-auth-flap-then-recovers
// ---------------------------------------------------------------------------

/// The server is up but refusing credentials, and then settles - and the
/// account does NOT come back on its own.
///
/// ## The finding this row records
///
/// A NAS whose PAM stack or directory service is still starting rejects a
/// perfectly good password, and on the wire that is indistinguishable from a
/// wrong one. Driven treats every refusal as permanent: one
/// `AuthInvalidGrant` outcome moves the account to `NeedsReauth` and SUSPENDS
/// the orchestrator (`orchestrator.rs::handle_auth_failure` -> V-F, DESIGN
/// s5.4), after which `account_is_runnable` skips every cycle and issues zero
/// remote calls until a human reconnects.
///
/// For Google Drive that is plainly correct - `invalid_grant` from a token
/// endpoint really is permanent. **For SFTP it is a sharper trade than it looks,
/// and this row is where it is written down:** a home NAS refusing SSH auth for
/// twenty seconds while it boots is common and self-healing, yet it parks the
/// account in an attention state that only the user can clear. The row was
/// originally written expecting recovery and did not get it; rather than
/// weaken the assertion, it now asserts the behaviour that actually exists.
///
/// The row is deliberately built so it cannot pass vacuously: after the flap it
/// CLEARS every fault, runs more cycles, proves the destination is still empty,
/// and then proves the server itself is perfectly healthy by uploading through
/// a fresh store. So "nothing was backed up" is demonstrably the account gate
/// latching, not the server still refusing.
///
/// The flap is longer than one attempt on purpose: a single rejection could be
/// papered over by an in-cycle retry, and the row would prove nothing about the
/// classification (the shape of the #192 flake).
struct SftpAuthFlapThenRecovers;

#[async_trait]
impl Scenario for SftpAuthFlapThenRecovers {
    fn name(&self) -> &'static str {
        "sftp-auth-flap-latches-needs-reauth"
    }
    fn description(&self) -> &'static str {
        "SFTP credentials refused: auth.invalid_grant latches needs-reauth and does NOT self-heal"
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
        let fx = SftpFixture::new().await?;
        let handle = fx.boot().await?;
        let src = fx.add_source(&handle).await?;
        write_file(fx.src_root(), "a.bin", &payload(SMALL_FIXTURE_LEN, 43))?;

        // Armed BEFORE the first cycle, so the flap lands on the initial
        // connect: the store holds one session for its lifetime, so a flap
        // armed after a healthy cycle would never be seen at all.
        const FLAP: u64 = 6;
        fx.server().arm_auth_failures(FLAP);

        let _ = handle.run_one_cycle().await;
        anyhow::ensure!(
            fx.server().fault_counts().auth_rejections >= 1,
            "the row never reached its fault: no authentication attempt was rejected"
        );
        let flapping_codes = error_codes_in_activity(handle.state.as_ref()).await?;
        anyhow::ensure!(
            flapping_codes.contains(&ErrorCode::AuthInvalidGrant),
            "expected auth.invalid_grant while the credential was refused, got {flapping_codes:?}"
        );
        let published = fx.oracle().entries(SFTP_SUBFOLDER);
        anyhow::ensure!(
            published.is_empty(),
            "a refused credential must publish NOTHING, found {:?}",
            published.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
        let rows = handle.state.load_source_file_state(src.id).await?;
        anyhow::ensure!(
            rows.values().all(|r| r.status != FileStateStatus::Synced),
            "nothing may be synced while the server is refusing the credential"
        );

        // The account is parked, and the parking is what the row is about.
        let account_state = handle
            .state
            .list_accounts()
            .await?
            .into_iter()
            .find(|a| a.id == handle.account_id)
            .map(|a| a.state)
            .ok_or_else(|| anyhow::anyhow!("the handle has no account row"))?;
        anyhow::ensure!(
            account_state == driven_core::types::AccountState::NeedsReauth,
            "a refused credential must park the account for a human, got {account_state:?}"
        );

        // The box settles - and the account still does not come back.
        fx.server().clear_faults();
        for _ in 0..MAX_RECOVERY_CYCLES {
            let _ = handle.run_one_cycle().await;
        }
        let still_empty = fx.oracle().entries(SFTP_SUBFOLDER);
        anyhow::ensure!(
            still_empty.is_empty(),
            "the account was suspended, so these cycles must have issued NO remote calls at all, \
             but {} object(s) appeared: {:?}",
            still_empty.len(),
            still_empty.iter().map(|e| &e.id).collect::<Vec<_>>()
        );

        // The anti-vacuous-green guard, and the whole reason the row is
        // trustworthy: prove the SERVER is fine, so the empty destination above
        // is the account gate latching rather than a fault still armed.
        {
            let store = fx.store()?;
            store
                .create(
                    SFTP_SUBFOLDER,
                    "proof-the-server-is-healthy.bin",
                    "application/octet-stream",
                    driven_remote::remote_store::UploadBody::Bytes(bytes::Bytes::from_static(
                        b"the credential works again",
                    )),
                    Default::default(),
                )
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "the server must accept the same credential again, or this row proves \
                         nothing about the ACCOUNT being what stopped: {e:#}"
                    )
                })?;
        }

        let codes = error_codes_in_activity(handle.state.as_ref()).await?;
        let quiesced = is_quiescent(&handle.state().await);
        // Swept against the SOURCE, which still has no synced row: the object
        // written just above belongs to the harness, not to the source.
        let report = assert_invariants(&handle, &fx.oracle(), src.id, SFTP_SUBFOLDER).await?;
        anyhow::ensure!(
            report.data_loss_paths.is_empty() && report.duplicate_op_uuids.is_empty(),
            "s6.3 invariants violated: {}",
            report.violation_summary()
        );
        let notes = vec![
            format!(
                "{} authentication attempt(s) were refused and surfaced as auth.invalid_grant; \
                 nothing was written and no file was synced",
                fx.server().fault_counts().auth_rejections
            ),
            "FINDING (documented behaviour, sharper on SFTP than on Drive): the refusal LATCHED. \
             The account moved to needs_reauth and the orchestrator suspended, so every later \
             cycle issued zero remote calls even after the server started accepting the same \
             credential again - proven here by uploading through a fresh store against the same \
             box. Correct for an OAuth invalid_grant, which really is permanent; a NAS that \
             refuses SSH auth for a few seconds while it boots is not, and it costs the user a \
             manual reconnect."
                .to_string(),
        ];
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::AuthInvalidGrant,
        }
    }
}

// ---------------------------------------------------------------------------
// sftp-host-key-swapped-mid-run
// ---------------------------------------------------------------------------

/// The server presents a DIFFERENT host key than the one the account pinned.
///
/// Either the box was rebuilt or somebody is standing in the middle, and
/// nothing on the wire can tell those apart. Driven pins on first use and
/// hard-fails on any change: the connection is refused INSIDE the key check,
/// before any credential is sent, and it keeps being refused - a swapped host
/// key is not a transient to retry through.
///
/// ## Why the run has to reboot
///
/// A pin is verified per CONNECTION, and the store correctly keeps using the
/// session it already authenticated. So a swap armed against a live session
/// changes nothing until that session ends - and a row that armed it and simply
/// ran another cycle would pass vacuously, proving only that a healthy session
/// stays healthy. Dropping the handle and booting a fresh one over the same
/// state DB and the same server is the honest way to reach the next connection,
/// and it is also the real-world shape: the app restarts, or the account
/// reconnects, and THAT is when the changed key is discovered.
struct SftpHostKeySwappedMidRun;

#[async_trait]
impl Scenario for SftpHostKeySwappedMidRun {
    fn name(&self) -> &'static str {
        "sftp-host-key-swapped-mid-run"
    }
    fn description(&self) -> &'static str {
        "SFTP host key changes after pinning: auth.invalid_grant, nothing written, baseline kept"
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
        let fx = SftpFixture::new().await?;
        let src;
        {
            let handle = fx.boot().await?;
            src = fx.add_source(&handle).await?;
            write_file(fx.src_root(), "before.bin", &payload(SMALL_FIXTURE_LEN, 44))?;
            handle.run_one_cycle().await?;
            let baseline = assert_invariants(&handle, &fx.oracle(), src.id, SFTP_SUBFOLDER).await?;
            anyhow::ensure!(
                baseline.live_object_count == 1 && baseline.ok(),
                "the row needs a healthy baseline, got {} object(s): {}",
                baseline.live_object_count,
                baseline.violation_summary()
            );
        }

        // The key changes, and the app comes back to a server it no longer
        // recognizes.
        fx.server().arm_host_key_swap();
        write_file(fx.src_root(), "after.bin", &payload(SMALL_FIXTURE_LEN, 45))?;
        let reopened = fx.boot().await?;
        for _ in 0..2 {
            let _ = reopened.run_one_cycle().await;
        }

        anyhow::ensure!(
            fx.server().fault_counts().host_key_swaps >= 1,
            "the row never reached its fault: no connection was served the alternate host key"
        );
        let codes = error_codes_in_activity(reopened.state.as_ref()).await?;
        anyhow::ensure!(
            codes.contains(&ErrorCode::AuthInvalidGrant),
            "a changed host key must surface as auth.invalid_grant - the account needs a human, \
             not a retry - got {codes:?}"
        );
        // The ATTENTION STATE, not just the log line. An activity row is
        // something a user might scroll past; `NeedsReauth` is what actually
        // stops the account and raises the reauth prompt, and it is the half
        // that makes "blocks sync" true rather than "complains about sync".
        let account_state = reopened
            .state
            .list_accounts()
            .await?
            .into_iter()
            .find(|a| a.id == reopened.account_id)
            .map(|a| a.state)
            .ok_or_else(|| anyhow::anyhow!("the rebooted handle has no account row"))?;
        anyhow::ensure!(
            account_state == driven_core::types::AccountState::NeedsReauth,
            "a server that failed the host-key pin must park the account for a human - a possible \
             MITM is not something to retry through - got {account_state:?}"
        );

        let oracle = fx.oracle();
        let after = assert_invariants(&reopened, &oracle, src.id, SFTP_SUBFOLDER).await?;
        anyhow::ensure!(
            after.live_object_count == 1,
            "nothing may be written to a server that failed the pin check, but the destination \
             holds {} object(s)",
            after.live_object_count
        );
        anyhow::ensure!(
            after.ok(),
            "s6.3 invariants violated: {}",
            after.violation_summary()
        );
        let rows = reopened.state.load_source_file_state(src.id).await?;
        let synced_after: Vec<String> = rows
            .iter()
            .filter(|(rel, r)| {
                r.status == FileStateStatus::Synced && rel.as_str().contains("after")
            })
            .map(|(rel, _)| rel.to_string())
            .collect();
        anyhow::ensure!(
            synced_after.is_empty(),
            "{synced_after:?} was marked synced although the server failed the host-key check"
        );

        let quiesced = is_quiescent(&reopened.state().await);
        let notes = vec![format!(
            "the server presented a different host key on {} connection(s); every one was refused \
             before a credential was sent, the pre-swap object survived untouched, and nothing new \
             was written",
            fx.server().fault_counts().host_key_swaps
        )];
        Ok(finish(&after, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::AuthInvalidGrant,
        }
    }
}

// ---------------------------------------------------------------------------
// sftp-destination-full-mid-upload
// ---------------------------------------------------------------------------

/// The remote filesystem runs out of room in the MIDDLE of an upload.
///
/// SFTPv3 has no ENOSPC status code: every server folds a full disk into
/// `SSH_FX_FAILURE` and leaves the human-readable MESSAGE as the only signal.
/// Read by status alone that is a generic transient, so Driven would retry a
/// full NAS forever instead of pausing the account and telling the user. This
/// row drives the message-based classification end to end.
///
/// The budget runs out with bytes ALREADY on the server, because "full before
/// we started" is the easy case and "full halfway through" is the one that can
/// publish a truncated object.
struct SftpDestinationFullMidUpload;

#[async_trait]
impl Scenario for SftpDestinationFullMidUpload {
    fn name(&self) -> &'static str {
        "sftp-destination-full-mid-upload"
    }
    fn description(&self) -> &'static str {
        "remote disk full mid-transfer: drive.quota_exhausted, nothing partial, recovers when freed"
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
        let fx = SftpFixture::new().await?;
        let handle = fx.boot().await?;
        let src = fx.add_source(&handle).await?;
        write_file(fx.src_root(), "big.bin", &payload(SFTP_TRANSFER_LEN, 46))?;

        // Room for ONE write packet of a 1 MiB transfer and nothing more.
        //
        // The budget has to clear a single packet or the row silently degrades
        // into the easy "full before we started" case: `russh-sftp` writes in
        // packets just under its 256 KiB `max_packet_len`, and a budget below
        // that refuses the very FIRST write, leaving nothing on the server. The
        // `bytes_accepted_before_enospc` guard below is what caught exactly
        // that - this row previously armed 128 KiB and never once filled up
        // mid-transfer.
        fx.server().arm_enospc_after_bytes(384 * 1024);
        let _ = handle.run_one_cycle().await;

        anyhow::ensure!(
            fx.server().fault_counts().enospc_refusals >= 1,
            "the row never reached its fault: no write was refused for space"
        );
        // The budget must have run out with bytes ALREADY on the server, or
        // this is the easy "full before we started" case rather than the one
        // that can publish a truncated object. Mirrors the S3 sibling's
        // `upload_part >= 1` guard.
        anyhow::ensure!(
            fx.server().fault_counts().bytes_accepted_before_enospc > 0,
            "the disk must fill up MID-transfer, with bytes already written - a destination that \
             refused the very first byte tests a different, easier failure"
        );
        let codes = error_codes_in_activity(handle.state.as_ref()).await?;
        anyhow::ensure!(
            codes.contains(&ErrorCode::DriveQuotaExhausted),
            "an ENOSPC-shaped SSH_FX_FAILURE must classify as drive.quota_exhausted rather than a \
             retryable transient, got {codes:?}"
        );

        let published = fx.oracle().entries(SFTP_SUBFOLDER);
        anyhow::ensure!(
            published.is_empty(),
            "a full destination published {} object(s); a partial upload must never appear as a \
             complete object: {:?}",
            published.len(),
            published
                .iter()
                .map(|e| (&e.id, e.size))
                .collect::<Vec<_>>()
        );
        let rows = handle.state.load_source_file_state(src.id).await?;
        anyhow::ensure!(
            rows.values().all(|r| r.status != FileStateStatus::Synced),
            "no file may be synced when the destination refused its bytes"
        );
        let quiesced = is_quiescent(&handle.state().await);

        // Freeing space must let the backup finish: a full destination is a
        // pause, not a permanent loss.
        fx.server().clear_faults();
        let cycles = run_until_all_synced(&handle, src.id).await?;
        let (report, mut notes) = sftp_settle_and_assert(&fx, &handle, &src, 1).await?;
        notes.push(format!(
            "the disk filled up with bytes already written; nothing partial was published, and the \
             upload completed after {cycles} cycle(s) once space was freed"
        ));
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::DriveQuotaExhausted,
        }
    }
}

// ---------------------------------------------------------------------------
// sftp-truncated-listing-is-an-error
// ---------------------------------------------------------------------------

/// A directory enumeration the server abandons half way must fail the LISTING,
/// never come back as a short one.
///
/// This is the completeness invariant at its sharpest.
/// `list_source_object_ids` feeds a `dead = recorded - live` computation, so a
/// truncated listing accepted as complete reads as a MASS DELETION and the
/// caller "heals" it by re-uploading the entire source - or, worse, by trashing
/// what it believes is gone. Over a network the window for a short answer is
/// wide: a server under load, an interrupted enumeration, a directory handle
/// that expired.
///
/// The fault serves a genuinely PARTIAL first batch and then fails the next
/// `readdir`, which is the only truncation any client can detect: a server that
/// quietly returns fewer entries and then a clean EOF is indistinguishable from
/// a smaller directory, in this protocol or any other. That gap is recorded on
/// `TestSftpServer::arm_truncated_readdir` rather than papered over with an
/// assertion that could not fail.
///
/// The row asserts two things, and the second is the one that matters: the call
/// errors, AND a full cycle over the truncated destination deletes nothing and
/// unsyncs nothing.
struct SftpTruncatedListingIsAnError;

#[async_trait]
impl Scenario for SftpTruncatedListingIsAnError {
    fn name(&self) -> &'static str {
        "sftp-truncated-listing-is-an-error"
    }
    fn description(&self) -> &'static str {
        "SFTP enumeration cut short fails the listing and never reads as a mass deletion"
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
        let fx = SftpFixture::new().await?;
        let handle = fx.boot().await?;
        let src = fx.add_source(&handle).await?;
        for (i, name) in ["a.bin", "b.bin", "c.bin"].iter().enumerate() {
            write_file(
                fx.src_root(),
                name,
                &payload(SMALL_FIXTURE_LEN, 50 + i as u64),
            )?;
        }
        handle.run_one_cycle().await?;
        let baseline = assert_invariants(&handle, &fx.oracle(), src.id, SFTP_SUBFOLDER).await?;
        anyhow::ensure!(
            baseline.live_object_count == 3 && baseline.ok(),
            "the row needs three healthy objects, got {}: {}",
            baseline.live_object_count,
            baseline.violation_summary()
        );

        let store = fx.store()?;
        let context = driven_remote::remote_store::DriveContext::MyDrive;
        let whole = store
            .list_source_object_ids(&src.id.to_string(), &context)
            .await?;
        anyhow::ensure!(
            whole.len() == 3,
            "the healthy audit must see all three objects, got {whole:?}"
        );

        // Two names per batch, against directories holding far more than two
        // entries - so the batch really is partial before the failure.
        fx.server().arm_truncated_readdir(2);
        let error = store
            .list_source_object_ids(&src.id.to_string(), &context)
            .await
            .err()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "a cut enumeration was reported as a COMPLETE listing - this is the mass \
                     deletion signal the whole row exists to prevent"
                )
            })?;
        anyhow::ensure!(
            fx.server().fault_counts().truncated_readdirs >= 1,
            "the row never reached its fault: no enumeration was truncated"
        );

        // The consequence that matters, one level up: a CYCLE over a
        // destination whose listing cannot be trusted must change nothing.
        //
        // Reaching the audit takes a REBOOT, and the reason is worth stating -
        // an earlier version of this row ran a cycle here and proved nothing.
        // `audit_remote_existence_if_due` is gated on a per-source `audited`
        // latch held in the orchestrator's memory, so the healthy first cycle
        // already consumed this source's one non-deep-verify audit and every
        // later cycle on the SAME orchestrator returns before issuing a single
        // remote call. (The deep-verify path is no help either: the source's
        // interval is 7 days against a `FakeClock` that never advances.) A
        // fresh handle over the same DB and the same server starts with an
        // empty latch, which is also the real-world shape - the app restarts
        // and re-audits.
        drop(handle);
        let rebooted = fx.boot().await?;
        let before_cycle = fx.server().fault_counts().truncated_readdirs;
        let _ = rebooted.run_one_cycle().await;
        let audited_in_cycle = fx.server().fault_counts().truncated_readdirs - before_cycle;

        // The anti-vacuous-green guard this row previously lacked: if the cycle
        // never enumerated, everything below is trivially true and the row is
        // measuring nothing.
        anyhow::ensure!(
            audited_in_cycle >= 1,
            "the cycle never reached the audit, so it cannot show what a failed listing does to \
             one: the truncation counter did not move across it"
        );

        let during = fx.oracle().entries(SFTP_SUBFOLDER);
        anyhow::ensure!(
            during.len() == 3,
            "a failed listing must not delete anything: {} object(s) remain",
            during.len()
        );
        let rows = rebooted.state.load_source_file_state(src.id).await?;
        anyhow::ensure!(
            rows.len() == 3 && rows.values().all(|r| r.status == FileStateStatus::Synced),
            "a failed listing must not unsync anything: {:?}",
            rows.iter()
                .map(|(rel, r)| (rel.to_string(), r.status))
                .collect::<Vec<_>>()
        );

        // Once the server behaves, the audit is whole again.
        fx.server().clear_faults();
        let recovered = store
            .list_source_object_ids(&src.id.to_string(), &context)
            .await?;
        anyhow::ensure!(
            recovered == whole,
            "the recovered audit must match the baseline exactly: {recovered:?} vs {whole:?}"
        );

        let codes = error_codes_in_activity(rebooted.state.as_ref()).await?;
        let quiesced = is_quiescent(&rebooted.state().await);
        let (report, mut notes) = sftp_settle_and_assert(&fx, &rebooted, &src, 3).await?;
        notes.push(format!(
            "the enumeration was cut after a partial batch and surfaced as an error ({}); a \
             rebooted orchestrator's remote-existence audit then hit the same truncation \
             {audited_in_cycle} time(s) in one cycle and deleted nothing, unsynced nothing; the \
             audit recovered to the same three ids once the server behaved",
            first_line(&format!("{error:#}"))
        ));
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

/// The first line of an error chain, for a note that has to stay one line.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

// ---------------------------------------------------------------------------
// sftp-torn-sidecar-residue
// ---------------------------------------------------------------------------

/// A metadata sidecar torn in half by a crash, and what it costs.
///
/// ## The ruling this row records
///
/// `driven_sftp::meta::parse` maps an unparseable sidecar to `None` - "this
/// object is not annotated" - rather than to an error. So does
/// `driven_localfs::meta::read_sidecar`, for the same stated reason: the
/// sidecar is Driven's OWN annotation, and failing the whole listing on one
/// truncated file would let a single bad byte wedge every audit against the
/// server, permanently, for every source. This row exists to decide whether
/// that leniency is acceptable on a network destination or has to be made
/// fail-closed, and the answer is **accepted, with a bounded residue** - which
/// is a claim, so the row measures it rather than asserting it.
///
/// What the leniency costs is visible here: an object whose annotation is
/// unreadable drops out of `list_source_object_ids`, so the remote-existence
/// audit no longer counts it as live. The object itself is untouched and its
/// bytes still match, and a later upload of the same name lands on exactly the
/// same path (an overwrite, never a duplicate), so this is COST, not data loss.
///
/// What bounds it is `guard_root`. The dangerous version of this - a torn
/// sidecar at a destination that is not really ours, where "unannotated" makes
/// the store adopt and overwrite a stranger's file - cannot happen while the
/// account carries a `destination_id`, because every mutating operation proves
/// the marker's identity first. It remains reachable only for an account
/// written before `destination_id` existed, where `guard_root` can check the
/// marker for PRESENCE alone; those accounts are asked to reconnect, and the
/// creation probe has recorded an id since Task 6.
///
/// Fail-closed was rejected on that evidence: it would trade a bounded,
/// recoverable residue for an unbounded wedge, and diverge from the sibling
/// backend for a hazard the marker already contains.
struct SftpTornSidecarResidue;

#[async_trait]
impl Scenario for SftpTornSidecarResidue {
    fn name(&self) -> &'static str {
        "sftp-torn-sidecar-residue"
    }
    fn description(&self) -> &'static str {
        "a torn metadata sidecar reads as unannotated: bounded residue, no data loss, no duplicate"
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
        let fx = SftpFixture::new().await?;
        let handle = fx.boot().await?;
        let src = fx.add_source(&handle).await?;
        let content = payload(SMALL_FIXTURE_LEN, 47);
        write_file(fx.src_root(), "torn.bin", &content)?;
        write_file(fx.src_root(), "intact.bin", &payload(SMALL_FIXTURE_LEN, 48))?;
        handle.run_one_cycle().await?;

        let oracle = fx.oracle();
        let baseline = assert_invariants(&handle, &oracle, src.id, SFTP_SUBFOLDER).await?;
        anyhow::ensure!(
            baseline.live_object_count == 2 && baseline.ok(),
            "the row needs two healthy objects, got {}: {}",
            baseline.live_object_count,
            baseline.violation_summary()
        );

        let store = fx.store()?;
        let context = driven_remote::remote_store::DriveContext::MyDrive;
        let before = store
            .list_source_object_ids(&src.id.to_string(), &context)
            .await?;
        anyhow::ensure!(before.len() == 2, "the healthy audit sees both: {before:?}");

        // The crash: a sidecar written half way and then abandoned.
        let torn = fx.tear_sidecar(SFTP_SUBFOLDER, "torn.bin")?;
        anyhow::ensure!(
            fx.oracle().sidecars_in(SFTP_SUBFOLDER).len() == 2,
            "the torn sidecar must still be PRESENT - the row is about an unreadable annotation, \
             not a missing one"
        );

        // Measured, not predicted: this is the residue the ruling accepts, and
        // it is ASSERTED rather than merely reported, so a later change that
        // made an unreadable sidecar fail closed (or made the audit data-driven)
        // has to revisit the ruling instead of silently rewriting the note.
        let after_tear = store
            .list_source_object_ids(&src.id.to_string(), &context)
            .await?;
        anyhow::ensure!(
            after_tear.len() == 1 && !after_tear.contains(&format!("{SFTP_SUBFOLDER}/torn.bin")),
            "the residue: an object whose annotation cannot be read must drop OUT of the \
             remote-existence audit (that is the accepted cost), got {after_tear:?}"
        );
        // The other half of the asymmetry, and the reason this is cost rather
        // than loss: `list_folder` is driven by the DATA files and joins the
        // sidecar onto them, so the object is still plainly there and still
        // restorable. Only the annotation-driven audit loses sight of it.
        let listed = store.list_folder(SFTP_SUBFOLDER, &context).await?;
        anyhow::ensure!(
            listed.len() == 2,
            "the object itself must remain visible and restorable through the store's own \
             listing, got {:?}",
            listed.iter().map(|e| &e.id).collect::<Vec<_>>()
        );

        // The object itself must be untouched, whatever the audit thinks of it.
        let torn_id = format!("{SFTP_SUBFOLDER}/torn.bin");
        anyhow::ensure!(
            oracle.object_bytes(&torn_id).as_deref() == Some(content.as_slice()),
            "a torn ANNOTATION must never disturb the DATA it annotates"
        );

        // What the ORCHESTRATOR does about it, which is the half that decides
        // whether the residue is permanent or self-healing.
        //
        // Reaching that takes a REBOOT, for the reason the truncated-listing
        // row documents: `audit_remote_existence_if_due` is gated on a
        // per-source latch held in the orchestrator's memory, and the healthy
        // first cycle already spent this source's audit - so a further cycle on
        // the SAME orchestrator returns before issuing a remote call and would
        // prove nothing at all. A fresh handle re-audits, which is also the
        // real shape: the app restarts and looks again.
        drop(handle);
        let rebooted = fx.boot().await?;
        let _ = rebooted.run_one_cycle().await;
        let after_cycle = store
            .list_source_object_ids(&src.id.to_string(), &context)
            .await?;

        let codes = error_codes_in_activity(rebooted.state.as_ref()).await?;
        let quiesced = is_quiescent(&rebooted.state().await);
        let report = assert_invariants(&rebooted, &oracle, src.id, SFTP_SUBFOLDER).await?;
        anyhow::ensure!(
            report.ok(),
            "s6.3 invariants violated: {}",
            report.violation_summary()
        );
        anyhow::ensure!(
            report.live_object_count == 2,
            "a torn sidecar must not produce, remove or duplicate an object: {} live",
            report.live_object_count
        );
        let checked = assert_synced_bytes_match_local(&rebooted, &oracle, &src).await?;
        anyhow::ensure!(
            checked == 2,
            "both objects must still be byte-verifiable against their local files, checked \
             {checked}"
        );
        // Whatever the audit decided, the DATA is what must never move.
        anyhow::ensure!(
            oracle.object_bytes(&torn_id).as_deref() == Some(content.as_slice()),
            "the object's bytes must survive the audit's opinion of its annotation"
        );
        let _ = &torn;
        let notes = vec![
            format!(
                "RULING (accepted behaviour): a torn sidecar reads as UNANNOTATED. The \
                 remote-existence audit dropped the object from {} live id(s) to {} while \
                 `list_folder` still reported {} object(s), so the object stayed visible and \
                 restorable throughout. After a rebooted orchestrator re-ran the audit the set \
                 was {} - and either way the data, its bytes and its file_state row were \
                 untouched, with no duplicate and no deletion.",
                before.len(),
                after_tear.len(),
                listed.len(),
                after_cycle.len()
            ),
            "cost, not data loss: an object the audit cannot see is one nothing will ever \
             reclaim if its file_state row also goes - the same bounded-residue trade the \
             abandoned-multipart finding records. Fail-closed was rejected: erroring on an \
             unreadable sidecar would let ONE bad byte wedge every audit against the server, and \
             `guard_root` already contains the dangerous variant (adopting a stranger's file at a \
             usurped destination) for every account carrying a destination_id."
                .to_string(),
        ];
        Ok(finish(&report, quiesced, codes, notes))
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
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

        // -- arm D: the SSH (SFTP) backend, via its identity marker -----------
        //
        // Hand-copied from arm C rather than dispatched over a shared trait,
        // deliberately and in keeping with the rest of this module: the arms
        // differ in how the fault is INJECTED (a marker rewritten on the served
        // directory, a `NoSuchBucket` on the wire, a latched flag) and any
        // abstraction that hid that would also hide the thing each arm is here
        // to prove. The duplication is the point - it is what makes "the
        // invariants are backend-independent" a test of four independent
        // detectors rather than of one shared code path called four times.
        //
        // The SFTP hazard is the same one the local-folder marker exists for,
        // one network hop further away. A NAS whose external volume or array is
        // not mounted this cycle leaves an ordinary empty directory at the
        // mount point: the connection succeeds, the credential authenticates,
        // the path exists and is writable, and a whole backup written into it
        // disappears on the next remount while `file_state` still calls every
        // file synced. Existence proves nothing over SSH either, which is why
        // this backend carries the same `.driven-destination.json`, byte for
        // byte, and verifies it on EVERY mutating operation.
        //
        // As in arm C the injected fault is the NASTIER of the two the marker
        // catches: not a missing marker, but one naming a DIFFERENT destination
        // - a second machine that re-initialized the same shared export.
        {
            let fx = SftpFixture::new().await?;
            let handle = fx.boot().await?;
            let src = fx.add_source(&handle).await?;
            write_file(fx.src_root(), "before.bin", &payload(SMALL_FIXTURE_LEN, 25))?;
            let oracle = fx.oracle();

            handle.run_one_cycle().await?;
            let baseline = assert_invariants(&handle, &oracle, src.id, SFTP_SUBFOLDER).await?;
            anyhow::ensure!(
                baseline.live_object_count == 1 && baseline.ok(),
                "the SFTP arm needs a healthy baseline, got {} object(s): {}",
                baseline.live_object_count,
                baseline.violation_summary()
            );

            fx.swap_marker_identity()?;
            write_file(fx.src_root(), "after.bin", &payload(SMALL_FIXTURE_LEN, 26))?;
            let _ = handle.run_one_cycle().await;

            let arm_codes = error_codes_in_activity(handle.state.as_ref()).await?;
            anyhow::ensure!(
                arm_codes.contains(&ErrorCode::DriveDestFolderMissing),
                "SFTP arm: a marker naming a different destination must raise \
                 drive.dest_folder_missing, got {arm_codes:?}"
            );
            let after = assert_invariants(&handle, &oracle, src.id, SFTP_SUBFOLDER).await?;
            anyhow::ensure!(
                after.live_object_count == 1,
                "SFTP arm: NOTHING may be written to a destination that is not ours - that is the \
                 whole point of the marker - but the destination holds {} object(s)",
                after.live_object_count
            );
            anyhow::ensure!(
                after.ok(),
                "SFTP arm: s6.3 invariants violated: {}",
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
                "SFTP arm: {synced_after:?} was marked synced although the destination belonged to \
                 a different Driven install"
            );

            quiesced &= is_quiescent(&handle.state().await);
            for c in arm_codes {
                if !codes.contains(&c) {
                    codes.push(c);
                }
            }
            notes.push(format!(
                "SFTP arm (marker holds a different destination id): baseline kept, nothing new \
                 written, {} live object(s)",
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
