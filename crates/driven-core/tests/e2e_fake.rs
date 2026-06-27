//! M3 ACCEPTANCE integration tests - one per ROADMAP M3 acceptance row.
//!
//! These drive the real M3 sync engine end to end against the fake seams from
//! `driven-test-fixtures` + `driven-drive`: an [`InMemoryRemoteStore`] for the
//! Drive side (with its fault-injection builders for the 429 / crash rows), a
//! [`FakePowerSource`] for the power gate, a hand-rolled `FakeNetProbe` for the
//! network-resilience reactions, a [`FakeClock`] for determinism, and a real
//! [`SqliteStateRepo`] on a throwaway temp DB so the `file_state` diff that
//! drives "only the changed files re-upload" is genuine, not mocked.
//!
//! Driving level is chosen per row (see the advisor split):
//! - Planner-delta + sync-policy rows (fresh sync, change-5, delete-3,
//!   dry-run, power gate, network matrix) drive the whole orchestrator
//!   `run_cycle`, which runs the real scan -> plan -> execute -> verify
//!   pipeline. Asserting on a hand-built `Plan` would make "only 5
//!   re-uploaded" vacuous.
//! - Executor-internal rows (429 retry, crash resume, concurrency,
//!   encryption round-trip) drive the [`DefaultExecutor`] directly with a
//!   hand-built `Plan`, mirroring the proven in-crate unit-test patterns.
//!
//! The quantitative perf-multiplier rows (throughput >=5x, blake3 >=2x,
//! adaptive parallelism) are kept as explicitly-named `#[ignore]`d benchmarks
//! with real bodies + how-to-run reasons (issue #28) rather than faked; see the
//! bottom of this file. The DNS-no-hang and lossy/intermittent breaker rows are
//! NOT here: they need the `#[cfg(test)]`-private FakeBackend/FakeClock/breaker
//! seam, so they live as real deterministic tests in driven-net (`resolve_within_*`)
//! and driven-core `network.rs` (`lossy_spread_loss_*`, `intermittent_link_*`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use driven_core::executor::{DefaultExecutor, Executor, ExecutorDeps, MemGauge, OpOutcome};
use driven_core::network::{NetworkProbe, NetworkState, ServiceHealth, ServiceName};
use driven_core::orchestrator::{Orchestrator, OrchestratorConfig, SyncOrchestrator, TickSource};
use driven_core::state::{AccountRow, NewPendingOp, SourceRow, SqliteStateRepo, StateRepo};
use driven_core::time::Clock;
use driven_core::types::{
    AccountId, AccountState, Op, OrchestratorEvent, OrchestratorState, Plan, RelativePath,
};

use driven_crypto::key::SourceKey;
use driven_crypto::{ContentDecryptor, DrivenCryptoSuite, SourceCryptoSuite, HEADER_LEN};

use driven_drive::fake::{InMemoryRemoteStore, CLIENT_OP_UUID_KEY};
use driven_drive::remote_store::{RemoteStore, UploadBody};

use driven_power::{PowerSource, PowerState};
use driven_test_fixtures::clock::FakeClock;
use driven_test_fixtures::power::FakePowerSource;

// ---------------------------------------------------------------------------
// Shared fakes + helpers
// ---------------------------------------------------------------------------

/// A minimal [`NetworkProbe`] for the orchestrator-reaction rows. The real
/// `Prober` + circuit-breaker mechanics are unit-tested in `network.rs`; here
/// we only need to drive the orchestrator's `evaluate_gates` decision, which
/// reads `probe()` and `service_health()`.
struct FakeNetProbe {
    state: std::sync::Mutex<NetworkState>,
    drive_health: std::sync::Mutex<ServiceHealth>,
    /// Counts `probe()` calls so a row can assert no probing happened.
    probe_calls: AtomicU64,
}

impl FakeNetProbe {
    fn online() -> Self {
        Self {
            state: std::sync::Mutex::new(NetworkState::Online),
            drive_health: std::sync::Mutex::new(ServiceHealth::Closed),
            probe_calls: AtomicU64::new(0),
        }
    }

    fn with_state(state: NetworkState) -> Self {
        Self {
            state: std::sync::Mutex::new(state),
            drive_health: std::sync::Mutex::new(ServiceHealth::Closed),
            probe_calls: AtomicU64::new(0),
        }
    }

    fn with_drive_open(retry_at: i64) -> Self {
        Self {
            state: std::sync::Mutex::new(NetworkState::Online),
            drive_health: std::sync::Mutex::new(ServiceHealth::Open { retry_at }),
            probe_calls: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl NetworkProbe for FakeNetProbe {
    async fn probe(&self) -> NetworkState {
        self.probe_calls.fetch_add(1, Ordering::SeqCst);
        *self.state.lock().unwrap()
    }
    fn service_health(&self, _service: ServiceName) -> ServiceHealth {
        *self.drive_health.lock().unwrap()
    }
    fn note_outcome(&self, _service: ServiceName, _ok: bool) {}
}

fn power_on_ac() -> PowerState {
    PowerState {
        ac_connected: true,
        battery_percent: Some(100),
        on_metered_network: false,
        network_reachable: true,
    }
}

fn power_on_battery() -> PowerState {
    PowerState {
        ac_connected: false,
        battery_percent: Some(50),
        on_metered_network: false,
        network_reachable: true,
    }
}

/// A source rooted at `root`, uploading into the fake remote's root folder.
fn source_in(account: AccountId, root: &std::path::Path, folder_id: &str) -> SourceRow {
    SourceRow {
        id: driven_core::types::SourceId::new_v4(),
        account_id: account,
        display_name: "e2e".into(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: folder_id.to_string(),
        drive_folder_path: "/e2e".into(),
        encryption_enabled: false,
        wrapped_source_key: None,
        respect_gitignore: false,
        include_patterns: vec![],
        exclude_patterns: vec![],
        schedule_json_v2_reserved: None,
        deep_verify_interval_secs: 604_800,
        last_full_scan_at: None,
        last_deep_verify_at: Some(0),
        created_at: 0,
    }
}

/// Open a throwaway state DB under `dir`.
async fn open_state(dir: &std::path::Path) -> Arc<SqliteStateRepo> {
    let db = dir.join("state.db");
    Arc::new(SqliteStateRepo::open(&db).await.unwrap())
}

async fn seed_account(state: &SqliteStateRepo) -> AccountId {
    let id = AccountId::new_v4();
    state
        .upsert_account(&AccountRow {
            id,
            email: "e2e@example.com".into(),
            display_name: None,
            state: AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        })
        .await
        .unwrap();
    id
}

/// Build an orchestrator over real state + the given fake seams.
fn orchestrator(
    account: AccountId,
    state: Arc<SqliteStateRepo>,
    remote: Arc<InMemoryRemoteStore>,
    power: Arc<dyn PowerSource>,
    net: Arc<dyn NetworkProbe>,
    clock: Arc<FakeClock>,
    config: OrchestratorConfig,
) -> SyncOrchestrator {
    let pacer = test_pacer(clock.clone());
    let executor = Arc::new(DefaultExecutor::with_clock(
        ExecutorDeps {
            remote,
            state: state.clone(),
            pacer,
            crypto: None,
            vss: None,
            network: None,
        },
        clock.clone(),
    ));
    SyncOrchestrator::new(account, state, executor, power, net, clock, config)
}

/// Build an orchestrator wired with a [`driven_vss::VssProvider`] (M3.5),
/// threading the SAME provider into both the executor (snapshot reads) and the
/// orchestrator (per-cycle release + orphan cleanup). Only the Windows degrade
/// row uses it (a real exclusive lock is Windows-only), so cfg-gate it to keep
/// non-Windows CI free of an unused-function warning.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn orchestrator_with_vss(
    account: AccountId,
    state: Arc<SqliteStateRepo>,
    remote: Arc<InMemoryRemoteStore>,
    power: Arc<dyn PowerSource>,
    net: Arc<dyn NetworkProbe>,
    clock: Arc<FakeClock>,
    config: OrchestratorConfig,
    vss: Arc<dyn driven_vss::VssProvider>,
) -> SyncOrchestrator {
    let pacer = test_pacer(clock.clone());
    let executor = Arc::new(DefaultExecutor::with_clock(
        ExecutorDeps {
            remote,
            state: state.clone(),
            pacer,
            crypto: None,
            vss: Some(vss.clone()),
            network: None,
        },
        clock.clone(),
    ));
    SyncOrchestrator::new(account, state, executor, power, net, clock, config).with_vss(vss)
}

/// A non-blocking [`Pacer`] for the acceptance rows.
///
/// These tests assert sync *correctness* (what gets uploaded / trashed /
/// resumed), not rate-pacing. The real `AimdPacer` blocks on `tokio::time`
/// while polling the injected clock for a token refill; driven by a
/// never-advancing `FakeClock`, its buckets drain past their initial burst and
/// `permit_*` deadlocks. The AIMD pacing behaviour is exercised deterministically
/// in `pacer.rs`'s own unit tests (which advance the `FakeClock`), so here we
/// substitute a pass-through pacer that never gates.
struct NoopPacer;

#[async_trait]
impl driven_core::pacer::Pacer for NoopPacer {
    async fn permit_request(&self) {}
    async fn permit_file_create(&self) {}
    async fn permit_bytes(&self, _n: u64) {}
    fn note_response(&self, _classification: driven_core::pacer::ResponseClass) {}
    fn ceilings(&self) -> driven_core::pacer::PacerCeilings {
        driven_core::pacer::PacerCeilings::default()
    }
}

fn test_pacer(_clock: Arc<FakeClock>) -> Arc<dyn driven_core::pacer::Pacer> {
    Arc::new(NoopPacer)
}

fn write_file(root: &std::path::Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, contents).unwrap();
}

/// Drive one orchestrator cycle while draining its event stream, returning the
/// per-counter MAXIMUM across every `Progress` event seen.
///
/// The executor emits a cumulative `Progress` after each op (so the running
/// `files_done` / `trashes_done` peak at their true totals), but the
/// orchestrator then forwards one synthetic closing snapshot whose
/// `trashes_done` is 0 (`exec_progress_from` can only recover `files_done` +
/// `errors` from the type-erased `OpOutcome`s, not the upload-vs-trash split).
/// Taking the max per counter recovers the true totals regardless of which
/// snapshot lands last. Used only for the small-delta rows; large rows assert
/// on terminal remote / file_state state to avoid any broadcast lag.
async fn run_cycle_capture_progress(orch: &SyncOrchestrator) -> driven_core::types::ExecProgress {
    let mut rx = orch.subscribe();
    orch.run_cycle(TickSource::Manual).await.unwrap();
    let mut agg = driven_core::types::ExecProgress::zero();
    while let Ok(ev) = rx.try_recv() {
        if let OrchestratorEvent::Progress { progress, .. } = ev {
            agg.files_done = agg.files_done.max(progress.files_done);
            agg.trashes_done = agg.trashes_done.max(progress.trashes_done);
            agg.bytes_done = agg.bytes_done.max(progress.bytes_done);
            agg.errors = agg.errors.max(progress.errors);
        }
    }
    agg
}

/// Count non-trashed objects under a folder.
async fn live_object_count(remote: &InMemoryRemoteStore, folder_id: &str) -> usize {
    remote
        .list_folder(folder_id)
        .await
        .unwrap()
        .iter()
        .filter(|e| !e.trashed)
        .count()
}

// ---------------------------------------------------------------------------
// Row: fresh sync of 100 files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fresh_sync_of_100_files() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    for i in 0..100u32 {
        write_file(
            src_dir.path(),
            &format!("f{i:03}.txt"),
            format!("body-{i}").as_bytes(),
        );
    }
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();

