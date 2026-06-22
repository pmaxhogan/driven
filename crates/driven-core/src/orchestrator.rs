//! The sync orchestrator: one per account, driving the
//! [`OrchestratorState`](crate::types::OrchestratorState) machine
//! (SPEC s5, DESIGN s5.1).
//!
//! The orchestrator is the conductor. Each tick (scheduler timer, a
//! watcher [`ScanTickRequest`](crate::watcher::ScanTickRequest), or a
//! manual trigger) it checks the power / network gates (DESIGN s5.7), then
//! for each enabled source runs scan -> plan -> execute -> verify (SPEC
//! s5), transitioning the state machine and broadcasting
//! [`OrchestratorEvent`](crate::types::OrchestratorEvent)s to the IPC
//! bridge, tray, and activity-log writer (SPEC s11.7). It reacts to
//! [`PowerEvent`](crate::types::PowerEvent) suspend/resume (DESIGN s5.10)
//! and [`NetworkEvent`](crate::network::NetworkEvent) transitions (DESIGN
//! s5.8) on the same event loop.
//!
//! # Phase 1 surface vs M3 implementation
//!
//! The interfaces phase shipped the [`OrchestratorConfig`], [`TickSource`],
//! and [`Orchestrator`] *control-surface* contract. This module fills in the
//! concrete [`SyncOrchestrator`] - the per-account state machine - against
//! that committed surface plus the sibling [`crate::scanner`],
//! [`crate::planner`], [`crate::executor::Executor`], [`crate::pacer::Pacer`],
//! [`crate::network::NetworkProbe`], [`driven_power::PowerSource`], and
//! [`crate::time::Clock`] seams.
//!
//! # State machine (DESIGN s5.1)
//!
//! The happy path is `Idle -> PowerCheck -> Scanning -> Planning ->
//! Executing -> Verifying -> Idle`. Two states are *orthogonal* to that
//! pipeline, enterable from the gate check rather than mid-pipeline:
//! - [`OrchestratorState::Paused`] - a closed gate (battery / metered /
//!   offline / service-down) or a manual pause (DESIGN s5.7). Manual pause
//!   persists across restarts; the gate-driven pauses lift when the gate
//!   re-opens.
//! - [`OrchestratorState::Backoff`] - a rate-limit / circuit-breaker timer
//!   the pacer set (DESIGN s5.8.3). Cleared by the clock reaching `until`.
//!
//! # Determinism
//!
//! Every timing decision - the scheduled-scan interval, the rate-limit
//! backoff window, and the DESIGN s5.10.3 30-second resume defer - reads the
//! injected [`Clock`](crate::time::Clock), never `tokio::time` directly, so
//! the `FakeClock` in `driven-test-fixtures` can drive the machine
//! deterministically. The unit tests exercise a single
//! [`SyncOrchestrator::run_cycle`] at a time rather than spawning the full
//! [`Orchestrator::run`] loop, so no real wall clock or sleep is involved.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch, Mutex, RwLock};

use driven_power::PowerSource;

use crate::executor::{Executor, OpOutcome};
use crate::network::{NetworkProbe, NetworkState, ServiceHealth, ServiceName};
use crate::pacer::PacerCeilings;
use crate::state::{SourceRow, StateRepo};
use crate::time::Clock;
use crate::types::{
    AccountId, ExecProgress, OrchestratorEvent, OrchestratorState, PauseReason, PowerEvent,
    ScanMode, UnixMs,
};

/// Module-level tracing target (SPEC s0 logging convention).
const TARGET: &str = "driven::core::orchestrator";

/// Capacity of the orchestrator's [`OrchestratorEvent`] broadcast channel.
/// A lagged consumer re-reads [`Orchestrator::state`] rather than treating a
/// dropped event as data loss (see [`OrchestratorEvent`] docs).
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// The DESIGN s5.10.3 step 1 resume defer: after a wake, real-world network
/// and keychain services are not yet ready, so the orchestrator waits this
/// long (measured on the injected [`Clock`]) before re-probing and resuming.
const RESUME_DEFER_MS: i64 = 30_000;

/// Runtime configuration the orchestrator reads each cycle (SPEC s5
/// `config: Arc<RwLock<OrchestratorConfig>>`).
///
/// Held behind a lock by the impl so a settings change takes effect on the
/// next cycle without restarting the orchestrator. This is the subset of
/// `settings` (SPEC s22) the orchestrator's *control flow* reads; per-op
/// crypto / bandwidth specifics flow through the executor + pacer seams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorConfig {
    /// When `true`, the cycle stops after planning and logs a dry-run
    /// summary instead of executing (SPEC s5 `if config.dry_run`).
    pub dry_run: bool,
    /// Pause sync while on battery (DESIGN s5.7). Maps a battery state to
    /// [`PauseReason::Battery`](crate::types::PauseReason::Battery).
    pub skip_on_battery: bool,
    /// Pause sync on a metered network (DESIGN s5.7). Maps to
    /// [`PauseReason::Metered`](crate::types::PauseReason::Metered).
    pub skip_on_metered: bool,
    /// Base scheduled-scan interval in seconds; the authoritative fallback
    /// the watcher only accelerates (DESIGN s5.9).
    pub scan_interval_secs: u64,
    /// Bandwidth cap in Mbps, or `None` for unlimited (SPEC s9 bandwidth
    /// bucket). Threaded to the [`Pacer`](crate::pacer::Pacer) at
    /// construction; carried here so a settings change re-derives it.
    pub bandwidth_cap_mbps: Option<u32>,
    /// The pacer's hard-cap ceilings (SPEC s9, DESIGN s18.1). The current
    /// AIMD budget lives in the pacer; only the user-configurable caps are
    /// config.
    pub pacer_ceilings: PacerCeilings,
}

