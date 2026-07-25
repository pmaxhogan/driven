//! The Driven side of the benchmark: one real backup cycle, in a child process.
//!
//! This is NOT a simplified upload loop. It assembles the same engine the
//! desktop app assembles in `src-tauri/src/assembly.rs` - `SqliteStateRepo` ->
//! `DefaultExecutor` (with the adaptive upload pool and the AIMD pacer) ->
//! `SyncOrchestrator` - and runs `run_cycle`, so what gets measured is the real
//! scan -> plan -> execute -> verify pipeline against a live `GoogleDriveStore`.
//! Benchmarking `driven-cli sync` instead would have measured a debug driver
//! that walks only the top level of the source folder and keeps no state.
//!
//! What is deliberately NOT wired, and why:
//!
//! - **VSS / crypto / hooks**: off. Encryption and shadow copies are opt-in
//!   features; including them would measure a configuration most users do not
//!   run, and rclone has no equivalent to compare against.
//! - **Network probing**: replaced with an always-online probe. The real prober
//!   issues its own HTTP requests, which would pollute the API-call count
//!   without telling us anything about backup throughput.
//! - **Power gating**: a fixed on-AC state, and both `skip_on_battery` and
//!   `skip_on_metered` are off, so an unplugged laptop cannot silently turn a
//!   benchmark into a no-op that looks blazingly fast.
//!
//! The process prints exactly one machine-readable line, prefixed with
//! [`METRICS_PREFIX`], for the parent harness to parse.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use driven_core::executor::{DefaultExecutor, ExecutorDeps};
use driven_core::network::{NetworkProbe, NetworkState, ServiceHealth, ServiceName};
use driven_core::orchestrator::{OrchestratorConfig, SyncOrchestrator, TickSource};
use driven_core::pacer::AimdPacer;
use driven_core::state::{AccountRow, SourceRow, SqliteStateRepo, StateRepo};
use driven_core::time::{Clock, SystemClock};
use driven_core::types::{AccountId, AccountState, OrchestratorEvent, SourceId};
use driven_drive::remote_store::RemoteStore;
use driven_power::{PowerSource, PowerState};
use driven_test_fixtures::power::FakePowerSource;

use crate::counting_store::{ApiCounts, CountingStore};
use crate::creds::BenchCreds;

/// The stdout marker the parent harness looks for.
pub const METRICS_PREFIX: &str = "DRIVEN_BENCH_METRICS ";

/// What one engine cycle reported about itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
// Missing fields decode as zero, so a metrics line written by an older build
// still parses instead of failing the whole phase.
#[serde(default)]
pub struct AgentMetrics {
    /// Time inside `run_cycle`, excluding process startup and Drive auth.
    pub engine_ms: u64,
    /// Time from the start of the cycle until the planner ran, i.e. how long
    /// walking and hashing the tree took.
    ///
    /// This is the number that says WHERE a slow run went. On the million-tiny-
    /// files shape a cold pass can be bound by the local scan or by Drive
    /// round-trips, and the totals alone cannot tell those apart. `None` when
    /// the cycle ended before the planner reported (nothing to do, or an error).
    pub scan_ms: Option<u64>,
    /// Time from the planner finishing to the end of the cycle, i.e. the
    /// upload half. `engine_ms - scan_ms` net of the planning step itself.
    pub upload_ms: Option<u64>,
    /// Files the executor finished, from the progress stream.
    pub files_done: u64,
    /// Bytes the executor moved, from the progress stream.
    pub bytes_done: u64,
    /// Files the planner decided to upload this cycle. On an incremental run
    /// this is the change-detection result: it should equal the mutated file
    /// count, not the whole tree.
    pub planned_uploads: u64,
    /// Bytes the planner decided to upload this cycle.
    pub planned_bytes: u64,
    /// Errors the executor reported.
    pub errors: u64,
    /// `upload_done` rows written during the cycle (the durable count).
    pub logged_files_uploaded: u64,
    /// Summed `upload_done` bytes during the cycle (the durable count).
    pub logged_bytes_uploaded: u64,
    /// Drive requests, counted at the store seam.
    pub api: ApiCounts,
}