    let orch = orchestrator(
        account,
        state.clone(),
        remote.clone(),
        Arc::new(FakePowerSource::new(power_on_ac())),
        Arc::new(FakeNetProbe::online()),
        Arc::new(FakeClock::new()),
        OrchestratorConfig::default(),
    );

    orch.run_cycle(TickSource::Scheduled).await.unwrap();

    // Terminal state, not drained events (the broadcast channel can lag at 100).
    assert_eq!(
        live_object_count(&remote, &folder).await,
        100,
        "all 100 files uploaded"
    );
    // file_state populated for every file -> a no-op second cycle.
    let progress = run_cycle_capture_progress(&orch).await;
    assert_eq!(
        progress.files_done, 0,
        "second cycle re-uploads nothing (file_state populated, no errors)"
    );
    assert_eq!(progress.errors, 0, "no errors on the steady-state cycle");
    assert!(matches!(orch.state().await, OrchestratorState::Idle { .. }));
}

// ---------------------------------------------------------------------------
// Row: sync, change 5 files, sync -> only 5 re-uploaded
// ---------------------------------------------------------------------------

#[tokio::test]
async fn change_five_files_reuploads_only_five() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    for i in 0..20u32 {
        write_file(
            src_dir.path(),
            &format!("f{i:02}.txt"),
            format!("v1-{i}").as_bytes(),
        );
    }
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    let clock = Arc::new(FakeClock::new());
    let orch = orchestrator(
        account,
        state.clone(),
        remote.clone(),
        Arc::new(FakePowerSource::new(power_on_ac())),
        Arc::new(FakeNetProbe::online()),
        clock.clone(),
        OrchestratorConfig::default(),
    );

    orch.run_cycle(TickSource::Scheduled).await.unwrap();
    assert_eq!(live_object_count(&remote, &folder).await, 20);

    // Change exactly 5 files - vary the SIZE (not just content) so the scanner's
    // (size, mtime) fast-path detects them deterministically regardless of
    // filesystem mtime granularity.
    for i in 0..5u32 {
        write_file(
            src_dir.path(),
            &format!("f{i:02}.txt"),
            format!("v2-changed-and-longer-{i}").as_bytes(),
        );
    }

    let progress = run_cycle_capture_progress(&orch).await;
    assert_eq!(
        progress.files_done, 5,
        "exactly the 5 changed files re-uploaded"
    );
    assert_eq!(progress.errors, 0);
    // Still 20 distinct objects (changes are updates, not new creates).
    assert_eq!(live_object_count(&remote, &folder).await, 20);
}

// ---------------------------------------------------------------------------
// Row: sync, delete 3 files, sync -> 3 trashed on remote
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_three_files_trashes_three() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    for i in 0..10u32 {
        write_file(
            src_dir.path(),
            &format!("f{i}.txt"),
            format!("body-{i}").as_bytes(),
        );
    }
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    let orch = orchestrator(
        account,
        state.clone(),
        remote.clone(),
        Arc::new(FakePowerSource::new(power_on_ac())),
        Arc::new(FakeNetProbe::online()),
        Arc::new(FakeClock::new()),
        OrchestratorConfig::default(),
    );

    orch.run_cycle(TickSource::Scheduled).await.unwrap();
    assert_eq!(live_object_count(&remote, &folder).await, 10);

    // Delete 3 files locally.
    for i in 0..3u32 {
        std::fs::remove_file(src_dir.path().join(format!("f{i}.txt"))).unwrap();
    }

    let progress = run_cycle_capture_progress(&orch).await;
    assert_eq!(progress.trashes_done, 3, "exactly 3 remote objects trashed");
    assert_eq!(progress.errors, 0);
    assert_eq!(
        live_object_count(&remote, &folder).await,
        7,
        "7 live objects remain; 3 trashed"
    );
    // And the 3 are present-but-trashed, not deleted outright.
    let with_trashed = remote.list_folder_with_trashed(&folder);
    assert_eq!(with_trashed.iter().filter(|e| e.trashed).count(), 3);
}

// ---------------------------------------------------------------------------
// Row: 429 on the 7th file -> executor retries with backoff, sync completes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rate_limit_on_seventh_file_retries_and_completes() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    // Trip a single 429 after 6 successful requests; the 7th request 429s once
    // then succeeds on retry (with_rate_limit_after stores n+1 internally).
    let remote = Arc::new(InMemoryRemoteStore::new().with_rate_limit_after(6));
    let folder = remote.root_id().to_string();

    for i in 0..10u32 {
        write_file(
            src_dir.path(),
            &format!("f{i}.txt"),
            format!("body-{i}").as_bytes(),
        );
    }
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();

    let clock = Arc::new(FakeClock::new());
    let pacer = test_pacer(clock.clone());
    let exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer,
            crypto: None,
            vss: None,
            network: None,
        },
        clock.clone(),
    );

    // Build the upload plan over the 10 files (single-threaded order so "the
    // 7th" is meaningful).
    let mut ops = Vec::new();
    for i in 0..10u32 {
        let rel = RelativePath::try_from(format!("f{i}.txt")).unwrap();
        let size = std::fs::metadata(src_dir.path().join(format!("f{i}.txt")))
            .unwrap()
            .len();
        ops.push(Op::HashThenUpload {
            source_id: src.id,
            relative_path: rel,
            size,
        });
    }
    let plan = Plan {
        ops,
        collisions: vec![],
    };

    let out = exec
        .execute(&src, &plan, &noop_progress, &noop_outcome)
        .await
        .unwrap();
    assert_eq!(out.len(), 10);
    assert!(
        out.iter().all(|o| matches!(o, OpOutcome::Done { .. })),
        "every op completes despite the 429 retry: {out:?}"
    );
    assert_eq!(live_object_count(&remote, &folder).await, 10);
}

// ---------------------------------------------------------------------------
// Row: crash mid-upload -> next sync recovers. BOTH recovery mechanisms exist
//      per DESIGN s5.4 (byte-level resumable resume) + s5.6 (client_op_uuid
//      orphan adoption), and are covered by the two tests below.
// ---------------------------------------------------------------------------
//
// 1. ADOPT (DESIGN s5.6): the object FINALIZED on Drive (UUID stamped in
//    appProperties atomically with the bytes) but the local
//    `commit_create_result` was lost before it ran. Reconcile finds the orphan
//    via `find_by_op_uuid`, re-hashes the current local file against the
//    uploaded blake3 (P1-2), and adopts the SAME object id - no duplicate.
//
// 2. RESUME (DESIGN s5.4): the crash happened MID-upload - a resumable session
//    was open with some chunks acked but the object NOT yet finalized. The
//    session (url/issued_at/total/kind/last-acked offset) was persisted in
//    `pending_ops.payload_json`; reconcile resumes it BYTE-FOR-BYTE from the
//    persisted offset (P1-3) rather than re-uploading from zero. Because the
//    fake rejects any `resume_chunk` whose offset != bytes-already-received,
//    completing the upload through the SAME session is itself proof the resume
//    started from the persisted offset, not from zero.

