//! Concurrency-edge scenarios (STRESS_HARNESS s3.8).
//!
//! Three rows, all crash / pause / resume edges around the resumable-upload
//! and crash-recovery machinery (DESIGN s5.4 byte-level resume, s5.6
//! `client_op_uuid` orphan adoption):
//!
//! `pause-mid-resumable-5m`
//!   Pause 5 min into a resumable upload; advance the clock 5 min; resume.
//!   The persisted session is younger than the executor's 6-day discard
//!   horizon (`SESSION_MAX_AGE_MS`), so reconcile resumes BYTE-FOR-BYTE from
//!   the last-acked offset and finalises the SAME object - no duplicate, no
//!   from-zero re-upload.
//!
//! `pause-mid-resumable-7d`
//!   Same setup, but advance the clock 7 days. The persisted session is now
//!   OLDER than the 6-day horizon, so reconcile DISCARDS it (rather than
//!   feeding a Drive-dead session forward) and requeues a clean upload; the
//!   next full sync cycle re-uploads from byte 0. The
//!   `drive.resumable_session_invalid` condition is handled INSIDE the
//!   executor (reconcile returns `Ok`) and never leaks past it.
//!
//! `kill-9-mid-pipeline`
//!   `kill_orchestrator()` mid 16-file pipeline (a Drive fault drops one
//!   file's upload mid-stream so a live session is persisted), then reboot a
//!   fresh [`DrivenHandle`] over the SAME state + remote. The reboot's first
//!   cycle runs the startup reconciliation pass (DESIGN s5.6): orphaned /
//!   half-uploaded files are adopted or resumed via `find_by_op_uuid`; the
//!   final state has every file backed up exactly once with no duplicates.
//!
//! Each scenario asserts the s6.3 cross-scenario invariants (no data loss, no
//! duplicate remote objects, clean terminal state) PLUS its own expected
//! outcome.
//!
//! ## Driving level (surfaced finding)
//!
//! The two `pause-mid-resumable-*` rows MUST advance the [`FakeClock`] to
//! exercise the executor's `SESSION_MAX_AGE_MS` (6-day) session-age branch.
//! The Phase-1 [`crate::handle::DrivenHandleBuilder`] constructs its
//! `FakeClock` internally and exposes it only as `Arc<dyn Clock>` (no
//! `.clock()` injector, and `Clock` is not `Any`, so the concrete clock is
//! not reachable for `advance`). These two scenarios therefore build a
//! [`DefaultExecutor`] directly over the same hermetic seams (the established
//! pattern for executor-internal rows in `driven-core/tests/e2e_fake.rs`),
//! which is the ONLY way to drive the session-age check deterministically.
//! `kill-9-mid-pipeline` needs only `kill_orchestrator` + reboot, both first-
//! class on [`DrivenHandle`], and so drives the full headless handle. The
//! STRESS_HARNESS s2.4 sketch should grow a clock seam on the builder; noted
//! for the Integrate agent.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use driven_core::executor::{DefaultExecutor, Executor, ExecutorDeps};
use driven_core::orchestrator::OrchestratorConfig;
use driven_core::pacer::{Pacer, PacerCeilings, ResponseClass};
use driven_core::state::{
    AccountRow, ActivityFilter, PageRequest, SourceRow, SqliteStateRepo, StateRepo,
};
use driven_core::types::{
    AccountId, AccountState, ErrorCode, Op, OrchestratorState, Plan, RelativePath, SourceId,
};

use driven_drive::fake::InMemoryRemoteStore;
use driven_drive::remote_store::RemoteStore;

use driven_test_fixtures::clock::FakeClock;

use crate::capabilities::CapabilityRequirements;
use crate::handle::{power_on_ac, DrivenHandle, DrivenHandleBuilder};
use crate::scenario::{ExpectedOutcome, Outcome, Scenario, ScenarioContext};

/// Every concurrency-edge scenario (STRESS_HARNESS s3.8).
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(PauseMidResumable5m),
        Box::new(PauseMidResumable7d),
        Box::new(Kill9MidPipeline),
    ]
}

// ===========================================================================
// Shared harness helpers (mirrors driven-core/tests/e2e_fake.rs)
// ===========================================================================

/// A pass-through [`Pacer`] that never gates. The real `AimdPacer` blocks on
/// `tokio::time` while polling a non-advancing `FakeClock` and would deadlock;
/// the harness asserts sync correctness, not pacing (which is unit-tested in
/// `pacer.rs`).
struct NoopPacer;