/// Arguments for the in-child engine run.
#[derive(Debug, Clone, clap::Args)]
pub struct AgentArgs {
    /// The local folder to back up.
    #[arg(long)]
    pub source: PathBuf,
    /// The Drive folder id to upload into.
    #[arg(long)]
    pub dest_folder_id: String,
    /// The state database for this scenario. Reused across the cold and
    /// incremental phases so the incremental phase actually has prior state to
    /// compare against.
    #[arg(long)]
    pub state_db: PathBuf,
}

/// A network probe that always answers "online".
struct AlwaysOnline;

#[async_trait::async_trait]
impl NetworkProbe for AlwaysOnline {
    async fn probe(&self) -> NetworkState {
        NetworkState::Online
    }
    fn service_health(&self, _service: ServiceName) -> ServiceHealth {
        ServiceHealth::Closed
    }
    fn note_outcome(&self, _service: ServiceName, _ok: bool) {}
}

/// Runs one backup cycle and prints the metrics line.
pub async fn run(args: AgentArgs) -> Result<()> {
    let creds = BenchCreds::from_env()?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let state: Arc<SqliteStateRepo> = Arc::new(
        SqliteStateRepo::open(&args.state_db)
            .await
            .with_context(|| format!("opening state db {}", args.state_db.display()))?,
    );

    // One account, reused across phases so the second cycle sees the first
    // cycle's `file_state` rows.
    let account_id = match state.list_accounts().await?.into_iter().next() {
        Some(existing) => existing.id,
        None => {
            let id = AccountId::new_v4();
            state
                .upsert_account(&AccountRow {
                    id,
                    email: "bench@driven.invalid".into(),
                    display_name: Some("driven-bench".into()),
                    state: AccountState::Ok,
                    encryption_master_key_id: None,
                    created_at: clock.now_ms(),
                    last_synced_at: None,
                })
                .await?;
            id
        }
    };

    // Same source row across phases, keyed by the local path. A fresh SourceId
    // per phase would orphan every `file_state` row and turn the incremental
    // phase into a second cold upload - the classic way to report a great
    // change-detection number that means nothing.
    let local_path = args.source.to_string_lossy().into_owned();
    let source = match state
        .list_sources()
        .await?
        .into_iter()
        .find(|s| s.local_path == local_path)
    {
        Some(existing) => existing,
        None => {
            let row = new_source(
                account_id,
                &local_path,
                &args.dest_folder_id,
                clock.now_ms(),
            );
            state.upsert_source(&row).await?;
            row
        }
    };

    let real: Arc<dyn RemoteStore> = Arc::new(creds.build_store()?);
    let (remote, counters) = CountingStore::new(real);

    let pacer = Arc::new(AimdPacer::new(clock.clone(), None));
    let power: Arc<dyn PowerSource> = Arc::new(FakePowerSource::new(PowerState {
        ac_connected: true,
        battery_percent: None,
        on_metered_network: false,
        network_reachable: true,
    }));
    let network: Arc<dyn NetworkProbe> = Arc::new(AlwaysOnline);

    // Mirror the app's adaptive upload parallelism (DESIGN s11.4.7): the pool
    // must be built here and injected, because `DefaultExecutor` otherwise
    // constructs its own and any configured concurrency is silently ignored.
    let upload_pool =
        driven_core::adaptive::UploadPool::new(driven_core::adaptive::default_pool_size());
    let throughput = driven_core::adaptive::ThroughputProbe::new();

    let executor = Arc::new(
        DefaultExecutor::with_clock(
            ExecutorDeps {
                remote: remote.clone(),
                state: state.clone(),
                pacer: pacer.clone(),
                crypto: None,
                vss: None,
                network: None,
            },
            clock.clone(),
        )
        .with_upload_pool(upload_pool.clone())
        .with_throughput_probe(throughput.clone()),
    );

    let config = OrchestratorConfig {
        // A laptop on battery, or a runner whose connection looks metered, must
        // not turn the benchmark into a paused no-op.
        skip_on_battery: false,
        skip_on_metered: false,
        ..Default::default()
    };

    let mut orchestrator = SyncOrchestrator::new(
        account_id,
        state.clone(),
        executor,
        power,
        network,
        clock.clone(),
        config,
    );
    orchestrator = orchestrator.with_pacer(pacer.clone());
    let disk: Arc<dyn driven_diskstat::DiskBusyProbe> = Arc::new(
        driven_diskstat::RealDiskBusyProbe::new(PathBuf::from(&source.local_path)),
    );
    orchestrator = orchestrator.with_adaptive_controller(Arc::new(
        driven_core::adaptive::AdaptiveController::new(
            upload_pool,
            throughput,
            disk,
            pacer,
            clock.clone(),
        ),
    ));
    let orchestrator = Arc::new(orchestrator);

    let events = orchestrator.subscribe();
    let window_start = clock.now_ms();
    let started = Instant::now();

    // Consume the event stream WHILE the cycle runs, not afterwards. Draining it
    // at the end would lose the phase boundaries entirely (a timestamp cannot be
    // recovered from a buffered event) and, on a large run, would also lose
    // events to broadcast lag before they were ever read.
    let observed = Arc::new(Mutex::new(Observed::default()));
    let observer = tokio::spawn(observe(events, observed.clone(), started));

    orchestrator
        .run_cycle(TickSource::Manual)
        .await
        .context("running the backup cycle")?;
    let engine_ms = started.elapsed().as_millis() as u64;
    let window_end = clock.now_ms();

    // The orchestrator owns the broadcast sender, so the observer's `recv` never
    // returns `Closed` on its own; the cycle is over, so stop it.
    observer.abort();
    let observed = observed.lock().expect("observer mutex").clone();

    let metrics = AgentMetrics {
        engine_ms,
        scan_ms: observed.scan_ms,
        upload_ms: observed.scan_ms.map(|scan| engine_ms.saturating_sub(scan)),
        files_done: observed.files_done,
        bytes_done: observed.bytes_done,
        planned_uploads: observed.planned_uploads,
        planned_bytes: observed.planned_bytes,
        errors: observed.errors,
        api: counters.snapshot(),
        ..Default::default()
    };
    let mut metrics = metrics;

    // The durable counterpart to the progress stream: the broadcast channel can
    // lag on a large run, the activity rows cannot.
    let telemetry = state
        .telemetry_events_since(window_start, window_end.max(window_start + 1))
        .await
        .context("reading the run's activity rows")?;
    metrics.logged_files_uploaded = telemetry.files_uploaded;
    metrics.logged_bytes_uploaded = telemetry.bytes_uploaded;

    println!("{METRICS_PREFIX}{}", serde_json::to_string(&metrics)?);
    Ok(())
}