#[tokio::test]
async fn crash_mid_upload_adopts_orphan_without_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    // >= 5 MiB so phase 1 exercises the resumable Create path (which stamps the
    // same `appProperties` the small/simple path does). The recovery path is
    // size-independent; the large file just preserves the "resumable" flavor.
    let big = vec![0x5Au8; 6 * 1024 * 1024];
    write_file(src_dir.path(), "big.bin", &big);
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();

    let rel = RelativePath::try_from("big.bin".to_string()).unwrap();
    let plan = Plan {
        ops: vec![Op::HashThenUpload {
            source_id: src.id,
            relative_path: rel.clone(),
            size: big.len() as u64,
        }],
        collisions: vec![],
    };

    // --- phase 1: a normal, completed upload --------------------------------
    let clock = Arc::new(FakeClock::new());
    let exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer: test_pacer(clock.clone()),
            crypto: None,
            vss: None,
            network: None,
        },
        clock.clone(),
    );
    let out = exec
        .execute(&src, &plan, &noop_progress, &noop_outcome)
        .await
        .unwrap();
    assert!(out.iter().all(|o| matches!(o, OpOutcome::Done { .. })));

    // The object landed with its create-op UUID stamped in appProperties
    // (DESIGN s5.6 step 2). Pull it back from the remote - this self-validates
    // the executor's stamping.
    let children = remote.list_folder(&folder).await.unwrap();
    assert_eq!(
        children.len(),
        1,
        "exactly one object after the first upload"
    );
    let op_uuid = children[0]
        .app_properties
        .get(CLIENT_OP_UUID_KEY)
        .cloned()
        .expect("the uploaded object carries its client_op_uuid in appProperties");
    assert!(state
        .get_pending_ops_for_source(src.id)
        .await
        .unwrap()
        .is_empty());

    // --- simulate the crash: the bytes committed remotely but the local commit
    //     was lost. We roll local state back to "object uploaded, commit not yet
    //     run": drop the file_state row and re-enqueue the create pending_op
    //     carrying the same UUID (drive_file_id=null => the create reconcile
    //     path, which finds the orphan by UUID).
    state.delete_file_state(src.id, &rel).await.unwrap();
    let now = clock.now_ms();
    // P1-2: the op records the blake3 (over plaintext) of what it uploaded so
    // adoption can re-hash the current local file and prove it still matches.
    let uploaded_hex = hex::encode(blake3::hash(&big).as_bytes());
    state
        .enqueue_pending_op(NewPendingOp {
            source_id: src.id,
            op_type: "upload".to_string(),
            relative_path: rel.clone(),
            payload_json: serde_json::json!({
                "client_op_uuid": op_uuid,
                "drive_file_id": serde_json::Value::Null,
                "uploaded_blake3_hex": uploaded_hex,
            }),
            scheduled_for: now,
            created_at: now,
        })
        .await
        .unwrap();

    // --- phase 2: a fresh executor over the SAME state + remote reconciles ---
    let exec2 = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer: test_pacer(clock.clone()),
            crypto: None,
            vss: None,
            network: None,
        },
        clock.clone(),
    );
    exec2.reconcile(&src).await.unwrap();

    // The orphan was adopted, NOT re-uploaded: still exactly one object.
    assert_eq!(
        live_object_count(&remote, &folder).await,
        1,
        "reconcile adopts the orphan; no duplicate object"
    );
    // The pending op drained and the file_state row was re-created (Synced).
    assert!(state
        .get_pending_ops_for_source(src.id)
        .await
        .unwrap()
        .is_empty());
    let fs = state.get_file_state(src.id, &rel).await.unwrap();
    let fs = fs.expect("file_state restored by adoption");
    assert_eq!(
        fs.drive_file_id.as_deref(),
        Some(children[0].id.as_str()),
        "adopted the SAME remote object id, not a fresh upload"
    );

    // --- and a subsequent real sync cycle does not duplicate either ---------
    // The next planner pass sees a Synced file_state and issues zero ops.
    let orch = orchestrator(
        account,
        state.clone(),
        remote.clone(),
        Arc::new(FakePowerSource::new(power_on_ac())),
        Arc::new(FakeNetProbe::online()),
        clock.clone(),
        OrchestratorConfig::default(),
    );
    let progress = run_cycle_capture_progress(&orch).await;
    assert_eq!(progress.files_done, 0, "steady-state: nothing re-uploaded");
    assert_eq!(progress.errors, 0);
    assert_eq!(
        live_object_count(&remote, &folder).await,
        1,
        "still exactly one object after a full follow-up cycle"
    );
}

// ---------------------------------------------------------------------------
// Row: crash MID-upload (object NOT finalized) -> next sync RESUMES the
//      persisted resumable session BYTE-FOR-BYTE from the last-acked offset
//      (DESIGN s5.4 / s5.6, P1-2 / P1-3). Distinct from the adopt-orphan row.
// ---------------------------------------------------------------------------
//
// P1-2: this test exercises the REAL executor's streaming-resumable path -
// there is NO manually-seeded `uploaded_blake3_hex`. Phase 1 runs the actual
// `execute()` against a remote rigged to DROP the 3rd request (the 2nd wire
// chunk's `resume_chunk`): the session opens (req 1), the first wire chunk
// acks + the executor persists `acked_offset` + the resume IDENTITY into
// `pending_ops.payload_json` (req 2), then the next chunk drops (req 3). The
// streaming pipeline produces the plaintext blake3 only DURING the upload, so
// the persisted op carries NO content hash - exactly the mid-stream-crash
// state. The drop surfaces as a fatal upload error, so the op is KEPT (never
// deleted) with the acked offset + identity but no hash.
//
// Phase 2: a FRESH executor over the same state + remote reconciles. The ONLY
// way it can complete is to validate the identity, re-read the file, and push
// the remaining bytes from the persisted offset through the SAME session - the
// fake rejects any chunk whose offset != bytes-received, so a from-zero retry
// would be `SessionInvalid`. Completion at the full byte count is therefore
// proof of byte-level resume driven entirely by the executor (no hash seed).
//
// This test goes RED on the pre-fix executor (which refuses to resume without
// a persisted hash and falls through to `find_by_op_uuid`, finds nothing - the
// create never finalized - and drops the op) and GREEN only once resume
// validates the persisted identity + offset and re-hashes the full stream.

#[tokio::test]
async fn crash_mid_upload_resumes_persisted_session_byte_for_byte() {
    const CHUNK_256K: usize = 256 * 1024;
    const WIRE_CHUNK: usize = 4 * 1024 * 1024;

    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    // Drop the 3rd request: req1 = resumable_session open, req2 = first wire
    // chunk (acks + persists offset/identity), req3 = second wire chunk DROPS.
    let remote = Arc::new(InMemoryRemoteStore::new().with_network_drop_after(2));
    let folder = remote.root_id().to_string();

    // A ~20 MiB file (>= RESUMABLE_THRESHOLD 5 MiB and >= PIPELINE_THRESHOLD
    // 4 MiB so it runs the STREAMING resumable path) large enough that the
    // drain loop flushes at least two 4 MiB wire chunks before EOF (the drain
    // holds back until acc >= 2 * WIRE_CHUNK), so the first chunk acks an
    // offset and the SECOND chunk is the one the drop lands on.
    let total_len = 5 * WIRE_CHUNK + 3 * CHUNK_256K + 17;
    let big: Vec<u8> = (0..total_len).map(|i| (i % 251) as u8).collect();
    write_file(src_dir.path(), "resume.bin", &big);
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    let rel = RelativePath::try_from("resume.bin".to_string()).unwrap();

    let clock = Arc::new(FakeClock::new());

    // --- phase 1: a REAL upload that crashes mid-stream ---------------------
    let exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer: test_pacer(clock.clone()),
            crypto: None,
            vss: None,
            network: None,
        },
        clock.clone(),
    );
    let plan = Plan {
        ops: vec![Op::HashThenUpload {
            source_id: src.id,
            relative_path: rel.clone(),
            size: total_len as u64,
        }],
        collisions: vec![],
    };
    // The dropped chunk is a fatal Drive error -> `execute` returns Err and the
    // pending op is KEPT (the SkipPostUpload/Fatal contract never deletes a
    // create op mid-stream).
    let phase1 = exec
        .execute(&src, &plan, &noop_progress, &noop_outcome)
        .await;
    assert!(
        phase1.is_err(),
        "the mid-stream network drop aborts the upload: {phase1:?}"
    );
    // The object has NOT materialized (a resumable Create commits only on its
    // final chunk, which never landed).
    assert_eq!(
        live_object_count(&remote, &folder).await,
        0,
        "no object until the resumable Create finalizes"
    );

    // The executor persisted an op carrying the live session + a NON-zero
    // acked offset + the resume IDENTITY, and critically NO content hash (the
    // streaming hash is only known at the end). We assert the payload shape
    // straight from the DB so the test proves the real executor wrote it.
    let pending = state.get_pending_ops_for_source(src.id).await.unwrap();
    assert_eq!(pending.len(), 1, "the create op survived the crash");
    let payload = &pending[0].payload_json;
    assert!(
        payload
            .get("uploaded_blake3_hex")
            .map(|v| v.is_null())
            .unwrap_or(true),
        "NO content hash was seeded (this is the mid-stream-crash invariant): {payload}"
    );
    let acked = payload
        .pointer("/resumable/acked_offset")
        .and_then(|v| v.as_u64())
        .expect("the executor persisted a resumable acked_offset");
    assert!(
        acked >= WIRE_CHUNK as u64,
        "at least the first wire chunk was acked before the drop; got {acked}"
    );
    assert!(
        acked < total_len as u64,
        "but NOT the whole file (it crashed mid-stream); got {acked}"
    );
    assert!(
        payload.get("resume_identity").is_some_and(|v| !v.is_null()),
        "the executor persisted a resume identity: {payload}"
    );

    // --- phase 2: a fresh executor reconciles -> resumes the session --------
    // The network drop was single-shot, so phase 2's requests all succeed.
    let exec2 = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer: test_pacer(clock.clone()),
            crypto: None,
            vss: None,
            network: None,
        },
        clock.clone(),
    );
    exec2.reconcile(&src).await.unwrap();

    // The upload completed via byte-level resume: exactly one object, and it
    // carries the full byte count (proving the resumed tail bytes landed, NOT a
    // truncated or from-zero re-do).
    let children = remote.list_folder(&folder).await.unwrap();
    assert_eq!(children.len(), 1, "resume finalized exactly one object");
    assert_eq!(
        children[0].size,
        Some(total_len as u64),
        "resumed object has the full size (tail bytes landed via resume)"
    );
    // No session leaked (completed sessions are removed).
    assert_eq!(remote.open_session_count(), 0, "session consumed by resume");

    // file_state committed Synced with the REAL plaintext hash (re-derived by
    // the resume over the full stream, NOT seeded), op drained.
    let fs = state
        .get_file_state(src.id, &rel)
        .await
        .unwrap()
        .expect("file_state committed by resume");
    assert_eq!(
        fs.drive_file_id.as_deref(),
        Some(children[0].id.as_str()),
        "committed the resumed object id"
    );
    assert_eq!(
        fs.hash_blake3,
        *blake3::hash(&big).as_bytes(),
        "file_state carries the real plaintext blake3 from the resumed stream"
    );
    assert!(state
        .get_pending_ops_for_source(src.id)
        .await
        .unwrap()
        .is_empty());

    // A follow-up steady-state cycle re-uploads nothing and does not duplicate.
    let orch = orchestrator(
        account,
        state.clone(),
        remote.clone(),
        Arc::new(FakePowerSource::new(power_on_ac())),
        Arc::new(FakeNetProbe::online()),
        clock.clone(),
        OrchestratorConfig::default(),
    );
    let progress = run_cycle_capture_progress(&orch).await;
    assert_eq!(progress.files_done, 0, "steady-state: nothing re-uploaded");
    assert_eq!(
        live_object_count(&remote, &folder).await,
        1,
        "still exactly one object after the resume + a full follow-up cycle"
    );
}