#[async_trait]
impl Pacer for NoopPacer {
    async fn permit_request(&self) {}
    async fn permit_file_create(&self) {}
    async fn permit_bytes(&self, _n: u64) {}
    fn note_response(&self, _classification: ResponseClass) {}
    fn ceilings(&self) -> PacerCeilings {
        PacerCeilings::default()
    }
}

/// Open a throwaway hermetic state DB under `dir`.
async fn open_state(dir: &std::path::Path) -> anyhow::Result<Arc<SqliteStateRepo>> {
    Ok(Arc::new(
        SqliteStateRepo::open(&dir.join("state.db")).await?,
    ))
}

/// Seed one account on a fresh DB.
async fn seed_account(state: &SqliteStateRepo) -> anyhow::Result<AccountId> {
    let id = AccountId::new_v4();
    state
        .upsert_account(&AccountRow {
            id,
            email: "chaos@example.com".into(),
            display_name: None,
            state: AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        })
        .await?;
    Ok(id)
}

/// A plaintext source rooted at `root`, uploading into `folder_id`.
fn source_in(account: AccountId, root: &std::path::Path, folder_id: &str) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id: account,
        display_name: "concurrency".into(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: folder_id.to_string(),
        drive_id: None,
        drive_folder_path: "/concurrency".into(),
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
        created_at: 0,
    }
}

fn write_file(root: &std::path::Path, rel: &str, contents: &[u8]) -> anyhow::Result<()> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, contents)?;
    Ok(())
}

/// Build a [`DefaultExecutor`] over the given seams (used by the two clock-
/// driven rows that need a reachable `FakeClock`).
fn executor(
    remote: Arc<InMemoryRemoteStore>,
    state: Arc<SqliteStateRepo>,
    clock: Arc<FakeClock>,
) -> DefaultExecutor {
    DefaultExecutor::with_clock(
        ExecutorDeps {
            remote,
            state,
            pacer: Arc::new(NoopPacer),
            crypto: None,
            vss: None,
            network: None,
        },
        clock,
    )
}

fn noop_progress(_p: driven_core::types::ExecProgress) {}

/// Count non-trashed objects under `folder_id`.
async fn live_object_count(remote: &InMemoryRemoteStore, folder_id: &str) -> anyhow::Result<usize> {
    Ok(remote
        .list_folder(
            folder_id,
            &driven_drive::remote_store::DriveContext::MyDrive,
        )
        .await?
        .iter()
        .filter(|e| !e.trashed)
        .count())
}

/// Re-derive the BLAKE3 of one object's stored bytes (the harness needs no
/// `blake3` dependency: it compares raw bytes, the same data-loss check
/// STRESS_HARNESS s6.3 mandates, by downloading and comparing to the local
/// source bytes directly).
async fn download_bytes(remote: &InMemoryRemoteStore, file_id: &str) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    remote
        .download(file_id)
        .await?
        .0
        .read_to_end(&mut buf)
        .await?;
    Ok(buf)
}