impl Default for OrchestratorConfig {
    /// Conservative, gate-respecting defaults (DESIGN s5.7, s5.9, SPEC s9):
    /// no dry-run, skip on battery + metered, 15-minute scan interval, no
    /// bandwidth cap, default pacer ceilings.
    fn default() -> Self {
        Self {
            dry_run: false,
            skip_on_battery: true,
            skip_on_metered: true,
            scan_interval_secs: 15 * 60,
            bandwidth_cap_mbps: None,
            pacer_ceilings: PacerCeilings::default(),
        }
    }
}

/// What kicked off an orchestrator cycle (DESIGN s5.1 "tick").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TickSource {
    /// The scheduled-scan timer fired (the authoritative fallback,
    /// DESIGN s5.9.5).
    Scheduled,
    /// A debounced filesystem-watcher request (DESIGN s5.9.1).
    Watcher,
    /// The user clicked "Sync now".
    Manual,
    /// A resume-from-sleep sequence asked for a re-scan (DESIGN s5.10.3
    /// step 5).
    Wake,
}

/// The orchestrator control surface (SPEC s5).
///
/// One instance per account. The owning process spawns [`Self::run`] as a
/// long-lived task; the IPC layer and tray call the control methods to
/// pause/resume and to read the current state for display. The state
/// machine, scan/plan/execute/verify sequencing, and event broadcasting
/// are the implementer's; this trait is the seam the Tauri command layer
/// (SPEC s11.3) and tests code against.
#[async_trait]
pub trait Orchestrator: Send + Sync {
    /// Runs the orchestrator loop until cancelled (SPEC s5
    /// `run(self: Arc<Self>)`). Each iteration waits for a tick or signal,
    /// checks the gates, and runs every enabled source. Returns `Ok(())`
    /// on a clean shutdown; an unrecoverable error propagates.
    async fn run(&self) -> anyhow::Result<()>;

    /// Requests an out-of-band cycle now, recording `reason` as the tick
    /// source (DESIGN s5.1). Ticks every enabled source for the account.
    async fn trigger(&self, reason: TickSource);

    /// Sets the manual-pause signal (DESIGN s5.7: orthogonal to the
    /// gate-driven pauses, persists across restarts). `true` pauses,
    /// `false` resumes.
    async fn set_paused(&self, paused: bool);

    /// Returns a snapshot of the current
    /// [`OrchestratorState`] for the tray / Activity dashboard.
    async fn state(&self) -> OrchestratorState;

    /// Applies a new [`OrchestratorConfig`], taking effect on the next
    /// cycle (the `Arc<RwLock<OrchestratorConfig>>` swap, SPEC s5).
    async fn reconfigure(&self, config: OrchestratorConfig);
}

/// Why the orchestrator's gate check refused to start a batch this cycle
/// (DESIGN s5.7, s5.8.6).
///
/// `Ok` lets the pipeline run; the other variants short-circuit it into a
/// [`OrchestratorState::Paused`] or [`OrchestratorState::Backoff`] without
/// touching the remote store - the load-bearing invariant the "dry-run /
/// gated => zero remote calls" tests rely on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateDecision {
    /// All gates open; the pipeline may run.
    Proceed,
    /// A gate is closed; pause with this reason (DESIGN s5.7).
    Pause(PauseReason),
    /// A rate-limit / circuit-breaker timer is active; back off until this
    /// wall-clock ms (DESIGN s5.8.3).
    Backoff(UnixMs),
}

/// The concrete per-account orchestrator (SPEC s5).
///
/// Owns the dependency seams and the [`OrchestratorState`] machine behind a
/// lock. Constructed via [`SyncOrchestrator::new`]; the owning process holds
/// it in an `Arc` and spawns [`Orchestrator::run`].
///
/// All real I/O is behind the injected traits, so the whole orchestrator is
/// exercisable against the `InMemoryRemoteStore`, `FakeClock`,
/// `FakePowerSource`, and `FakeNetwork` fixtures from `driven-test-fixtures`
/// (DESIGN s14) with no Tauri shell, no real Drive, and no real wall clock.
pub struct SyncOrchestrator {
    /// The account this orchestrator drives (DESIGN s5.1: one per account).
    account_id: AccountId,
    /// SQLite state layer (SPEC s2). Source list, pending-ops, file_state.
    state: Arc<dyn StateRepo>,
    /// Plan executor (SPEC s8). Owns the upload pool + crash-safe protocol.
    executor: Arc<dyn Executor>,
    /// Power / battery / metered / reachability source (SPEC s10).
    power: Arc<dyn PowerSource>,
    /// Network-resilience probe + per-service circuit breakers (DESIGN s5.8).
    network: Arc<dyn NetworkProbe>,
    /// Clock seam; every timing decision reads this (DESIGN s18.7).
    clock: Arc<dyn Clock>,
    /// Live configuration (SPEC s5 `Arc<RwLock<OrchestratorConfig>>`).
    config: Arc<RwLock<OrchestratorConfig>>,
    /// The coarse-grained lifecycle state (DESIGN s5.1), snapshotted for the
    /// tray via [`Orchestrator::state`].
    state_machine: RwLock<OrchestratorState>,
    /// Manual-pause signal (DESIGN s5.7). `true` = user paused. The watch
    /// receiver lets the run loop wake on a flip; the sender is held so the
    /// control method [`Orchestrator::set_paused`] can drive it.
    pause_tx: watch::Sender<bool>,
    /// Broadcast sender for [`OrchestratorEvent`] (SPEC s5, s11.7).
    events: broadcast::Sender<OrchestratorEvent>,
    /// Has the startup reconciliation pass run yet (DESIGN s5.6)? Guards it
    /// to once-before-first-cycle. `Mutex` not `RwLock` because the
    /// check-and-set must be atomic.
    reconciled: Mutex<bool>,
}