// ---------------------------------------------------------------------------
// Row: crash-mid-upload orphan whose LOCAL bytes changed before recovery ->
//      reconcile requeues + the NEXT full sync cycle re-uploads the NEW bytes
//      as an UPDATE (no duplicate). This is the end-to-end proof of P1-2: it
//      runs the real scan -> plan -> execute pipeline so a requeue row that
//      the FastPath scanner would treat as "unchanged" (the data-loss trap)
//      fails loudly here.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconcile_requeue_reuploads_changed_bytes_on_next_cycle() {
    use tokio::io::AsyncReadExt;

    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    let old_bytes = b"OLD uploaded bytes".to_vec();
    let new_bytes = b"NEW locally-edited bytes - different length".to_vec();

    // The orphan landed on Drive with the OLD bytes + its client_op_uuid.
    let op_uuid = uuid::Uuid::new_v4().to_string();
    let mut app = std::collections::HashMap::new();
    app.insert(CLIENT_OP_UUID_KEY.to_string(), op_uuid.clone());
    let created = remote
        .create(
            &folder,
            "drift.bin",
            "application/octet-stream",
            UploadBody::Bytes(bytes::Bytes::from(old_bytes.clone())),
            app,
        )
        .await
        .unwrap();

    // But locally the file now holds the NEW bytes (edited after the upload,
    // before the lost commit).
    write_file(src_dir.path(), "drift.bin", &new_bytes);
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    let rel = RelativePath::try_from("drift.bin".to_string()).unwrap();

    let clock = Arc::new(FakeClock::new());
    // The op recorded the OLD bytes' hash; reconcile re-hashes the NEW local
    // file, sees a mismatch, and requeues.
    let uploaded_hex = hex::encode(blake3::hash(&old_bytes).as_bytes());
    let now = clock.now_ms();
    state
        .enqueue_pending_op(NewPendingOp {
            source_id: src.id,
            op_type: "upload".to_string(),
            relative_path: rel.clone(),
            payload_json: serde_json::json!({
                "client_op_uuid": op_uuid,
                "drive_file_id": serde_json::Value::Null,
                "uploaded_blake3_hex": uploaded_hex,
            }),
            scheduled_for: now,
            created_at: now,
        })
        .await
        .unwrap();

    // Phase 1: reconcile requeues the orphan (no duplicate, not Synced).
    let exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer: test_pacer(clock.clone()),
            crypto: None,
            vss: None,
            network: None,
        },
        clock.clone(),
    );
    exec.reconcile(&src).await.unwrap();
    assert_eq!(
        live_object_count(&remote, &folder).await,
        1,
        "reconcile produced no duplicate"
    );

    // Phase 2: a full sync cycle MUST re-upload the NEW bytes. This is the
    // discriminating check - a requeue row stamped with the current local
    // identity would be skipped by the FastPath scanner and files_done would
    // be 0 with Drive still holding the OLD bytes.
    let orch = orchestrator(
        account,
        state.clone(),
        remote.clone(),
        Arc::new(FakePowerSource::new(power_on_ac())),
        Arc::new(FakeNetProbe::online()),
        clock.clone(),
        OrchestratorConfig::default(),
    );
    let progress = run_cycle_capture_progress(&orch).await;
    assert_eq!(
        progress.files_done, 1,
        "the changed file must be re-uploaded on the next cycle"
    );
    assert_eq!(progress.errors, 0);

    // Still exactly one object (UPDATE against the same id, no duplicate)...
    let children = remote.list_folder(&folder).await.unwrap();
    assert_eq!(children.len(), 1, "re-upload was an UPDATE, not a CREATE");
    assert_eq!(
        children[0].id, created.id,
        "the SAME drive object id was updated"
    );
    // ...and its bytes are now the NEW content, not the stale OLD bytes.
    let mut blob = Vec::new();
    remote
        .download(&children[0].id)
        .await
        .unwrap()
        .0
        .read_to_end(&mut blob)
        .await
        .unwrap();
    assert_eq!(blob, new_bytes, "Drive now holds the re-uploaded NEW bytes");

    // And the file is finally Synced with the NEW plaintext hash.
    let fs = state
        .get_file_state(src.id, &rel)
        .await
        .unwrap()
        .expect("file_state after re-upload");
    assert_eq!(
        fs.hash_blake3,
        *blake3::hash(&new_bytes).as_bytes(),
        "file_state carries the NEW bytes' hash after re-upload"
    );
}

// ---------------------------------------------------------------------------
// Row: parallel uploads (concurrency) -> no remote state corruption
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parallel_uploads_no_corruption() {
    // The upload pool size is `default_pool_size()` = min(num_cpus*2, 16); there
    // is no public setter to force literally 4. On a multicore CI runner this is
    // >= 4, so this exercises the same ">1 concurrent uploads, no corruption"
    // property the ROADMAP's "concurrency=4" row asks for (see report).
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    let mut expected = Vec::new();
    let mut ops = Vec::new();
    for i in 0..40u32 {
        let name = format!("p{i:02}.bin");
        let contents = format!("payload-{i}-{}", "x".repeat(i as usize)).into_bytes();
        write_file(src_dir.path(), &name, &contents);
        let rel = RelativePath::try_from(name.clone()).unwrap();
        ops.push(Op::HashThenUpload {
            source_id: driven_core::types::SourceId::new_v4(),
            relative_path: rel,
            size: contents.len() as u64,
        });
        expected.push((name, contents));
    }
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    // Re-point the ops' source id to the real source.
    let ops: Vec<Op> = ops
        .into_iter()
        .map(|op| match op {
            Op::HashThenUpload {
                relative_path,
                size,
                ..
            } => Op::HashThenUpload {
                source_id: src.id,
                relative_path,
                size,
            },
            other => other,
        })
        .collect();
    let plan = Plan {
        ops,
        collisions: vec![],
    };

    let clock = Arc::new(FakeClock::new());
    let exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer: test_pacer(clock.clone()),
            crypto: None,
            vss: None,
            network: None,
        },
        clock,
    );
    let out = exec
        .execute(&src, &plan, &noop_progress, &noop_outcome)
        .await
        .unwrap();
    assert_eq!(out.len(), 40);
    assert!(out.iter().all(|o| matches!(o, OpOutcome::Done { .. })));

    // Every object present with the right size, none duplicated/missing.
    let children = remote.list_folder(&folder).await.unwrap();
    assert_eq!(children.len(), 40, "no missing/duplicate objects");
    for (name, contents) in &expected {
        let entry = children.iter().find(|e| &e.name == name).expect("present");
        assert_eq!(entry.size, Some(contents.len() as u64));
    }
}

// ---------------------------------------------------------------------------
// Row: power gate -> on battery -> orchestrator transitions to Paused
// ---------------------------------------------------------------------------

