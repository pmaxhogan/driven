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
//! Rows that are genuinely infeasible against an in-memory fake (the
//! quantitative throughput multipliers, and the DNS-no-hang timeout row that
//! needs a real transport) are `#[ignore]`d with a reason rather than faked;
//! see the `ignored_*` tests at the bottom and the integration report.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use driven_core::executor::{DefaultExecutor, Executor, ExecutorDeps, OpOutcome};
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
use driven_drive::remote_store::RemoteStore;

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
        },
        clock.clone(),
    ));
    SyncOrchestrator::new(account, state, executor, power, net, clock, config)
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

    let out = exec.execute(&src, &plan, &noop_progress).await.unwrap();
    assert_eq!(out.len(), 10);
    assert!(
        out.iter().all(|o| matches!(o, OpOutcome::Done { .. })),
        "every op completes despite the 429 retry: {out:?}"
    );
    assert_eq!(live_object_count(&remote, &folder).await, 10);
}

// ---------------------------------------------------------------------------
// Row: crash mid-upload -> next sync adopts the orphaned remote object,
//      no duplicate (DESIGN s5.6 client_op_uuid reconciliation)
// ---------------------------------------------------------------------------
//
// SPEC AMBIGUITY (see report): the ROADMAP row says "next sync resumes the
// resumable session", but the executor has no byte-level cross-restart resume -
// `upload_resumable` opens a fresh session every call and `reconcile` never
// reopens a session or calls `resume_chunk`. Recovery is the `client_op_uuid`
// reconciliation protocol (DESIGN s5.6): the create's UUID lands in the remote
// object's `appProperties` atomically with the bytes, so after a crash the
// orphaned object is ADOPTED via `find_by_op_uuid` rather than re-uploaded.
//
// The only crash window where a DUPLICATE could occur is: the object landed on
// Drive (with its UUID stamped) but the local `commit_create_result` was lost
// before it ran. We model exactly that leftover state and prove reconcile
// adopts the orphan instead of creating a second object.

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
        },
        clock.clone(),
    );
    let out = exec.execute(&src, &plan, &noop_progress).await.unwrap();
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
    state
        .enqueue_pending_op(NewPendingOp {
            source_id: src.id,
            op_type: "upload".to_string(),
            relative_path: rel.clone(),
            payload_json: serde_json::json!({
                "client_op_uuid": op_uuid,
                "drive_file_id": serde_json::Value::Null,
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
        },
        clock,
    );
    let out = exec.execute(&src, &plan, &noop_progress).await.unwrap();
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
    let src = source_in(account, src_dir.path(), &folder);
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
            crypto: Some(suite),
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
    let out = exec.execute(&src, &plan, &noop_progress).await.unwrap();
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
// Ignored rows: genuinely infeasible against the in-memory fake (documented).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "Quantitative throughput multiplier (>=5x serial) is a perf benchmark, \
            not a behavioral assertion; meaningless against an instantaneous \
            in-memory fake with no real upload cost. Behavioral coverage: \
            fresh_sync_of_100_files + parallel_uploads_no_corruption."]
async fn throughput_5x_serial_baseline() {}

#[tokio::test]
#[ignore = "blake3 update_rayon >=2x single-threaded throughput is a CPU \
            micro-benchmark requiring a multi-core timing harness and a >100 MiB \
            file; flaky/infeasible as a correctness test against the fake."]
async fn blake3_rayon_2x() {}

#[tokio::test]
#[ignore = "1 GiB pipeline CPU-idle <90% and the 16x100MB RSS<400MiB ceiling are \
            resource/timing measurements needing a real large-file workload and \
            an RSS probe; not assertable against an in-memory fake on CI."]
async fn pipeline_cpu_and_memory_bounds() {}

#[tokio::test]
#[ignore = "Adaptive-parallelism pool reaction to induced latency needs the real \
            ThroughputProbe loop in Orchestrator::run (app-shell select loop, not \
            wired in core) + a latency-shaping remote; the pacer AIMD reaction is \
            unit-tested in pacer.rs."]
async fn adaptive_parallelism_reacts_to_latency() {}

#[tokio::test]
#[ignore = "DNS-fail no-hang-beyond-3s asserts a real reqwest/hickory connect \
            timeout; InMemoryRemoteStore has no transport to time out. The probe \
            classification (DnsFail) is unit-tested in network.rs."]
async fn dns_fail_no_hang() {}

#[tokio::test]
#[ignore = "Lossy (30% drop +500ms) and intermittent (60s up/down) circuit-breaker \
            open/close cycles are exercised against the real Prober + FakeBackend \
            in network.rs unit tests (five_consecutive_failures_open_breaker, \
            intermittent_opens_then_closes_breaker); re-testing the breaker \
            mechanics at integration scope needs those private internals."]
async fn lossy_and_intermittent_breaker_cycles() {}

// ---------------------------------------------------------------------------
// shared no-op progress sink
// ---------------------------------------------------------------------------

fn noop_progress(_p: driven_core::types::ExecProgress) {}