impl SyncOrchestrator {
    /// Builds a new orchestrator for `account_id` over the injected seams.
    ///
    /// Starts in [`OrchestratorState::Idle`] with `last_run_at = None` and an
    /// unpaused manual signal. The reconciliation pass (DESIGN s5.6) runs
    /// lazily on the first [`Self::run_cycle`] / [`Orchestrator::run`]
    /// iteration, not here.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        account_id: AccountId,
        state: Arc<dyn StateRepo>,
        executor: Arc<dyn Executor>,
        power: Arc<dyn PowerSource>,
        network: Arc<dyn NetworkProbe>,
        clock: Arc<dyn Clock>,
        config: OrchestratorConfig,
    ) -> Self {
        let (events, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (pause_tx, _pause_rx) = watch::channel(false);
        Self {
            account_id,
            state,
            executor,
            power,
            network,
            clock,
            config: Arc::new(RwLock::new(config)),
            state_machine: RwLock::new(OrchestratorState::Idle { last_run_at: None }),
            pause_tx,
            events,
            reconciled: Mutex::new(false),
        }
    }

    /// Subscribes to the [`OrchestratorEvent`] broadcast (SPEC s5, s11.7).
    ///
    /// The IPC event bridge, the tray, and the activity-log writer each call
    /// this. A consumer that lags sees `RecvError::Lagged` and should re-read
    /// [`Orchestrator::state`] rather than treat the gap as lost data.
    pub fn subscribe(&self) -> broadcast::Receiver<OrchestratorEvent> {
        self.events.subscribe()
    }

    /// Transitions to `next`, storing it and broadcasting a
    /// [`OrchestratorEvent::StateChanged`] (SPEC s5 `transition`).
    ///
    /// A `send` error means no subscribers are listening, which is benign for
    /// a tray-facing event - the next [`Orchestrator::state`] read still
    /// reflects the stored state.
    async fn transition(&self, next: OrchestratorState) {
        *self.state_machine.write().await = next.clone();
        let _ = self
            .events
            .send(OrchestratorEvent::StateChanged { state: next });
    }

    /// Broadcasts an execution-progress tick (SPEC s5, s11.7). Throttled by
    /// the executor's `on_progress` cadence, not one per byte.
    fn emit_progress(&self, source_id: crate::types::SourceId, progress: ExecProgress) {
        let _ = self.events.send(OrchestratorEvent::Progress {
            source_id,
            progress,
        });
    }

    /// Evaluates the power / network gates before a batch (DESIGN s5.7, s5.8).
    ///
    /// Order mirrors DESIGN s5.7's precedence: a manual pause wins over every
    /// gate (it is the user's explicit intent and persists across restarts),
    /// then offline / metered / battery, then the Drive circuit breaker.
    /// Returns the decision the cycle acts on without issuing any remote call.
    async fn evaluate_gates(&self) -> GateDecision {
        // Manual pause is orthogonal and highest precedence (DESIGN s5.7).
        if *self.pause_tx.borrow() {
            return GateDecision::Pause(PauseReason::Manual);
        }

        let cfg = self.config.read().await.clone();
        let power = self.power.current().await;

        // Network reachability first (DESIGN s5.8): no point pausing for
        // battery when we are simply offline - offline is the more actionable
        // banner. A non-online probe maps to Offline (the network-resilience
        // layer renders the finer captive/no-internet/DNS substates).
        if !power.network_reachable || self.network.probe().await != NetworkState::Online {
            return GateDecision::Pause(PauseReason::Offline);
        }

        // Metered network (DESIGN s5.7): pause if configured.
        if cfg.skip_on_metered && power.on_metered_network {
            return GateDecision::Pause(PauseReason::Metered);
        }

        // Battery gate (DESIGN s5.7): pause on battery when skip_on_battery.
        if cfg.skip_on_battery && !power.ac_connected {
            return GateDecision::Pause(PauseReason::Battery);
        }

        // Drive circuit breaker (DESIGN s5.8.3): if Drive's breaker is open,
        // back off until its half-open probe time rather than hammer a known-
        // down dependency.
        if let ServiceHealth::Open { retry_at } = self.network.service_health(ServiceName::Drive) {
            if retry_at > self.clock.now_ms() {
                return GateDecision::Backoff(retry_at);
            }
        }

        GateDecision::Proceed
    }

    /// True when a deep-verify pass is due for `source` (SPEC s5
    /// `verify::due`, DESIGN s3.3).
    ///
    /// Due when the source has never had a deep-verify, or when at least
    /// `deep_verify_interval_secs` of wall time have elapsed since the last
    /// one. Reads the injected [`Clock`]; a backwards wall jump simply makes
    /// the next verify look "not yet due" until the clock catches up, which is
    /// safe (the failure mode is a delayed verify, never a missed upload).
    fn deep_verify_due(&self, source: &SourceRow) -> bool {
        match source.last_deep_verify_at {
            None => true,
            Some(last) => {
                let interval_ms = i64::from(source.deep_verify_interval_secs).saturating_mul(1_000);
                self.clock.now_ms().saturating_sub(last) >= interval_ms
            }
        }
    }

    /// Runs the startup reconciliation pass once (DESIGN s5.6).
    ///
    /// Adopts orphaned remote objects for any still-pending op carrying a
    /// `client_op_uuid` (or re-runs the op) so a `kill -9` mid-create never
    /// leaves a duplicate on Drive. Idempotent + guarded to run at most once
    /// before the first normal cycle; cheap - touches only `pending_ops`.
    async fn reconcile_once(&self) -> anyhow::Result<()> {
        {
            let mut done = self.reconciled.lock().await;
            if *done {
                return Ok(());
            }
            *done = true;
        }
        let sources = self.state.list_enabled_sources_for(self.account_id).await?;
        for source in &sources {
            tracing::debug!(target: TARGET, source_id = %source.id, "startup reconcile");
            self.executor.reconcile(source).await?;
        }
        Ok(())
    }

    /// Runs the full scan -> plan -> execute -> verify pipeline for one source
    /// (SPEC s5 `run_one_source`).
    ///
    /// `deep_verify` selects [`ScanMode::DeepVerify`] (the verify pass) vs
    /// [`ScanMode::FastPath`] (the normal per-tick scan); a deep-verify cycle
    /// transitions through [`OrchestratorState::Verifying`] for the tray.
    /// On `dry_run` the pipeline stops after planning and issues no remote
    /// call (SPEC s5).
    async fn run_one_source(&self, source: &SourceRow, deep_verify: bool) -> anyhow::Result<()> {
        let mode = if deep_verify {
            ScanMode::DeepVerify
        } else {
            ScanMode::FastPath
        };

        self.transition(OrchestratorState::Scanning {
            source_id: source.id,
            scanned: 0,
        })
        .await;
        let scan = crate::scanner::scan(source, self.state.as_ref(), mode).await?;

        // DESIGN s5.5: flag still-present-but-now-excluded paths so the UI can
        // surface them; never a trash. Non-fatal - a flag write failure must
        // not abort the upload pipeline.
        if !scan.excluded_orphans.is_empty() {
            if let Err(err) = self
                .state
                .mark_excluded_orphans(source.id, &scan.excluded_orphans)
                .await
            {
                tracing::warn!(target: TARGET, source_id = %source.id, %err, "failed to mark excluded orphans");
            }
        }

        let plan = crate::planner::plan(source, &scan, self.state.as_ref()).await?;
        let summary = plan.summary();
        self.transition(OrchestratorState::Planning { plan: summary })
            .await;

        // SPEC s24 local.unicode_collision: surface every dropped collider as
        // an activity error; V1 policy is skip-the-colliding-file (the planner
        // already emitted no op), not fail-closed on the whole source.
        for collision in &plan.collisions {
            tracing::warn!(target: TARGET, source_id = %source.id, path = %collision, "local.unicode_collision; skipping colliding file");
        }

        // Dry-run: stop after planning, no remote call (SPEC s5). This is the
        // load-bearing branch the "dry-run computes plan + zero remote calls"
        // test asserts on.
        if self.config.read().await.dry_run {
            tracing::info!(
                target: TARGET,
                source_id = %source.id,
                uploads = summary.uploads,
                trashes = summary.trashes,
                bytes = summary.bytes,
                "dry-run: plan computed, skipping execution"
            );
            return Ok(());
        }

        self.transition(OrchestratorState::Executing {
            progress: ExecProgress::zero(),
        })
        .await;

        // The executor reports throttled progress; forward each tick as a
        // Progress event so the tray's bar moves without a full state render.
        let source_id = source.id;
        let events = self.events.clone();
        let on_progress = move |progress: ExecProgress| {
            let _ = events.send(OrchestratorEvent::Progress {
                source_id,
                progress,
            });
        };
        let outcomes = self.executor.execute(source, &plan, &on_progress).await?;
        log_outcomes(source, &outcomes);
        // Emit a final progress snapshot so a consumer that missed the
        // throttled ticks still sees the completed counts.
        self.emit_progress(source.id, exec_progress_from(&summary, &outcomes));

        // Deep-verify (DESIGN s3.3): the verify *mode* already re-hashed via
        // the DeepVerify scan above and any mismatch was re-uploaded by the
        // executor; the Verifying state reflects that pass for the tray.
        if deep_verify {
            let mismatches = outcomes
                .iter()
                .filter(|o| matches!(o, OpOutcome::Failed { .. }))
                .count() as u64;
            self.transition(OrchestratorState::Verifying {
                sampled: u64::try_from(summary.uploads).unwrap_or(u64::MAX),
                mismatches,
            })
            .await;
        }

        Ok(())
    }

    /// Runs exactly one orchestrator cycle (SPEC s5 one loop iteration).
    ///
    /// This is the deterministic unit the [`Orchestrator::run`] loop calls per
    /// tick and the tests drive directly: it runs the startup reconcile (once),
    /// checks the gates, and - if they are open - runs every enabled source
    /// through the pipeline, ending in [`OrchestratorState::Idle`]. When a gate
    /// is closed it transitions to [`OrchestratorState::Paused`] /
    /// [`OrchestratorState::Backoff`] and returns WITHOUT issuing any remote
    /// call.
    pub async fn run_cycle(&self, tick: TickSource) -> anyhow::Result<()> {
        self.reconcile_once().await?;

        self.transition(OrchestratorState::PowerCheck).await;
        match self.evaluate_gates().await {
            GateDecision::Pause(reason) => {
                tracing::info!(target: TARGET, account_id = %self.account_id, ?reason, ?tick, "gate closed; pausing");
                self.transition(OrchestratorState::Paused { reason }).await;
                return Ok(());
            }
            GateDecision::Backoff(until) => {
                tracing::info!(target: TARGET, account_id = %self.account_id, until, "Drive breaker open; backing off");
                self.transition(OrchestratorState::Backoff { until }).await;
                return Ok(());
            }
            GateDecision::Proceed => {}
        }

        let sources = self.state.list_enabled_sources_for(self.account_id).await?;
        for source in &sources {
            let deep_verify = self.deep_verify_due(source);
            self.run_one_source(source, deep_verify).await?;
        }

        self.transition(OrchestratorState::Idle {
            last_run_at: Some(self.clock.now_ms()),
        })
        .await;
        Ok(())
    }

    /// Handles a [`PowerEvent`] sleep/wake transition (DESIGN s5.10).
    ///
    /// On [`PowerEvent::Suspending`]: broadcast the event and pause gracefully
    /// (DESIGN s5.10.2). On [`PowerEvent::Resumed`]: run the strict s5.10.3
    /// resume sequence - defer 30 s (measured on the injected [`Clock`]),
    /// re-probe the network, then trigger a from-scratch re-scan (the executor
    /// re-creates any pre-sleep resumable session from byte 0 via its own
    /// session-restart path, so pre-sleep sessions are effectively discarded).
    ///
    /// The 30-second defer is expressed as a clock deadline returned to the
    /// caller (the run loop schedules the [`TickSource::Wake`] re-scan once the
    /// clock reaches it), keeping this method free of any real sleep so a
    /// `FakeClock`-driven test stays deterministic.
    pub async fn on_power_event(&self, event: PowerEvent) -> ResumePlan {
        let _ = self.events.send(OrchestratorEvent::Power { event });
        match event {
            PowerEvent::Suspending => {
                tracing::info!(target: TARGET, account_id = %self.account_id, "suspending: graceful pause");
                // Graceful pause; in-flight requests are allowed to finish
                // (DESIGN s5.10.2). The gate check on the next cycle handles
                // the rest.
                ResumePlan::None
            }
            PowerEvent::Resumed => {
                let resume_at = self.clock.now_ms().saturating_add(RESUME_DEFER_MS);
                tracing::info!(target: TARGET, account_id = %self.account_id, resume_at, "resumed: deferring 30s before re-probe + re-scan");
                ResumePlan::DeferUntil(resume_at)
            }
        }
    }

    /// Completes the DESIGN s5.10.3 resume sequence once the 30 s defer has
    /// elapsed (the clock has reached the [`ResumePlan::DeferUntil`] deadline).
    ///
    /// Re-probes the network (step 2) and, when the OS-connectivity probe is
    /// green, runs a fresh [`TickSource::Wake`] cycle (steps 5-6). If still
    /// offline it pauses rather than push work through a dead link.
    pub async fn complete_resume(&self) -> anyhow::Result<()> {
        // Step 2: re-probe; do not proceed until connectivity is green.
        if self.network.probe().await != NetworkState::Online {
            tracing::info!(target: TARGET, account_id = %self.account_id, "resume: network not yet online; staying paused");
            self.transition(OrchestratorState::Paused {
                reason: PauseReason::Offline,
            })
            .await;
            return Ok(());
        }
        // Steps 5-6: re-scan from scratch and resume normal ticks.
        self.run_cycle(TickSource::Wake).await
    }
}