#[tokio::test]
async fn power_gate_on_battery_pauses() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();
    write_file(src_dir.path(), "a.txt", b"work to do");
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();

    let power = Arc::new(FakePowerSource::new(power_on_battery()));
    let orch = orchestrator(
        account,
        state.clone(),
        remote.clone(),
        power.clone(),
        Arc::new(FakeNetProbe::online()),
        Arc::new(FakeClock::new()),
        OrchestratorConfig::default(), // skip_on_battery defaults to true
    );

    orch.run_cycle(TickSource::Scheduled).await.unwrap();
    assert_eq!(
        orch.state().await,
        OrchestratorState::Paused {
            reason: driven_core::types::PauseReason::Battery
        }
    );
    // Gate closed -> zero objects uploaded.
    assert_eq!(live_object_count(&remote, &folder).await, 0);

    // AC restored -> the next cycle proceeds and uploads.
    power.set(power_on_ac());
    orch.run_cycle(TickSource::Scheduled).await.unwrap();
    assert!(matches!(orch.state().await, OrchestratorState::Idle { .. }));
    assert_eq!(live_object_count(&remote, &folder).await, 1);
}

// ---------------------------------------------------------------------------
// Row: dry-run -> plan computed, zero remote calls executed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dry_run_plans_with_zero_remote_calls() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();
    for i in 0..5u32 {
        write_file(src_dir.path(), &format!("f{i}.txt"), b"content");
    }
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();

    let cfg = OrchestratorConfig {
        dry_run: true,
        ..OrchestratorConfig::default()
    };
    let orch = orchestrator(
        account,
        state.clone(),
        remote.clone(),
        Arc::new(FakePowerSource::new(power_on_ac())),
        Arc::new(FakeNetProbe::online()),
        Arc::new(FakeClock::new()),
        cfg,
    );

    orch.run_cycle(TickSource::Scheduled).await.unwrap();
    assert_eq!(
        live_object_count(&remote, &folder).await,
        0,
        "dry-run issues zero remote uploads"
    );
    assert!(matches!(orch.state().await, OrchestratorState::Idle { .. }));
}

// ---------------------------------------------------------------------------
// Row: encryption ON + sync round-trip + restore via direct API -> bytes match
// ---------------------------------------------------------------------------

#[tokio::test]
async fn encryption_on_round_trip_bytes_match() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    // Single-chunk plaintext (< 64 KiB READ_BUF) so the on-disk blob is exactly
    // `header || finalize_last(ciphertext)`.
    let plaintext = b"the quick brown fox encrypts over the lazy dog".to_vec();
    write_file(src_dir.path(), "secret.txt", &plaintext);
    // M5 per-source crypto FAILS CLOSED on `encryption_enabled`: a wired suite
    // only encrypts when the SourceRow says the source is encrypted. Flip it on
    // so the executor takes the ciphertext path this test asserts.
    let src = SourceRow {
        encryption_enabled: true,
        ..source_in(account, src_dir.path(), &folder)
    };
    state.upsert_source(&src).await.unwrap();

    // Wire the executor with a real per-source crypto suite.
    let source_key = SourceKey::generate();
    let suite: Arc<dyn driven_crypto::SourceCryptoSuite> =
        Arc::new(DrivenCryptoSuite::new(source_key.clone()));
    let clock = Arc::new(FakeClock::new());
    let exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer: test_pacer(clock.clone()),
            // M5: ExecutorDeps.crypto is a per-source CryptoProvider; wrap the
            // single test suite (one suite for every source) to preserve the
            // pre-M5 executor-wide encryption behaviour this test asserts.
            crypto: Some(
                Arc::new(driven_core::crypto_provider::SingleSuiteProvider::new(
                    suite,
                )) as Arc<dyn driven_core::crypto_provider::CryptoProvider>,
            ),
            vss: None,
            network: None,
        },
        clock,
    );

    let rel = RelativePath::try_from("secret.txt".to_string()).unwrap();
    let plan = Plan {
        ops: vec![Op::HashThenUpload {
            source_id: src.id,
            relative_path: rel,
            size: plaintext.len() as u64,
        }],
        collisions: vec![],
    };
    let out = exec
        .execute(&src, &plan, &noop_progress, &noop_outcome)
        .await
        .unwrap();
    assert!(out.iter().all(|o| matches!(o, OpOutcome::Done { .. })));

    // The stored object is ciphertext, NOT plaintext.
    let children = remote.list_folder(&folder).await.unwrap();
    assert_eq!(children.len(), 1);
    let object_id = children[0].id.clone();
    let mut blob = Vec::new();
    remote
        .download(&object_id)
        .await
        .unwrap()
        .0
        .read_to_end(&mut blob)
        .await
        .unwrap();
    assert_ne!(
        blob, plaintext,
        "stored bytes must be encrypted, not plaintext"
    );
    assert!(blob.len() >= HEADER_LEN);

    // Restore via the direct crypto API (the recovery path): a fresh suite over
    // the same source key derives the decryptor from the stored header, then
    // decrypts the single final chunk. Asserts the bytes match.
    let header = &blob[..HEADER_LEN];
    let ciphertext = &blob[HEADER_LEN..];
    let restore_suite = DrivenCryptoSuite::new(source_key);
    let dec: Box<dyn ContentDecryptor> = restore_suite.content_decryptor(header).unwrap();
    let restored = dec.decrypt_last(ciphertext).unwrap();
    assert_eq!(
        restored.as_ref(),
        plaintext.as_slice(),
        "decrypted bytes match the original plaintext"
    );
}

// ---------------------------------------------------------------------------
// Row: encryption ON + NESTED path -> the REMOTE filenames are CIPHERTEXT
//      (folders AND leaf), the file lands under the encrypted path, and a
//      full restore (decrypt_filename per component + decrypt content)
//      recovers the original path + bytes. This is the P1-5 deliverable: it
//      proves filenames do not LEAK (DESIGN s7) AND, because the file is
//      > PIPELINE_THRESHOLD, exercises the encrypted STREAMING pipeline + the
//      up-front length prediction against real crypto framing at the same
//      time (the flat row above only covers the inline path).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn encryption_nested_remote_is_ciphertext_and_restores() {
    use driven_crypto::SourceCryptoSuite as _;

    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    // A nested path so encrypt_filename's parent-AAD chaining + ensure_folder
    // of the encrypted parents both run. > PIPELINE_THRESHOLD so it streams.
    let rel_str = "docs/private/big-secret.bin";
    let plaintext: Vec<u8> = (0..(5 * 1024 * 1024usize))
        .map(|i| (i % 247) as u8)
        .collect();
    write_file(src_dir.path(), rel_str, &plaintext);
    // M5 per-source crypto FAILS CLOSED on `encryption_enabled` - flip it on so
    // the wired suite actually encrypts (else the executor uploads plaintext).
    let src = SourceRow {
        encryption_enabled: true,
        ..source_in(account, src_dir.path(), &folder)
    };
    state.upsert_source(&src).await.unwrap();

    let source_key = SourceKey::generate();
    let suite: Arc<dyn driven_crypto::SourceCryptoSuite> =
        Arc::new(DrivenCryptoSuite::new(source_key.clone()));
    let clock = Arc::new(FakeClock::new());
    let exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer: test_pacer(clock.clone()),
            // M5: ExecutorDeps.crypto is a per-source CryptoProvider; wrap the
            // single test suite (one suite for every source) to preserve the
            // pre-M5 executor-wide encryption behaviour this test asserts.
            crypto: Some(
                Arc::new(driven_core::crypto_provider::SingleSuiteProvider::new(
                    suite,
                )) as Arc<dyn driven_core::crypto_provider::CryptoProvider>,
            ),
            vss: None,
            network: None,
        },
        clock,
    );

    let rel = RelativePath::try_from(rel_str.to_string()).unwrap();
    let plan = Plan {
        ops: vec![Op::HashThenUpload {
            source_id: src.id,
            relative_path: rel.clone(),
            size: plaintext.len() as u64,
        }],
        collisions: vec![],
    };
    let out = exec
        .execute(&src, &plan, &noop_progress, &noop_outcome)
        .await
        .unwrap();
    assert!(
        out.iter().all(|o| matches!(o, OpOutcome::Done { .. })),
        "got {out:?}"
    );

    // The source ROOT must contain a CIPHERTEXT folder named neither "docs"
    // nor anything plaintext - the first encrypted path component.
    let root_children = remote.list_folder(&folder).await.unwrap();
    assert_eq!(
        root_children.len(),
        1,
        "exactly one top-level encrypted folder"
    );
    let lvl1 = &root_children[0];
    assert_ne!(
        lvl1.name, "docs",
        "top folder name must be ciphertext, not 'docs'"
    );

    // Restore the WHOLE path from ciphertext: decrypt each component's name
    // with its parent's ciphertext as AAD (DESIGN s7.3), proving both that
    // the names are real ciphertext and that nothing plaintext leaked.
    let restore = DrivenCryptoSuite::new(source_key);
    let d1 = restore.decrypt_filename(&lvl1.name, &[]).unwrap();
    assert_eq!(d1, "docs", "level-1 ciphertext decrypts to 'docs'");

    let lvl2_children = remote.list_folder(&lvl1.id).await.unwrap();
    assert_eq!(lvl2_children.len(), 1);
    let lvl2 = &lvl2_children[0];
    assert_ne!(lvl2.name, "private");
    let d2 = restore
        .decrypt_filename(&lvl2.name, lvl1.name.as_bytes())
        .unwrap();
    assert_eq!(d2, "private", "level-2 ciphertext decrypts to 'private'");

    let leaf_children = remote.list_folder(&lvl2.id).await.unwrap();
    assert_eq!(leaf_children.len(), 1);
    let leaf = &leaf_children[0];
    assert_ne!(leaf.name, "big-secret.bin", "leaf name must be ciphertext");
    let d_leaf = restore
        .decrypt_filename(&leaf.name, lvl2.name.as_bytes())
        .unwrap();
    assert_eq!(
        d_leaf, "big-secret.bin",
        "leaf ciphertext decrypts to the real name"
    );

    // The reconstructed plaintext path equals the original.
    assert_eq!(format!("{d1}/{d2}/{d_leaf}"), rel_str);

    // file_state persisted the encrypted_remote_path (the slash-joined
    // ciphertext path) + the plaintext blake3.
    let row = state.get_file_state(src.id, &rel).await.unwrap().unwrap();
    assert_eq!(
        row.encrypted_remote_path.as_deref(),
        Some(format!("{}/{}/{}", lvl1.name, lvl2.name, leaf.name).as_str()),
        "encrypted_remote_path is the slash-joined ciphertext path"
    );
    assert_eq!(row.hash_blake3, *blake3::hash(&plaintext).as_bytes());

    // Restore the CONTENT: download the ciphertext blob + decrypt -> original.
    let mut blob = Vec::new();
    remote
        .download(&leaf.id)
        .await
        .unwrap()
        .0
        .read_to_end(&mut blob)
        .await
        .unwrap();
    assert_ne!(blob, plaintext, "stored content must be encrypted");
    let mut dec: Box<dyn ContentDecryptor> =
        restore.content_decryptor(&blob[..HEADER_LEN]).unwrap();
    let ct_chunk = 64 * 1024 + 16; // 64 KiB plaintext chunk + Poly1305 tag
    let body = &blob[HEADER_LEN..];
    let mut off = 0;
    let mut restored = Vec::new();
    while body.len() - off > ct_chunk {
        restored.extend_from_slice(&dec.decrypt_chunk(&body[off..off + ct_chunk]).unwrap());
        off += ct_chunk;
    }
    restored.extend_from_slice(&dec.decrypt_last(&body[off..]).unwrap());
    assert_eq!(
        restored, plaintext,
        "decrypted content matches the original bytes"
    );
}