/// Harvest every stable error code surfaced in the activity log for `source`.
/// Activity `event_type`s are the dotted codes (SPEC s2 / s24), so a row whose
/// `event_type` parses back to an [`ErrorCode`] is one Driven surfaced.
async fn activity_error_codes(
    state: &SqliteStateRepo,
    source: SourceId,
) -> anyhow::Result<Vec<ErrorCode>> {
    let page = state
        .query_activity(
            ActivityFilter {
                source_id: Some(source),
                ..ActivityFilter::default()
            },
            PageRequest::first(500),
        )
        .await?;
    let mut codes = Vec::new();
    for row in page.rows {
        if let Some(code) = ErrorCode::from_code(&row.event_type) {
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
    }
    Ok(codes)
}

/// The size of the resumable test file. Chosen exactly like the e2e_fake
/// `crash_mid_upload_resumes_persisted_session_byte_for_byte` row so the real
/// executor flushes at least two 4 MiB wire chunks before EOF: the first chunk
/// acks an offset and the SECOND is where the injected drop lands, leaving a
/// genuine persisted session with a non-zero acked offset.
const WIRE_CHUNK: usize = 4 * 1024 * 1024;
const CHUNK_256K: usize = 256 * 1024;

fn resumable_test_bytes() -> Vec<u8> {
    let total = 5 * WIRE_CHUNK + 3 * CHUNK_256K + 17;
    (0..total).map(|i| (i % 251) as u8).collect()
}

/// Drive a REAL mid-stream crash: a `with_network_drop_after(2)` remote drops
/// the second wire chunk of a streaming resumable upload, so `execute` aborts
/// with the create op KEPT in `pending_ops` carrying the live session + a
/// non-zero acked offset (DESIGN s5.4). Returns the bytes written so callers
/// can assert byte-identity after recovery.
///
/// The drop is single-shot, so the post-crash executor's requests all succeed.
async fn crash_mid_resumable(
    remote: &Arc<InMemoryRemoteStore>,
    state: &Arc<SqliteStateRepo>,
    clock: &Arc<FakeClock>,
    src: &SourceRow,
    src_dir: &std::path::Path,
    rel_name: &str,
) -> anyhow::Result<Vec<u8>> {
    let bytes = resumable_test_bytes();
    write_file(src_dir, rel_name, &bytes)?;
    let rel = RelativePath::try_from(rel_name.to_string())?;
    let exec = executor(remote.clone(), state.clone(), clock.clone());
    let plan = Plan {
        ops: vec![Op::HashThenUpload {
            source_id: src.id,
            relative_path: rel,
            size: bytes.len() as u64,
        }],
        collisions: vec![],
    };
    // The dropped chunk is a fatal Drive error: execute returns Err and the
    // op is kept (the create op is never deleted mid-stream).
    let result = exec
        .execute(
            src,
            &plan,
            &noop_progress,
            &driven_core::executor::noop_outcome_sink,
        )
        .await;
    anyhow::ensure!(
        result.is_err(),
        "expected the mid-stream network drop to abort the upload, got {result:?}"
    );
    // The resumable Create commits only on its final chunk, which never
    // landed: no object yet.
    anyhow::ensure!(
        live_object_count(remote, &src.drive_folder_id).await? == 0,
        "no object should exist until the resumable create finalises"
    );
    let pending = state.get_pending_ops_for_source(src.id).await?;
    anyhow::ensure!(
        pending.len() == 1,
        "the create op must survive the crash; got {} ops",
        pending.len()
    );
    Ok(bytes)
}

// ===========================================================================
// pause-mid-resumable-5m
// ===========================================================================

/// STRESS_HARNESS s3.8: pause 5 min into a resumable upload, advance the
/// clock 5 min, resume. Session still valid -> byte-level resume, no
/// duplicate.
struct PauseMidResumable5m;

#[async_trait]
impl Scenario for PauseMidResumable5m {
    fn name(&self) -> &'static str {
        "pause-mid-resumable-5m"
    }

    fn description(&self) -> &'static str {
        "Pause 5 min into a resumable upload then resume; session still valid, byte-level resume, no duplicate."
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }

    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let dir = tempfile::tempdir()?;
        let src_dir = tempfile::tempdir()?;
        let state = open_state(dir.path()).await?;
        let account = seed_account(&state).await?;
        let remote = Arc::new(InMemoryRemoteStore::new().with_network_drop_after(2));
        let folder = remote.root_id().to_string();
        let clock = Arc::new(FakeClock::new());

        let src = source_in(account, src_dir.path(), &folder);
        state.upsert_source(&src).await?;
        let rel_name = "resume.bin";
        let bytes =
            crash_mid_resumable(&remote, &state, &clock, &src, src_dir.path(), rel_name).await?;
        let rel = RelativePath::try_from(rel_name.to_string())?;

        // Pause: 5 minutes pass. The persisted session's `issued_at` is t=0,
        // so its age is 5 min - well under the 6-day discard horizon.
        clock.advance(Duration::from_secs(5 * 60));

        // Resume: a fresh executor over the SAME state + remote reconciles.
        let exec2 = executor(remote.clone(), state.clone(), clock.clone());
        exec2.reconcile(&src).await?;

        // The upload completed via byte-level resume: exactly one object, full
        // size, the SAME session consumed (not a from-zero re-do).
        let children = remote
            .list_folder(&folder, &driven_drive::remote_store::DriveContext::MyDrive)
            .await?;
        anyhow::ensure!(
            children.len() == 1,
            "resume must finalise exactly one object; got {}",
            children.len()
        );
        anyhow::ensure!(
            children[0].size == Some(bytes.len() as u64),
            "resumed object must carry the full byte count (tail bytes landed via resume)"
        );
        anyhow::ensure!(
            remote.open_session_count() == 0,
            "the resumed session must be consumed, not leaked"
        );

        // No data loss: the stored bytes equal the local source bytes.
        let stored = download_bytes(&remote, &children[0].id).await?;
        let hash_matches = stored == bytes;
        anyhow::ensure!(
            hash_matches,
            "stored bytes must match the local source bytes"
        );

        // file_state Synced, op drained, no leaked pending ops.
        let fs = state
            .get_file_state(src.id, &rel)
            .await?
            .ok_or_else(|| anyhow::anyhow!("file_state must be committed by resume"))?;
        anyhow::ensure!(
            fs.drive_file_id.as_deref() == Some(children[0].id.as_str()),
            "file_state must record the resumed object id"
        );
        anyhow::ensure!(
            state.get_pending_ops_for_source(src.id).await?.is_empty(),
            "the create op must drain after a successful resume"
        );

        // No `drive.resumable_session_invalid` was surfaced (a valid session
        // resumes cleanly, never invalidates).
        let codes = activity_error_codes(&state, src.id).await?;
        anyhow::ensure!(
            !codes.contains(&ErrorCode::DriveResumableSessionInvalid),
            "a within-horizon session must not surface drive.resumable_session_invalid; saw {codes:?}"
        );

        // s6.3 cross-scenario invariants, computed from the SAME hermetic state
        // + remote the executor wrote. This row is executor-driven (no
        // orchestrator handle), so boot a throwaway handle over the same state
        // DB + remote purely to run the canonical checker against the terminal
        // state; the resume already drained the queue and committed the synced
        // row, so booting touches neither file_state, pending_ops, nor objects.
        let inv_handle = DrivenHandleBuilder::new(dir.path().join("state.db"))
            .remote(remote.clone())
            .power(power_on_ac())
            .boot()
            .await?;
        let report =
            crate::scenarios::reporting::assert_invariants(&inv_handle, &remote, src.id, &folder)
                .await?;
        // clean_shutdown holds: reconcile ran to completion, drained the create
        // op, and left no open session (both asserted above).
        let invariants = Some(report.to_invariant_outcome(true));

        Ok(Outcome {
            error_codes_seen: codes,
            final_drive_object_count: 1,
            final_hash_matches_local: hash_matches,
            notes: vec![
                "5-min-old resumable session resumed byte-for-byte; one object, no duplicate"
                    .to_string(),
            ],
            invariants,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ===========================================================================
// pause-mid-resumable-7d
// ===========================================================================

/// STRESS_HARNESS s3.8: pause 7 days into a resumable upload, then resume.
/// The session is older than the executor's 6-day horizon, so it is discarded
/// and the upload restarts from byte 0; `drive.resumable_session_invalid`
/// never leaks past the executor.
struct PauseMidResumable7d;

#[async_trait]
impl Scenario for PauseMidResumable7d {
    fn name(&self) -> &'static str {
        "pause-mid-resumable-7d"
    }

    fn description(&self) -> &'static str {
        "Pause 7 days into a resumable upload; stored session discarded (>6 days), upload restarts from byte 0, no leaked error."
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }

    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let dir = tempfile::tempdir()?;
        let src_dir = tempfile::tempdir()?;
        let state = open_state(dir.path()).await?;
        let account = seed_account(&state).await?;
        let remote = Arc::new(InMemoryRemoteStore::new().with_network_drop_after(2));
        let folder = remote.root_id().to_string();
        let clock = Arc::new(FakeClock::new());

        let src = source_in(account, src_dir.path(), &folder);
        state.upsert_source(&src).await?;
        let rel_name = "resume.bin";
        let bytes =
            crash_mid_resumable(&remote, &state, &clock, &src, src_dir.path(), rel_name).await?;
        let rel = RelativePath::try_from(rel_name.to_string())?;

        // Pause: 7 days pass. The persisted session's age now exceeds the
        // executor's 6-day SESSION_MAX_AGE horizon.
        clock.advance(Duration::from_secs(7 * 24 * 60 * 60));

        // Reconcile: the executor must DISCARD the stale session (handling the
        // resumable-session-invalid condition internally) and requeue a clean
        // upload. It must NOT feed a Drive-dead session forward, and reconcile
        // returns Ok (no error leaks past the executor).
        let exec2 = executor(remote.clone(), state.clone(), clock.clone());
        exec2.reconcile(&src).await?;

        // The stale create op was dropped (the create never finalised, so
        // find_by_op_uuid finds nothing -> requeue clean). No partial object.
        anyhow::ensure!(
            live_object_count(&remote, &folder).await? == 0,
            "the discarded session must leave no orphaned object behind"
        );

        // The next full sync cycle re-uploads the file from byte 0 through the
        // real scan -> plan -> execute pipeline.
        let handle = DrivenHandleBuilder::new(dir.path().join("state.db"))
            .remote(remote.clone())
            .power(power_on_ac())
            .boot()
            .await?;
        handle.run_one_cycle().await?;

        // Exactly one object, full size, no duplicate.
        let children = remote
            .list_folder(&folder, &driven_drive::remote_store::DriveContext::MyDrive)
            .await?;
        anyhow::ensure!(
            children.len() == 1,
            "restart must produce exactly one object; got {}",
            children.len()
        );
        anyhow::ensure!(
            children[0].size == Some(bytes.len() as u64),
            "restarted object must carry the full byte count (re-uploaded from byte 0)"
        );
        // The discarded >6-day session is deliberately NOT aborted by Driven:
        // DESIGN s5.4 leaves the partial create for Drive to garbage-collect on
        // expiry (executor.rs returns `Ok(None)` on the stale session, it does
        // not cancel it server-side). So the fake legitimately still shows that
        // one abandoned, GC-pending session. The invariant that actually
        // matters is that the RESTART's own from-zero upload session was
        // consumed and did not leak - i.e. at most the single stale session
        // remains, never two.
        anyhow::ensure!(
            remote.open_session_count() <= 1,
            "the restart's own resumable session must be consumed; only the \
             discarded GC-pending session may remain (got {} open)",
            remote.open_session_count()
        );

        // No data loss: stored bytes equal the local source bytes.
        let stored = download_bytes(&remote, &children[0].id).await?;
        let hash_matches = stored == bytes;
        anyhow::ensure!(
            hash_matches,
            "re-uploaded bytes must match the local source bytes"
        );

        // file_state Synced, no leaked pending ops.
        let fs = state
            .get_file_state(src.id, &rel)
            .await?
            .ok_or_else(|| anyhow::anyhow!("file_state must be committed by the restart cycle"))?;
        anyhow::ensure!(
            fs.drive_file_id.as_deref() == Some(children[0].id.as_str()),
            "file_state must record the re-uploaded object id"
        );
        anyhow::ensure!(
            state.get_pending_ops_for_source(src.id).await?.is_empty(),
            "no pending op may leak after the restart"
        );

        // The orchestrator settled clean (Idle), and no
        // drive.resumable_session_invalid surfaced past the executor.
        anyhow::ensure!(
            matches!(handle.state().await, OrchestratorState::Idle { .. }),
            "the orchestrator must settle Idle after the restart cycle"
        );
        let codes = activity_error_codes(&state, src.id).await?;
        anyhow::ensure!(
            !codes.contains(&ErrorCode::DriveResumableSessionInvalid),
            "the stale-session discard must be handled inside the executor and not leak \
             drive.resumable_session_invalid; saw {codes:?}"
        );

        // s6.3 cross-scenario invariants over the rebooted handle's terminal
        // state (same state DB + remote the restart cycle ran on).
        let report =
            crate::scenarios::reporting::assert_invariants(&handle, &remote, src.id, &folder)
                .await?;
        // clean_shutdown holds: the orchestrator settled Idle (asserted above).
        let clean_shutdown = matches!(handle.state().await, OrchestratorState::Idle { .. });
        let invariants = Some(report.to_invariant_outcome(clean_shutdown));

        Ok(Outcome {
            error_codes_seen: codes,
            final_drive_object_count: 1,
            final_hash_matches_local: hash_matches,
            notes: vec![
                "7-day-old resumable session discarded (>6 days); upload restarted from byte 0, \
                 one object, no leaked drive.resumable_session_invalid"
                    .to_string(),
            ],
            invariants,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ===========================================================================
// kill-9-mid-pipeline
// ===========================================================================

/// STRESS_HARNESS s3.8: `kill_orchestrator()` mid 16-file pipeline, reboot
/// handle. The reboot's startup reconciliation pass (DESIGN s5.6) adopts or
/// resumes orphans via `find_by_op_uuid`; final state has every file backed up
/// exactly once with no duplicates.
struct Kill9MidPipeline;

#[async_trait]
impl Scenario for Kill9MidPipeline {
    fn name(&self) -> &'static str {
        "kill-9-mid-pipeline"
    }

    fn description(&self) -> &'static str {
        "kill -9 mid 16-file pipeline then reboot; reconciliation adopts/resumes orphans, no duplicates, no data loss."
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }

    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        // A 16-file pipeline: 15 small files plus 1 big file on the streaming
        // resumable path. The crash is staged DETERMINISTICALLY (see below) so
        // the reboot has a genuine mixed mid-pipeline state to recover from:
        // one half-uploaded file with a persisted resumable session, some files
        // never started.
        let dir = tempfile::tempdir()?;
        let src_dir = tempfile::tempdir()?;
        let state = open_state(dir.path()).await?;
        let account = seed_account(&state).await?;

        // The crash is induced on the BIG file's streaming upload (the only
        // path on which a single network drop is fatal: the inline create path
        // retries transient drops inline up to MAX_TRANSIENT_RETRIES and would
        // simply succeed, so a drop landing on a small file would NOT crash the
        // pipeline). Driving the big file's crash directly through the executor
        // - rather than racing a concurrent orchestrator pool against a single-
        // shot drop whose target file is non-deterministic - guarantees the
        // mid-stream crash lands where it must. The drop is single-shot, so
        // every post-reboot request succeeds.
        let remote = Arc::new(InMemoryRemoteStore::new().with_network_drop_after(2));
        let folder = remote.root_id().to_string();

        let src = source_in(account, src_dir.path(), &folder);
        state.upsert_source(&src).await?;

        // Stage the 15 small files on disk (they exist locally but were NOT yet
        // uploaded when the process died - the "files not yet started" half of
        // a mid-pipeline kill).
        let mut expected: Vec<(String, Vec<u8>)> = Vec::new();
        for i in 0..15u32 {
            let name = format!("small{i:02}.txt");
            let body = format!("pipeline-payload-{i}-{}", "x".repeat(i as usize)).into_bytes();
            write_file(src_dir.path(), &name, &body)?;
            expected.push((name, body));
        }

        // Crash the big streaming file mid-upload: a real execute() that aborts
        // on the dropped chunk, leaving the create op KEPT in pending_ops with
        // a live resumable session (DESIGN s5.4). This is the in-flight file the
        // process was uploading at the instant of the kill.
        let big_name = "big-streaming.bin";
        let crash_clock = Arc::new(FakeClock::new());
        let big_bytes = crash_mid_resumable(
            &remote,
            &state,
            &crash_clock,
            &src,
            src_dir.path(),
            big_name,
        )
        .await?;
        expected.push((big_name.to_string(), big_bytes.clone()));

        // Model the `kill -9`: boot a handle over the SAME state + remote and
        // immediately drop it via `kill_orchestrator` WITHOUT a graceful
        // shutdown. (We do not run a cycle through this handle: the mid-pipeline
        // crash state is already staged above; running an inline-retrying cycle
        // here would mask the crash rather than model the abrupt death.) The
        // hermetic SQLite file and the Arc'd remote survive the kill.
        let handle1 = DrivenHandleBuilder::new(dir.path().join("state.db"))
            .remote(remote.clone())
            .power(power_on_ac())
            .config(crate::handle::HermeticConfig {
                run_uuid: uuid::Uuid::new_v4().to_string(),
                orchestrator: OrchestratorConfig::default(),
            })
            .boot()
            .await?;
        let killed_state = handle1.kill_orchestrator().await;
        drop(killed_state);

        // --- reboot over the SAME state + remote ----------------------------
        // The reboot's first cycle runs the startup reconciliation pass
        // (DESIGN s5.6) before the normal scan: it adopts finalised orphans and
        // resumes the persisted session, then the scan + plan + execute pass
        // uploads anything still missing.
        let handle2 = DrivenHandleBuilder::new(dir.path().join("state.db"))
            .remote(remote.clone())
            .power(power_on_ac())
            .boot()
            .await?;
        handle2.run_one_cycle().await?;
        // A second cycle proves steady state: no re-upload, no new duplicate.
        handle2.run_one_cycle().await?;

        // --- invariants -----------------------------------------------------
        // Every file backed up exactly once: 16 live objects, names unique.
        let children = remote
            .list_folder(&folder, &driven_drive::remote_store::DriveContext::MyDrive)
            .await?;
        let live: Vec<_> = children.iter().filter(|e| !e.trashed).collect();
        anyhow::ensure!(
            live.len() == expected.len(),
            "after reconciliation every file must be backed up exactly once: expected {}, got {}",
            expected.len(),
            live.len()
        );
        let mut names: Vec<&str> = live.iter().map(|e| e.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        anyhow::ensure!(
            names.len() == live.len(),
            "no duplicate remote objects may exist after reconciliation"
        );

        // No data loss: every file's stored bytes equal its local bytes.
        let mut hash_matches = true;
        for (name, body) in &expected {
            let entry = live.iter().find(|e| &e.name == name).ok_or_else(|| {
                anyhow::anyhow!("file {name} missing from the remote after recovery")
            })?;
            anyhow::ensure!(
                entry.size == Some(body.len() as u64),
                "file {name} has the wrong size on the remote after recovery"
            );
            let stored = download_bytes(&remote, &entry.id).await?;
            if &stored != body {
                hash_matches = false;
            }
        }
        anyhow::ensure!(
            hash_matches,
            "every recovered file's bytes must match its local source"
        );

        // No leaked pending ops, clean terminal state, no leaked sessions.
        anyhow::ensure!(
            state.get_pending_ops_for_source(src.id).await?.is_empty(),
            "reconciliation must drain every pending op"
        );
        anyhow::ensure!(
            remote.open_session_count() == 0,
            "no resumable session may be left open after recovery"
        );
        anyhow::ensure!(
            matches!(handle2.state().await, OrchestratorState::Idle { .. }),
            "the rebooted orchestrator must settle Idle"
        );

        // Harvest the codes surfaced across recovery for the report. NOTE
        // (surfaced finding): STRESS_HARNESS s3.8 lists `state.reconcile_orphan`
        // as a logged outcome of this row, but the M3 core defines the
        // `ErrorCode::StateReconcileOrphan` variant WITHOUT emitting it from the
        // executor's reconcile / adopt path (verified: the only references to
        // it are the enum definition + code<->string mapping in
        // driven-core/src/types.rs; no `write_activity` / event uses it). The
        // observable reconciliation contract (orphan adopted/resumed, no
        // duplicate, no data loss, op drained) DOES hold and is asserted above;
        // the missing log marker is reported to the Integrate agent rather than
        // faked green. This scenario therefore asserts the real behaviour and
        // records the gap as a note, with a DocumentedBehaviour expectation.
        let codes = activity_error_codes(&state, src.id).await?;
        let reconcile_orphan_logged = codes.contains(&ErrorCode::StateReconcileOrphan);

        let mut notes = vec![format!(
            "16-file pipeline killed mid-stream; reconciliation recovered all {} files with no \
             duplicates and no data loss",
            expected.len()
        )];
        if !reconcile_orphan_logged {
            notes.push(
                "FINDING: STRESS_HARNESS s3.8 expects `state.reconcile_orphan` logged, but the M3 \
                 core never emits ErrorCode::StateReconcileOrphan (enum defined, never written). \
                 Asserted the observable reconcile invariants instead; the log marker is a core \
                 gap for the maintainer, not a harness failure."
                    .to_string(),
            );
        }

        // s6.3 cross-scenario invariants over the rebooted handle's terminal
        // state (same state DB + remote the reconciliation cycles ran on).
        let report =
            crate::scenarios::reporting::assert_invariants(&handle2, &remote, src.id, &folder)
                .await?;
        // clean_shutdown holds: the rebooted orchestrator settled Idle
        // (asserted above).
        let clean_shutdown = matches!(handle2.state().await, OrchestratorState::Idle { .. });
        let invariants = Some(report.to_invariant_outcome(clean_shutdown));

        Ok(Outcome {
            error_codes_seen: codes,
            final_drive_object_count: live.len() as u64,
            final_hash_matches_local: hash_matches,
            notes,
            invariants,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        // The recovery is asserted by snapshot-style invariants (no duplicate,
        // no data loss, clean terminal state), not a single error code: the
        // spec's `state.reconcile_orphan` log marker is a documented core gap
        // (see the note in `run_assertions`), so a `GracefulFailureWith` on
        // that code would falsely require an unemitted code. DocumentedBehaviour
        // is the honest expectation.
        ExpectedOutcome::DocumentedBehaviour
    }
}