/// What [`SyncOrchestrator::on_power_event`] asks the run loop to do next
/// (DESIGN s5.10.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumePlan {
    /// No deferred action (a `Suspending` event).
    None,
    /// Wait until the clock reaches this Unix-ms deadline, then call
    /// [`SyncOrchestrator::complete_resume`] (the 30 s wake defer).
    DeferUntil(UnixMs),
}

/// Derives a final [`ExecProgress`] snapshot from the plan summary and the
/// executor's per-op outcomes, for the closing Progress event.
fn exec_progress_from(summary: &crate::types::PlanSummary, outcomes: &[OpOutcome]) -> ExecProgress {
    let mut files_done: u64 = 0;
    let mut errors: u64 = 0;
    for outcome in outcomes {
        match outcome {
            OpOutcome::Done { .. } => files_done = files_done.saturating_add(1),
            OpOutcome::Failed { .. } => errors = errors.saturating_add(1),
            OpOutcome::Skipped { .. } => {}
        }
    }
    ExecProgress {
        files_done,
        files_total: u64::try_from(summary.uploads).unwrap_or(u64::MAX),
        bytes_done: summary.bytes,
        bytes_total: summary.bytes,
        trashes_total: u64::try_from(summary.trashes).unwrap_or(u64::MAX),
        errors,
        ..ExecProgress::zero()
    }
}