// ---------------------------------------------------------------------------
// Network-resilience matrix (orchestrator-reaction rows; DESIGN s5.8.1)
// ---------------------------------------------------------------------------

/// Helper: build an orchestrator with a file ready to upload and the given
/// network probe, returning (orch, remote, folder).
async fn net_orchestrator(
    net: Arc<dyn NetworkProbe>,
) -> (
    SyncOrchestrator,
    Arc<InMemoryRemoteStore>,
    String,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();
    write_file(src_dir.path(), "a.txt", b"queued work");
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    let orch = orchestrator(
        account,
        state,
        remote.clone(),
        Arc::new(FakePowerSource::new(power_on_ac())),
        net,
        Arc::new(FakeClock::new()),
        OrchestratorConfig::default(),
    );
    (orch, remote, folder, dir, src_dir)
}

#[tokio::test]
async fn network_offline_pauses_no_calls() {
    let net = Arc::new(FakeNetProbe::with_state(NetworkState::Offline));
    let (orch, remote, folder, _d, _s) = net_orchestrator(net).await;
    orch.run_cycle(TickSource::Scheduled).await.unwrap();
    assert_eq!(
        orch.state().await,
        OrchestratorState::Paused {
            reason: driven_core::types::PauseReason::Offline
        }
    );
    assert_eq!(
        live_object_count(&remote, &folder).await,
        0,
        "offline -> no remote upload issued"
    );
}

#[tokio::test]
async fn network_no_internet_pauses_drive_ops() {
    let net = Arc::new(FakeNetProbe::with_state(NetworkState::NoInternet));
    let (orch, remote, folder, _d, _s) = net_orchestrator(net).await;
    orch.run_cycle(TickSource::Scheduled).await.unwrap();
    // A non-Online probe maps to the Offline pause banner (DESIGN s5.8); the
    // observable effect is the same: Drive ops pause.
    assert!(matches!(
        orch.state().await,
        OrchestratorState::Paused { .. }
    ));
    assert_eq!(live_object_count(&remote, &folder).await, 0);
}

#[tokio::test]
async fn network_captive_portal_pauses() {
    let net = Arc::new(FakeNetProbe::with_state(NetworkState::CaptivePortal));
    let (orch, remote, folder, _d, _s) = net_orchestrator(net).await;
    orch.run_cycle(TickSource::Scheduled).await.unwrap();
    assert!(matches!(
        orch.state().await,
        OrchestratorState::Paused { .. }
    ));
    assert_eq!(live_object_count(&remote, &folder).await, 0);
}

#[tokio::test]
async fn network_drive_only_down_backs_off() {
    // Drive's breaker open with a future retry_at -> Backoff, not a full pause:
    // other services are unaffected, Drive ops defer.
    let net = Arc::new(FakeNetProbe::with_drive_open(i64::MAX));
    let (orch, remote, folder, _d, _s) = net_orchestrator(net).await;
    orch.run_cycle(TickSource::Scheduled).await.unwrap();
    assert!(matches!(
        orch.state().await,
        OrchestratorState::Backoff { .. }
    ));
    assert_eq!(
        live_object_count(&remote, &folder).await,
        0,
        "Drive breaker open -> Drive ops deferred"
    );
}

#[tokio::test]
async fn network_updater_down_does_not_block_sync() {
    // The updater being down is not a Drive gate: sync proceeds normally. We
    // model this as "Drive healthy, network online" (the orchestrator does not
    // gate sync on the update endpoint), and assert the cycle completes.
    let net = Arc::new(FakeNetProbe::online());
    let (orch, remote, folder, _d, _s) = net_orchestrator(net).await;
    orch.run_cycle(TickSource::Scheduled).await.unwrap();
    assert!(matches!(orch.state().await, OrchestratorState::Idle { .. }));
    assert_eq!(
        live_object_count(&remote, &folder).await,
        1,
        "updater-down does not pause Drive sync"
    );
}

// ---------------------------------------------------------------------------
// Row (M3.5): VSS unavailable -> locked file skipped + reported, rest sync.
// ---------------------------------------------------------------------------

/// ROADMAP M3.5 degrade acceptance: when VSS is UNAVAILABLE (the
/// `FakeVssProvider::unavailable` stands in for not-elevated / `never` / off
/// Windows), a genuinely locked file is SKIPPED and reported as
/// `local.vss_unavailable` in the activity log (P2-6: distinct from
/// `local.file_locked`, so the user can tell "would back up if Driven ran
/// elevated" from "VSS tried and still failed"), while every other file in the
/// same source still backs up. This is the degrade-gracefully contract, now
/// asserted end-to-end through the real `DefaultExecutor` + `SyncOrchestrator`
/// + `InMemoryRemoteStore`.
///
/// `#[cfg(windows)]` because only Windows produces a real
/// `ERROR_SHARING_VIOLATION` from an exclusive open - but it is NOT
/// elevation-gated: creating the lock needs no privilege (only VSS *snapshot
/// creation* does), so this runs on the non-elevated Windows CI runner and
/// pins the user's core pain-point failure mode. The cross-OS pure decision is
/// covered by `driven_vss::fallback_decision`'s table test.
#[cfg(windows)]
#[tokio::test]
async fn vss_unavailable_skips_locked_file_reports_and_continues() {
    use driven_core::state::{ActivityFilter, PageRequest};
    use std::os::windows::fs::OpenOptionsExt;

    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    // Two files: one we will lock exclusively, one that must still upload.
    write_file(src_dir.path(), "locked.dat", b"held-exclusively");
    write_file(src_dir.path(), "ok.txt", b"this one uploads");
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();

    // Lock locked.dat with NO sharing for the whole cycle.
    const GENERIC_WRITE: u32 = 0x4000_0000;
    let _exclusive = std::fs::OpenOptions::new()
        .access_mode(GENERIC_WRITE)
        .share_mode(0)
        .write(true)
        .open(src_dir.path().join("locked.dat"))
        .expect("lock locked.dat exclusively");

    // VSS unavailable => the locked file degrades to skip.
    let vss: Arc<dyn driven_vss::VssProvider> =
        Arc::new(driven_vss::FakeVssProvider::unavailable());
    let orch = orchestrator_with_vss(
        account,
        state.clone(),
        remote.clone(),
        Arc::new(FakePowerSource::new(power_on_ac())),
        Arc::new(FakeNetProbe::online()),
        Arc::new(FakeClock::new()),
        OrchestratorConfig::default(),
        vss,
    );

    orch.run_cycle(TickSource::Scheduled).await.unwrap();

    // The unlocked file backed up; the locked one did not.
    assert_eq!(
        live_object_count(&remote, &folder).await,
        1,
        "the unlocked file must still upload while the locked one is skipped"
    );

    // P2-6: a local.vss_unavailable activity row was written for the skipped
    // file (VSS was unavailable, so a snapshot was never attempted), DISTINCT
    // from local.file_locked (which would mean VSS was tried and still failed).
    let page = state
        .query_activity(
            ActivityFilter {
                event_types: vec!["local.vss_unavailable".to_string()],
                ..ActivityFilter::default()
            },
            PageRequest::first(50),
        )
        .await
        .unwrap();
    assert!(
        page.rows
            .iter()
            .any(|r| r.event_type == "local.vss_unavailable"),
        "the skipped locked file must surface a local.vss_unavailable activity row; got {:?}",
        page.rows
    );

    // And it must NOT be mislabelled as local.file_locked.
    let locked_page = state
        .query_activity(
            ActivityFilter {
                event_types: vec!["local.file_locked".to_string()],
                ..ActivityFilter::default()
            },
            PageRequest::first(50),
        )
        .await
        .unwrap();
    assert!(
        locked_page.rows.is_empty(),
        "an unavailable-VSS lock must not be reported as local.file_locked; got {:?}",
        locked_page.rows
    );

    // The cycle completed (Idle), not stuck.
    assert!(matches!(orch.state().await, OrchestratorState::Idle { .. }));
}

