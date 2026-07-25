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
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::TryRecvError;

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

    let mut events = orchestrator.subscribe();
    let window_start = clock.now_ms();
    let started = Instant::now();
    orchestrator
        .run_cycle(TickSource::Manual)
        .await
        .context("running the backup cycle")?;
    let engine_ms = started.elapsed().as_millis() as u64;
    let window_end = clock.now_ms();

    let mut metrics = AgentMetrics {
        engine_ms,
        api: counters.snapshot(),
        ..Default::default()
    };
    drain_events(&mut events, &mut metrics);

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

/// Folds every buffered orchestrator event into `metrics`.
///
/// The executor emits cumulative progress snapshots and the orchestrator
/// forwards a closing one whose per-counter values may be lower, so each counter
/// takes its maximum rather than its last value.
fn drain_events(
    events: &mut tokio::sync::broadcast::Receiver<OrchestratorEvent>,
    metrics: &mut AgentMetrics,
) {
    loop {
        match events.try_recv() {
            Ok(OrchestratorEvent::Progress { progress, .. }) => {
                metrics.files_done = metrics.files_done.max(progress.files_done);
                metrics.bytes_done = metrics.bytes_done.max(progress.bytes_done);
                metrics.errors = metrics.errors.max(progress.errors);
            }
            Ok(OrchestratorEvent::StateChanged {
                state: driven_core::types::OrchestratorState::Planning { plan },
            }) => {
                metrics.planned_uploads = metrics.planned_uploads.max(plan.uploads as u64);
                metrics.planned_bytes = metrics.planned_bytes.max(plan.bytes);
            }
            Ok(_) => {}
            // A lagged receiver has dropped events; the durable activity rows
            // below are the authority, so keep draining what is left.
            Err(TryRecvError::Lagged(_)) => {}
            Err(TryRecvError::Empty | TryRecvError::Closed) => return,
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
        assert_eq!(parsed.files_done, 7);
        assert_eq!(parsed.planned_uploads, 7);
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