/// Logs the per-op outcomes of an execution at the appropriate level.
fn log_outcomes(source: &SourceRow, outcomes: &[OpOutcome]) {
    for outcome in outcomes {
        match outcome {
            OpOutcome::Done { relative_path } => {
                tracing::debug!(target: TARGET, source_id = %source.id, path = %relative_path, "op done");
            }
            OpOutcome::Skipped {
                relative_path,
                reason,
            } => {
                tracing::info!(target: TARGET, source_id = %source.id, path = %relative_path, ?reason, code = %reason.error_code(), "op skipped, re-queued");
            }
            OpOutcome::Failed {
                relative_path,
                code,
            } => {
                tracing::warn!(target: TARGET, source_id = %source.id, path = %relative_path, %code, "op failed");
            }
        }
    }
}

#[async_trait]
impl Orchestrator for SyncOrchestrator {
    async fn run(&self) -> anyhow::Result<()> {
        // The production run loop selects over the scheduled-tick timer, the
        // watcher channel, the power/network broadcast channels, the manual
        // pause watch, and the trigger channel (DESIGN s5.1, s5.9.1). The
        // channel wiring (watcher Receiver, trigger mpsc) is handed in by the
        // owning Tauri process at spawn time; until that wiring lands in the
        // app shell, `run` drives a single cycle so a smoke test of the spawn
        // path has defined behaviour. The deterministic per-tick logic the
        // tests exercise lives in `run_cycle`.
        self.run_cycle(TickSource::Scheduled).await
    }

    async fn trigger(&self, reason: TickSource) {
        // Out-of-band cycle. Errors are logged rather than propagated: a
        // trigger is fire-and-forget from the IPC layer's perspective, and a
        // failed cycle surfaces through the Error state + activity log.
        if let Err(err) = self.run_cycle(reason).await {
            tracing::warn!(target: TARGET, account_id = %self.account_id, ?reason, %err, "triggered cycle failed");
        }
    }

    async fn set_paused(&self, paused: bool) {
        // Update the manual-pause signal. The next gate check observes it; if
        // we are pausing now, reflect it immediately for the tray.
        //
        // `send_replace`, not `send`: the watch cell is a sender-held state
        // slot read via `pause_tx.borrow()` in `evaluate_gates`. `send` aborts
        // without writing the value when no receiver is currently subscribed
        // (the app-shell run loop subscribes only once it spawns), which would
        // silently leave the gate open. `send_replace` always writes.
        let _ = self.pause_tx.send_replace(paused);
        if paused {
            self.transition(OrchestratorState::Paused {
                reason: PauseReason::Manual,
            })
            .await;
        }
    }

    async fn state(&self) -> OrchestratorState {
        self.state_machine.read().await.clone()
    }