/// P1-3 (M3.5 codex): a coherent VSS snapshot read must size the upload off the
/// FROZEN snapshot bytes, not the scanner's stale live size. The scanner
/// records the live file's size; by the time the (paranoid `always`-mode)
/// snapshot is read, the live file has a DIFFERENT size, but the snapshot copy
/// is frozen at its own size. Before the fix the upload predicted the live
/// length and the read-stage `read_total != size` check tripped a spurious
/// `ChangedDuringUpload`, skipping exactly the locked-changing file VSS exists
/// to back up. After the fix the upload uses the effective (snapshot) size, so
/// the snapshot bytes upload cleanly and the stored object's size equals the
/// snapshot's, NOT the live file's.
///
/// `#[cfg(windows)]` only because `orchestrator_with_vss` is Windows-gated;
/// the size logic itself is OS-independent (the fake maps to a real directory).
#[cfg(windows)]
#[tokio::test]
async fn vss_frozen_snapshot_uses_effective_size_no_false_changed() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let snap_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    // LIVE file the scanner measures (200 bytes). The SNAPSHOT copy is frozen
    // at a DIFFERENT size (100 bytes) - the coherent point-in-time the shadow
    // copy captured before the live file kept growing.
    let live_bytes = vec![b'L'; 200];
    let snap_bytes = vec![b'S'; 100];
    write_file(src_dir.path(), "db.dat", &live_bytes);
    write_file(snap_dir.path(), "db.dat", &snap_bytes);

    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();

    // Paranoid `always` mode routes EVERY read through the snapshot dir, so the
    // executor reads the frozen 100-byte copy while the scanner recorded 200.
    let vss: Arc<dyn driven_vss::VssProvider> = Arc::new(
        driven_vss::FakeVssProvider::mapped_under(driven_vss::VssMode::Always, snap_dir.path()),
    );
    let config = OrchestratorConfig {
        vss_mode: driven_vss::VssMode::Always,
        ..OrchestratorConfig::default()
    };
    let orch = orchestrator_with_vss(
        account,
        state.clone(),
        remote.clone(),
        Arc::new(FakePowerSource::new(power_on_ac())),
        Arc::new(FakeNetProbe::online()),
        Arc::new(FakeClock::new()),
        config,
        vss,
    );

    orch.run_cycle(TickSource::Scheduled).await.unwrap();

    // The file uploaded (no false ChangedDuringUpload skip) ...
    let entries = remote.list_folder(&folder).await.unwrap();
    let live: Vec<_> = entries.iter().filter(|e| !e.trashed).collect();
    assert_eq!(
        live.len(),
        1,
        "the frozen-snapshot file must upload, not be skipped as changed; got {live:?}"
    );
    // ... and the stored object is the SNAPSHOT's 100 bytes, proving the upload
    // was sized off the effective (snapshot) stat, not the live 200.
    assert_eq!(
        live[0].size,
        Some(snap_bytes.len() as u64),
        "uploaded object must be the snapshot's effective size, not the live size"
    );
    assert_ne!(
        live[0].size,
        Some(live_bytes.len() as u64),
        "uploaded object must NOT be the scanner's stale live size"
    );

    assert!(matches!(orch.state().await, OrchestratorState::Idle { .. }));
}

// ---------------------------------------------------------------------------
// Perf-multiplier benchmark rows (ROADMAP M3 acceptance, tracking issue #28).
//
// These three rows are QUANTITATIVE perf multipliers, not behavioral
// assertions. They are kept as explicitly-named `#[ignore]`d *benchmarks* with
// a real, non-empty body (the prior empty `async fn x() {}` stubs were
// fake-green: even `--ignored` ran nothing). They are excluded from the
// default `cargo test` run (the multiplier is environment-sensitive and the
// 5x / adaptive rows are not assertable against the zero-latency in-memory
// fake), and each `#[ignore]` reason states exactly how to run it. Behavioral
// coverage of the same code paths is NOT skipped - it lives in the un-ignored
// rows above (fresh_sync_of_100_files, parallel_uploads_no_corruption,
// pipeline_streaming_keeps_memory_bounded) and in pacer.rs (AIMD reaction).
// ---------------------------------------------------------------------------

/// blake3 `update_rayon` (multi-core) must beat single-threaded `update` by
/// >=2x on a large buffer (DESIGN s11.4.4). Run it with:
///
/// ```text
/// cargo test -p driven-core --test e2e_fake -- --ignored blake3_rayon_2x
/// ```
///
/// The body is real: it always asserts the two hashers agree on the digest
/// (correctness of the multi-core path - a regression here would corrupt
/// `file_state` hashes), and on a machine with >=4 cores it additionally
/// asserts the >=2x speedup. On fewer cores it prints the measured ratio
/// instead of asserting (the 2x target needs real parallelism). Tracking: #28.
#[tokio::test]
#[ignore = "perf benchmark: blake3 update_rayon >=2x vs single-threaded update; \
            run with `cargo test -p driven-core --test e2e_fake -- --ignored \
            blake3_rayon_2x` on a multi-core box. Tracking #28."]
async fn blake3_rayon_2x() {
    // 256 MiB so the rayon fan-out amortizes over real work (well above the
    // 100 MiB RAYON_HASH_THRESHOLD the executor uses).
    let len = 256 * 1024 * 1024usize;
    let buf: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();

    let t0 = std::time::Instant::now();
    let serial = {
        let mut h = blake3::Hasher::new();
        h.update(&buf);
        h.finalize()
    };
    let serial_elapsed = t0.elapsed();

    let t1 = std::time::Instant::now();
    let rayon = {
        let mut h = blake3::Hasher::new();
        h.update_rayon(&buf);
        h.finalize()
    };
    let rayon_elapsed = t1.elapsed();

    // Correctness: the multi-core path must produce the identical digest.
    assert_eq!(
        serial, rayon,
        "update_rayon must produce the same blake3 digest as update"
    );

    let ratio = serial_elapsed.as_secs_f64() / rayon_elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    eprintln!(
        "blake3 {len} bytes: serial {serial_elapsed:?}, rayon {rayon_elapsed:?}, \
         speedup {ratio:.2}x on {cores} cores"
    );
    if cores >= 4 {
        assert!(
            ratio >= 2.0,
            "update_rayon must be >=2x faster on {cores} cores (got {ratio:.2}x)"
        );
    }
}

/// Parallel sync throughput must beat a serial baseline by >=5x (DESIGN
/// s11.4.2). Run it with:
///
/// ```text
/// cargo test -p driven-core --test e2e_fake -- --ignored throughput_5x_serial_baseline
/// ```
///
/// The body is real: it runs a genuine multi-file plan end to end through the
/// `DefaultExecutor` (real hash -> upload pipeline against the in-memory
/// remote), asserts every op completed and every object landed, and prints the
/// measured files/sec + MiB/s. The >=5x-vs-serial multiplier itself is NOT
/// asserted here: it needs a latency-shaping remote and a serial (pool=1)
/// baseline that the zero-latency in-memory fake + fixed pool size cannot
/// provide, so this remains a benchmark pointer. Tracking: #28.
#[tokio::test]
#[ignore = "perf benchmark: parallel sync >=5x serial throughput; run with \
            `cargo test -p driven-core --test e2e_fake -- --ignored \
            throughput_5x_serial_baseline`. The 5x multiplier needs a \
            latency-shaping remote (not the zero-latency fake); this body \
            measures + asserts a correct parallel run. Tracking #28."]
async fn throughput_5x_serial_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    let n_files = 64u32;
    let file_bytes = 256 * 1024usize; // 256 KiB each
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    let mut ops = Vec::new();
    for i in 0..n_files {
        let name = format!("f{i}.bin");
        let contents: Vec<u8> = (0..file_bytes)
            .map(|j| ((i as usize + j) % 251) as u8)
            .collect();
        write_file(src_dir.path(), &name, &contents);
        let rel = RelativePath::try_from(name).unwrap();
        ops.push(Op::HashThenUpload {
            source_id: src.id,
            relative_path: rel,
            size: file_bytes as u64,
        });
    }
    let plan = Plan {
        ops,
        collisions: vec![],
    };

    let clock = Arc::new(FakeClock::new());
    let exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer: test_pacer(clock.clone()),
            crypto: None,
            vss: None,
            network: None,
        },
        clock,
    );

    let t0 = std::time::Instant::now();
    let out = exec
        .execute(&src, &plan, &noop_progress, &noop_outcome)
        .await
        .unwrap();
    let elapsed = t0.elapsed();

    assert!(out.iter().all(|o| matches!(o, OpOutcome::Done { .. })));
    assert_eq!(
        live_object_count(&remote, &folder).await,
        n_files as usize,
        "every file landed"
    );
    let secs = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
    let mib = (n_files as f64 * file_bytes as f64) / (1024.0 * 1024.0);
    eprintln!(
        "throughput: {n_files} files in {elapsed:?} = {:.0} files/s, {:.1} MiB/s \
         (parallel pool); 5x-vs-serial needs a latency-shaping remote, see #28",
        n_files as f64 / secs,
        mib / secs
    );
}