/// What watching the orchestrator's event stream revealed about one cycle.
#[derive(Debug, Clone, Default)]
struct Observed {
    /// Elapsed time when the planner first reported - the end of the scan.
    scan_ms: Option<u64>,
    files_done: u64,
    bytes_done: u64,
    planned_uploads: u64,
    planned_bytes: u64,
    errors: u64,
}

/// Folds orchestrator events into `observed` as they arrive.
///
/// The executor emits cumulative progress snapshots and the orchestrator
/// forwards a closing one whose per-counter values may be lower, so each counter
/// takes its maximum rather than its last value. Runs until aborted.
async fn observe(
    mut events: tokio::sync::broadcast::Receiver<OrchestratorEvent>,
    observed: Arc<Mutex<Observed>>,
    started: Instant,
) {
    loop {
        match events.recv().await {
            Ok(OrchestratorEvent::Progress { progress, .. }) => {
                let mut o = observed.lock().expect("observer mutex");
                o.files_done = o.files_done.max(progress.files_done);
                o.bytes_done = o.bytes_done.max(progress.bytes_done);
                o.errors = o.errors.max(progress.errors);
            }
            Ok(OrchestratorEvent::StateChanged {
                state: driven_core::types::OrchestratorState::Planning { plan },
            }) => {
                let mut o = observed.lock().expect("observer mutex");
                // The FIRST planning event ends the scan; a later one (a second
                // source, say) must not overwrite that boundary.
                o.scan_ms
                    .get_or_insert_with(|| started.elapsed().as_millis() as u64);
                o.planned_uploads = o.planned_uploads.max(plan.uploads as u64);
                o.planned_bytes = o.planned_bytes.max(plan.bytes);
            }
            Ok(_) => {}
            // A lagged receiver has dropped events; the durable activity rows
            // are the authority for totals, so keep reading what is left.
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

/// Builds the benchmark's backup source row.
fn new_source(account_id: AccountId, local_path: &str, folder_id: &str, now: i64) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id,
        display_name: "driven-bench".into(),
        enabled: true,
        local_path: local_path.to_string(),
        drive_folder_id: folder_id.to_string(),
        drive_id: None,
        drive_folder_path: "/driven-bench".into(),
        encryption_enabled: false,
        wrapped_source_key: None,
        respect_gitignore: false,
        include_patterns: vec![],
        exclude_patterns: vec![],
        placeholder_policy: Default::default(),
        schedule_json_v2_reserved: None,
        // A week, so the deep-verify pass never fires mid-benchmark and turns
        // one run's numbers into an outlier.
        deep_verify_interval_secs: 604_800,
        last_full_scan_at: None,
        last_deep_verify_at: Some(now),
        mtime_granularity_ns: None,
        created_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_round_trip_through_the_marker_line() {
        let metrics = AgentMetrics {
            engine_ms: 1234,
            scan_ms: Some(400),
            upload_ms: Some(834),
            files_done: 7,
            bytes_done: 42,
            planned_uploads: 7,
            ..Default::default()
        };
        let line = format!(
            "{METRICS_PREFIX}{}",
            serde_json::to_string(&metrics).unwrap()
        );
        let parsed = crate::tools::parse_agent_metrics(&line).expect("parses");
        assert_eq!(parsed.engine_ms, 1234);
        assert_eq!(parsed.scan_ms, Some(400));
        assert_eq!(parsed.upload_ms, Some(834));
        assert_eq!(parsed.files_done, 7);
        assert_eq!(parsed.planned_uploads, 7);
    }

    #[test]
    fn a_cycle_with_no_planning_event_reports_no_scan_time() {
        // A cycle that ends before the planner reports (nothing to do, or an
        // error) must leave the column empty rather than claim a zero-second
        // scan, which would read as "the walk was instant".
        let observed = Observed::default();
        assert_eq!(observed.scan_ms, None);
        let upload_ms = observed.scan_ms.map(|s| 50u64.saturating_sub(s));
        assert!(upload_ms.is_none());
    }

    #[test]
    fn the_first_planning_event_fixes_the_scan_boundary() {
        // A later planning event (a second source, say) must not move it.
        let mut observed = Observed::default();
        observed.scan_ms.get_or_insert(300);
        observed.scan_ms.get_or_insert(900);
        assert_eq!(observed.scan_ms, Some(300));
    }

    #[test]
    fn the_bench_source_never_enables_encryption_or_gitignore_rules() {
        // Both would change what is uploaded and make the comparison with rclone
        // meaningless, so they are pinned here rather than left to a default.
        let row = new_source(AccountId::new_v4(), "/tmp/x", "folder", 0);
        assert!(!row.encryption_enabled);
        assert!(!row.respect_gitignore);
        assert!(row.include_patterns.is_empty());
        assert!(row.exclude_patterns.is_empty());
        assert!(row.enabled);
    }
}