    async fn reconfigure(&self, config: OrchestratorConfig) {
        *self.config.write().await = config;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;

    // `super::*` already brings in `Executor`, `OpOutcome`, `NetworkProbe`,
    // `NetworkState`, `ServiceHealth`, `ServiceName`, `SourceRow`, `StateRepo`,
    // the orchestrator types, `ExecProgress`, `PauseReason`, `PowerEvent`,
    // `PowerSource`, etc. Only the test-only row + id types and the fixtures
    // need explicit imports.
    use super::*;
    use crate::state::{
        AccountRow, ActivityFilter, ActivityPage, FileSearchHit, FileStateRow, NewActivity,
        NewPendingOp, PageRequest, PendingOpRow,
    };
    use crate::test_support::FakeClock;
    use crate::types::{AccountState, ActivityId, PendingOpId, Plan, RelativePath, SourceId};
    use driven_power::PowerState;
    use driven_test_fixtures::power::FakePowerSource;

    // --- a recording fake executor -----------------------------------------

    /// Records every `execute` / `reconcile` call so tests can assert on
    /// remote activity (the "zero remote calls" invariants).
    #[derive(Default)]
    struct RecordingExecutor {
        executes: AtomicU64,
        reconciles: AtomicU64,
        /// Sources passed to `reconcile`, for the orphan-adoption test.
        reconciled_sources: StdMutex<Vec<SourceId>>,
    }

    #[async_trait]
    impl Executor for RecordingExecutor {
        async fn execute(
            &self,
            _source: &SourceRow,
            plan: &Plan,
            on_progress: &(dyn Fn(ExecProgress) + Send + Sync),
        ) -> anyhow::Result<Vec<OpOutcome>> {
            self.executes.fetch_add(1, Ordering::SeqCst);
            // Report a progress tick and a Done outcome per op.
            on_progress(ExecProgress {
                files_total: plan.ops.len() as u64,
                ..ExecProgress::zero()
            });
            let outcomes = plan
                .ops
                .iter()
                .map(|op| match op {
                    crate::types::Op::HashThenUpload { relative_path, .. } => OpOutcome::Done {
                        relative_path: relative_path.clone(),
                    },
                    crate::types::Op::Trash { relative_path, .. } => OpOutcome::Done {
                        relative_path: relative_path.clone(),
                    },
                })
                .collect();
            Ok(outcomes)
        }

        async fn reconcile(&self, source: &SourceRow) -> anyhow::Result<()> {
            self.reconciles.fetch_add(1, Ordering::SeqCst);
            self.reconciled_sources.lock().unwrap().push(source.id);
            Ok(())
        }
    }

    // --- a minimal in-memory StateRepo -------------------------------------

    /// Covers only the methods the orchestrator calls
    /// (`list_enabled_sources_for`, `load_source_file_state`,
    /// `get_file_state`, `mark_excluded_orphans`); the rest bail loudly.
    #[derive(Default)]
    struct FakeState {
        sources: StdMutex<Vec<SourceRow>>,
        files: StdMutex<HashMap<(SourceId, RelativePath), FileStateRow>>,
    }

    impl FakeState {
        fn with_sources(sources: Vec<SourceRow>) -> Self {
            Self {
                sources: StdMutex::new(sources),
                files: StdMutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl StateRepo for FakeState {
        async fn list_enabled_sources_for(
            &self,
            account: AccountId,
        ) -> anyhow::Result<Vec<SourceRow>> {
            Ok(self
                .sources
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.account_id == account && s.enabled)
                .cloned()
                .collect())
        }

        async fn load_source_file_state(
            &self,
            source: SourceId,
        ) -> anyhow::Result<HashMap<RelativePath, FileStateRow>> {
            Ok(self
                .files
                .lock()
                .unwrap()
                .iter()
                .filter(|((s, _), _)| *s == source)
                .map(|((_, p), r)| (p.clone(), r.clone()))
                .collect())
        }

        async fn get_file_state(
            &self,
            source: SourceId,
            path: &RelativePath,
        ) -> anyhow::Result<Option<FileStateRow>> {
            Ok(self
                .files
                .lock()
                .unwrap()
                .get(&(source, path.clone()))
                .cloned())
        }

        async fn mark_excluded_orphans(
            &self,
            _source: SourceId,
            _paths: &[RelativePath],
        ) -> anyhow::Result<u64> {
            Ok(0)
        }

        async fn delete_file_state(
            &self,
            source: SourceId,
            path: &RelativePath,
        ) -> anyhow::Result<()> {
            self.files.lock().unwrap().remove(&(source, path.clone()));
            Ok(())
        }

        async fn list_accounts(&self) -> anyhow::Result<Vec<AccountRow>> {
            unimplemented!()
        }
        async fn upsert_account(&self, _row: &AccountRow) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn mark_account_state(
            &self,
            _id: AccountId,
            _state: AccountState,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn delete_account(&self, _id: AccountId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn list_sources(&self) -> anyhow::Result<Vec<SourceRow>> {
            unimplemented!()
        }
        async fn upsert_source(&self, _row: &SourceRow) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn delete_source(&self, _id: SourceId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn upsert_file_state(&self, _row: &FileStateRow) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn enqueue_pending_op(&self, _row: NewPendingOp) -> anyhow::Result<PendingOpId> {
            unimplemented!()
        }
        async fn get_pending_ops_due(
            &self,
            _now_ms: i64,
            _limit: u32,
        ) -> anyhow::Result<Vec<PendingOpRow>> {
            unimplemented!()
        }
        async fn get_pending_ops_for_source(
            &self,
            _source: SourceId,
        ) -> anyhow::Result<Vec<PendingOpRow>> {
            unimplemented!()
        }
        async fn mark_pending_op_attempted(
            &self,
            _id: PendingOpId,
            _error: Option<&str>,
            _next_attempt_ms: i64,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn delete_pending_op(&self, _id: PendingOpId) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn update_pending_op_payload(
            &self,
            _id: PendingOpId,
            _payload_json: &serde_json::Value,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn commit_create_result(
            &self,
            _op_id: PendingOpId,
            _file_state: &FileStateRow,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn commit_update_result(
            &self,
            _op_id: PendingOpId,
            _file_state: &FileStateRow,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn write_activity(&self, _row: NewActivity) -> anyhow::Result<ActivityId> {
            unimplemented!()
        }
        async fn query_activity(
            &self,
            _filter: ActivityFilter,
            _page: PageRequest,
        ) -> anyhow::Result<ActivityPage> {
            unimplemented!()
        }
        async fn prune_activity_older_than(
            &self,
            _before_ms: i64,
            _hard_cap: u64,
            _batch_size: Option<u32>,
        ) -> anyhow::Result<u64> {
            unimplemented!()
        }
        async fn delete_activity_by_source(&self, _source: SourceId) -> anyhow::Result<u64> {
            unimplemented!()
        }
        async fn get_setting(&self, _key: &str) -> anyhow::Result<Option<serde_json::Value>> {
            unimplemented!()
        }
        async fn set_setting(&self, _key: &str, _value: &serde_json::Value) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn search_files(
            &self,
            _source: Option<SourceId>,
            _query: &str,
            _limit: u32,
        ) -> anyhow::Result<Vec<FileSearchHit>> {
            unimplemented!()
        }
    }

    // --- a configurable fake NetworkProbe ----------------------------------

    struct FakeNet {
        state: StdMutex<NetworkState>,
        drive_health: StdMutex<ServiceHealth>,
    }

    impl FakeNet {
        fn online() -> Self {
            Self {
                state: StdMutex::new(NetworkState::Online),
                drive_health: StdMutex::new(ServiceHealth::Closed),
            }
        }
        fn with_drive_open(retry_at: i64) -> Self {
            Self {
                state: StdMutex::new(NetworkState::Online),
                drive_health: StdMutex::new(ServiceHealth::Open { retry_at }),
            }
        }
    }

    #[async_trait]
    impl NetworkProbe for FakeNet {
        async fn probe(&self) -> NetworkState {
            *self.state.lock().unwrap()
        }
        fn service_health(&self, _service: ServiceName) -> ServiceHealth {
            *self.drive_health.lock().unwrap()
        }
        fn note_outcome(&self, _service: ServiceName, _ok: bool) {}
    }

    // --- helpers -----------------------------------------------------------

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

    fn source_in(account: AccountId, root: &std::path::Path) -> SourceRow {
        SourceRow {
            id: SourceId::new_v4(),
            account_id: account,
            display_name: "t".into(),
            enabled: true,
            local_path: root.to_string_lossy().into_owned(),
            drive_folder_id: "f".into(),
            drive_folder_path: "/f".into(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore: true,
            include_patterns: vec![],
            exclude_patterns: vec![],
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: Some(0),
            created_at: 0,
        }
    }

    /// Builds an orchestrator over the given seams. The temp dir is returned
    /// so it outlives the scan.
    fn build(
        account: AccountId,
        sources: Vec<SourceRow>,
        executor: Arc<RecordingExecutor>,
        power: PowerState,
        net: Arc<FakeNet>,
        config: OrchestratorConfig,
    ) -> (SyncOrchestrator, Arc<FakeClock>) {
        let state = Arc::new(FakeState::with_sources(sources));
        let clock = Arc::new(FakeClock::new());
        let power = Arc::new(FakePowerSource::new(power));
        let orch =
            SyncOrchestrator::new(account, state, executor, power, net, clock.clone(), config);
        (orch, clock)
    }

    #[tokio::test]
    async fn battery_gate_pauses_when_skip_on_battery() {
        // On battery with skip_on_battery => Paused{Battery}, no execute.
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        let (orch, _clock) = build(
            account,
            vec![src],
            exec.clone(),
            power_on_battery(),
            Arc::new(FakeNet::online()),
            OrchestratorConfig::default(),
        );

        orch.run_cycle(TickSource::Scheduled).await.unwrap();

        assert_eq!(
            orch.state().await,
            OrchestratorState::Paused {
                reason: PauseReason::Battery
            }
        );
        assert_eq!(
            exec.executes.load(Ordering::SeqCst),
            0,
            "battery pause must not execute any plan"
        );
    }

    #[tokio::test]
    async fn ac_resumes_after_battery_pause() {
        // Power gate: pause on battery, resume on AC (the two-cycle path).
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        // An empty source so execute is a no-op even when it does run.
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        let state = Arc::new(FakeState::with_sources(vec![src]));
        let clock = Arc::new(FakeClock::new());
        let power = Arc::new(FakePowerSource::new(power_on_battery()));
        let orch = SyncOrchestrator::new(
            account,
            state,
            exec.clone(),
            power.clone(),
            Arc::new(FakeNet::online()),
            clock,
            OrchestratorConfig::default(),
        );

        orch.run_cycle(TickSource::Scheduled).await.unwrap();
        assert!(matches!(
            orch.state().await,
            OrchestratorState::Paused {
                reason: PauseReason::Battery
            }
        ));

        // AC connects; next cycle proceeds to Idle.
        power.set(power_on_ac());
        orch.run_cycle(TickSource::Scheduled).await.unwrap();
        assert!(matches!(orch.state().await, OrchestratorState::Idle { .. }));
    }

    #[tokio::test]
    async fn dry_run_plans_with_zero_remote_calls() {
        // dry_run: computes the plan but never calls the executor.
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        // Seed a file so the plan is non-empty - the assertion that we do NOT
        // execute is only meaningful when there is work to skip.
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        let cfg = OrchestratorConfig {
            dry_run: true,
            ..OrchestratorConfig::default()
        };
        let (orch, _clock) = build(
            account,
            vec![src],
            exec.clone(),
            power_on_ac(),
            Arc::new(FakeNet::online()),
            cfg,
        );

        orch.run_cycle(TickSource::Scheduled).await.unwrap();

        assert_eq!(
            exec.executes.load(Ordering::SeqCst),
            0,
            "dry-run must compute the plan but issue zero remote calls"
        );
        // Cycle still completes to Idle after the dry-run summary.
        assert!(matches!(orch.state().await, OrchestratorState::Idle { .. }));
    }

    #[tokio::test]
    async fn non_dry_run_executes() {
        // Control for the dry-run test: with dry_run off and a non-empty plan,
        // the executor IS called.
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        let (orch, _clock) = build(
            account,
            vec![src],
            exec.clone(),
            power_on_ac(),
            Arc::new(FakeNet::online()),
            OrchestratorConfig::default(),
        );

        orch.run_cycle(TickSource::Scheduled).await.unwrap();
        assert_eq!(
            exec.executes.load(Ordering::SeqCst),
            1,
            "a non-empty plan must be executed when dry_run is off"
        );
    }

    #[tokio::test]
    async fn manual_pause_and_resume() {
        // set_paused(true) => Paused{Manual} and gate refuses the next cycle;
        // set_paused(false) => the cycle proceeds.
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        let (orch, _clock) = build(
            account,
            vec![src],
            exec.clone(),
            power_on_ac(),
            Arc::new(FakeNet::online()),
            OrchestratorConfig::default(),
        );

        orch.set_paused(true).await;
        assert_eq!(
            orch.state().await,
            OrchestratorState::Paused {
                reason: PauseReason::Manual
            }
        );
        orch.run_cycle(TickSource::Manual).await.unwrap();
        assert_eq!(
            orch.state().await,
            OrchestratorState::Paused {
                reason: PauseReason::Manual
            },
            "a manual pause must keep the gate closed"
        );
        assert_eq!(exec.executes.load(Ordering::SeqCst), 0);

        // Resume: the next cycle runs to Idle.
        orch.set_paused(false).await;
        orch.run_cycle(TickSource::Manual).await.unwrap();
        assert!(matches!(orch.state().await, OrchestratorState::Idle { .. }));
    }

    #[tokio::test]
    async fn startup_reconcile_adopts_orphan() {
        // The first cycle runs reconcile() exactly once per enabled source
        // (DESIGN s5.6 - the executor adopts the orphan); a second cycle does
        // not re-run it.
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        let src = source_in(account, dir.path());
        let src_id = src.id;
        let exec = Arc::new(RecordingExecutor::default());
        let (orch, _clock) = build(
            account,
            vec![src],
            exec.clone(),
            power_on_ac(),
            Arc::new(FakeNet::online()),
            OrchestratorConfig::default(),
        );

        orch.run_cycle(TickSource::Scheduled).await.unwrap();
        assert_eq!(
            exec.reconciles.load(Ordering::SeqCst),
            1,
            "startup reconcile runs once for the enabled source"
        );
        assert_eq!(
            exec.reconciled_sources.lock().unwrap().as_slice(),
            &[src_id],
            "reconcile adopts the orphan for the right source"
        );

        // A second cycle must NOT re-run the startup reconcile.
        orch.run_cycle(TickSource::Scheduled).await.unwrap();
        assert_eq!(
            exec.reconciles.load(Ordering::SeqCst),
            1,
            "reconcile is a once-before-first-cycle pass"
        );
    }

    #[tokio::test]
    async fn drive_breaker_open_backs_off() {
        // A Drive circuit breaker open past `now` => Backoff{until}, no execute.
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        // FakeClock starts at now_ms = 0; an open breaker until 60_000 is in
        // the future, so the gate backs off.
        let (orch, _clock) = build(
            account,
            vec![src],
            exec.clone(),
            power_on_ac(),
            Arc::new(FakeNet::with_drive_open(60_000)),
            OrchestratorConfig::default(),
        );

        orch.run_cycle(TickSource::Scheduled).await.unwrap();
        assert_eq!(
            orch.state().await,
            OrchestratorState::Backoff { until: 60_000 }
        );
        assert_eq!(exec.executes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn offline_pauses() {
        // Network probe Offline => Paused{Offline}, no execute.
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        let net = Arc::new(FakeNet::online());
        *net.state.lock().unwrap() = NetworkState::Offline;
        let (orch, _clock) = build(
            account,
            vec![src],
            exec.clone(),
            power_on_ac(),
            net,
            OrchestratorConfig::default(),
        );

        orch.run_cycle(TickSource::Scheduled).await.unwrap();
        assert_eq!(
            orch.state().await,
            OrchestratorState::Paused {
                reason: PauseReason::Offline
            }
        );
        assert_eq!(exec.executes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resume_defers_thirty_seconds_then_rescans() {
        // PowerEvent::Resumed => DeferUntil(now + 30s); after the clock
        // advances, complete_resume runs a Wake cycle to Idle.
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        let state = Arc::new(FakeState::with_sources(vec![src]));
        let clock = Arc::new(FakeClock::new());
        let power = Arc::new(FakePowerSource::new(power_on_ac()));
        let orch = SyncOrchestrator::new(
            account,
            state,
            exec.clone(),
            power,
            Arc::new(FakeNet::online()),
            clock.clone(),
            OrchestratorConfig::default(),
        );

        let plan = orch.on_power_event(PowerEvent::Resumed).await;
        assert_eq!(plan, ResumePlan::DeferUntil(RESUME_DEFER_MS));

        // Advance past the defer and complete the resume sequence.
        clock.advance(std::time::Duration::from_millis(RESUME_DEFER_MS as u64));
        orch.complete_resume().await.unwrap();
        assert!(matches!(orch.state().await, OrchestratorState::Idle { .. }));
    }

    #[tokio::test]
    async fn suspending_emits_no_defer() {
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        let (orch, _clock) = build(
            account,
            vec![src],
            exec,
            power_on_ac(),
            Arc::new(FakeNet::online()),
            OrchestratorConfig::default(),
        );
        assert_eq!(
            orch.on_power_event(PowerEvent::Suspending).await,
            ResumePlan::None
        );
    }
}