// ---------------------------------------------------------------------------
// Row: bounded-memory streaming pipeline (P1-4, DESIGN s11.4.3 / s11.4.6).
//      The 16x100MB RSS<400MiB ceiling and the 1 GiB CPU-idle<90% multiplier
//      are two distinct claims. The MEMORY-BOUND half IS deterministically
//      measurable against the instantaneous fake: the streaming pipeline must
//      never buffer a whole file, so its peak in-flight bytes stay a small
//      multiple of the channel/wire-chunk sizes regardless of file size. We
//      instrument that directly with a MemGauge and assert it. The CPU-idle
//      / throughput-multiplier half is NOT measurable against a zero-latency
//      fake (it needs a real upload cost + a wall-clock/CPU probe) and stays
//      a reported-qualitative claim (see throughput_5x_serial_baseline /
//      blake3_rayon_2x, which remain ignored as perf micro-benchmarks).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pipeline_streaming_keeps_memory_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    // Several files, each well above PIPELINE_THRESHOLD and large enough to
    // run the uploader accumulator's flush loop many times. A regression to
    // whole-file buffering would push peak in-flight to file_size * N.
    let file_bytes = 32 * 1024 * 1024usize; // 32 MiB each
    let n_files = 4u32;
    let mut ops = Vec::new();
    for i in 0..n_files {
        let name = format!("big{i}.bin");
        let contents: Vec<u8> = (0..file_bytes)
            .map(|j| ((i as usize + j) % 251) as u8)
            .collect();
        write_file(src_dir.path(), &name, &contents);
        let rel = RelativePath::try_from(name).unwrap();
        ops.push(Op::HashThenUpload {
            source_id: driven_core::types::SourceId::new_v4(),
            relative_path: rel,
            size: file_bytes as u64,
        });
    }
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    let ops: Vec<Op> = ops
        .into_iter()
        .map(|op| match op {
            Op::HashThenUpload {
                relative_path,
                size,
                ..
            } => Op::HashThenUpload {
                source_id: src.id,
                relative_path,
                size,
            },
            other => other,
        })
        .collect();
    let plan = Plan {
        ops,
        collisions: vec![],
    };

    let clock = Arc::new(FakeClock::new());
    let gauge = Arc::new(MemGauge::default());
    let exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer: test_pacer(clock.clone()),
            crypto: None,
            vss: None,
            network: None,
        },
        clock,
    )
    .with_mem_gauge(gauge.clone());

    let out = exec
        .execute(&src, &plan, &noop_progress, &noop_outcome)
        .await
        .unwrap();
    assert!(out.iter().all(|o| matches!(o, OpOutcome::Done { .. })));
    assert_eq!(
        live_object_count(&remote, &folder).await,
        n_files as usize,
        "every streamed file landed"
    );

    // The pipeline ran across up to default_pool_size() files concurrently,
    // yet peak in-flight bytes stayed bounded by (pool * (channel backlog +
    // accumulator)), NOT by total bytes. With 4 x 32 MiB = 128 MiB of data
    // the peak must be a small fraction. The bound here is generous (per-file
    // ~10 MiB accumulator+channels x concurrency) but emphatically far below
    // the 128 MiB total, and a whole-file-buffering regression blows past it.
    let peak = gauge.peak();
    let total_bytes = (file_bytes as u64) * (n_files as u64);
    // Lower bound: prove the STREAMING path actually ran. peak==0 would mean
    // these large files took the inline buffered path (gauge records nothing)
    // and every upper-bound assertion below would pass vacuously. The
    // uploader's 2 x WIRE_CHUNK (8 MiB) hold-back guarantees peak >= one wire
    // chunk (4 MiB) for any 32 MiB file.
    const WIRE_CHUNK: u64 = 4 * 1024 * 1024;
    assert!(
        peak >= WIRE_CHUNK,
        "streaming must actually run (peak >= one 4 MiB wire chunk proves accumulation), got {peak}"
    );
    assert!(
        peak < total_bytes / 2,
        "streaming peak in-flight {peak} bytes must stay far below the {total_bytes}-byte total (bounded pipeline, not whole-file buffering)"
    );
    // Per-file, the bound is the channel backlog + < 2 wire chunks (~9 MiB);
    // even under full concurrency the peak per the default pool is well under
    // 100 MiB. This is the discriminating ceiling.
    assert!(
        peak < 100 * 1024 * 1024,
        "peak in-flight {peak} bytes must respect the bounded-memory ceiling"
    );
}

/// Adaptive-parallelism: the upload pool must shrink under induced latency and
/// recover when it clears (DESIGN s11.4.2 AIMD). Run it with:
///
/// ```text
/// cargo test -p driven-core --test e2e_fake -- --ignored adaptive_parallelism_reacts_to_latency
/// ```
///
/// The body is real: it runs a genuine multi-file plan end to end and asserts
/// every op completed correctly + prints the measured throughput. The pool's
/// reaction to *induced latency* is NOT asserted here: it is driven by the
/// `ThroughputProbe` loop in the app-shell `Orchestrator::run` select loop
/// (not wired into core) over a latency-shaping remote, neither of which this
/// harness provides. The AIMD step logic itself is unit-tested in pacer.rs.
/// Tracking: #28.
#[tokio::test]
#[ignore = "perf benchmark: adaptive-parallelism pool reaction to induced \
            latency; run with `cargo test -p driven-core --test e2e_fake -- \
            --ignored adaptive_parallelism_reacts_to_latency`. The pool \
            reaction needs the app-shell ThroughputProbe loop + a \
            latency-shaping remote (not in core); AIMD steps are unit-tested \
            in pacer.rs. Tracking #28."]
async fn adaptive_parallelism_reacts_to_latency() {
    let dir = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let state = open_state(dir.path()).await;
    let account = seed_account(&state).await;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();

    let n_files = 32u32;
    let file_bytes = 256 * 1024usize;
    let src = source_in(account, src_dir.path(), &folder);
    state.upsert_source(&src).await.unwrap();
    let mut ops = Vec::new();
    for i in 0..n_files {
        let name = format!("a{i}.bin");
        let contents: Vec<u8> = (0..file_bytes)
            .map(|j| ((i as usize + j) % 251) as u8)
            .collect();
        write_file(src_dir.path(), &name, &contents);
        let rel = RelativePath::try_from(name).unwrap();
        ops.push(Op::HashThenUpload {
            source_id: src.id,
            relative_path: rel,
            size: file_bytes as u64,
        });
    }
    let plan = Plan {
        ops,
        collisions: vec![],
    };

    let clock = Arc::new(FakeClock::new());
    let exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: remote.clone(),
            state: state.clone(),
            pacer: test_pacer(clock.clone()),
            crypto: None,
            vss: None,
            network: None,
        },
        clock,
    );

    let out = exec
        .execute(&src, &plan, &noop_progress, &noop_outcome)
        .await
        .unwrap();
    assert!(out.iter().all(|o| matches!(o, OpOutcome::Done { .. })));
    assert_eq!(
        live_object_count(&remote, &folder).await,
        n_files as usize,
        "every file landed under the parallel pool"
    );
    eprintln!(
        "adaptive-parallelism: {n_files} files synced; pool-shrink-under-latency \
         needs the app-shell ThroughputProbe loop (see #28)"
    );
}

// The DNS-no-hang and lossy/intermittent breaker-cycle acceptance rows used to
// live here as empty `#[ignore]` stubs (fake-green: they asserted nothing).
// They have been replaced by real, deterministic, mutation-checked tests where
// the controllable seam + clock are reachable (issue #28):
//   - DNS no-hang (bounded by the 3s budget, classified DnsFailed):
//       driven-net `resolve_within_bounds_a_blackholed_lookup`
//       + `resolve_within_classifies_each_outcome`
//       + `resolve_invalid_tld_is_dns_failed_and_bounded`.
//   - Lossy (spread loss never trips the breaker) + intermittent (open during a
//     down window, close during an up window) circuit-breaker cycles:
//       driven-core `network::tests::lossy_spread_loss_never_opens_breaker`
//       + `network::tests::intermittent_link_opens_during_down_closes_during_up`
//       (alongside five_consecutive_failures_open_breaker / open_breaker_defers_probe_until_retry_at).
// These could not be honestly forced through the integration boundary: the
// FakeBackend / FakeClock / breaker seam they need is `#[cfg(test)]`-private to
// network.rs, and e2e_fake.rs is an external crate seeing only the public API.

// ---------------------------------------------------------------------------
// shared no-op progress sink
// ---------------------------------------------------------------------------

fn noop_progress(_p: driven_core::types::ExecProgress) {}

/// R2-P2-1: a no-op per-op outcome sink for the e2e fakes.
fn noop_outcome(_o: &OpOutcome) -> futures::future::BoxFuture<'static, ()> {
    Box::pin(async {})
}
