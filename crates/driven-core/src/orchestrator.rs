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
use tokio::sync::{broadcast, mpsc, watch, Mutex, RwLock};

use driven_power::PowerSource;
use driven_vss::{VssMode, VssProvider};

use crate::executor::{Executor, OpOutcome};
use crate::network::{NetworkProbe, NetworkState, ServiceHealth, ServiceName};
use crate::pacer::PacerCeilings;
use crate::state::{ActivityLevel, NewActivity, SourceRow, StateRepo};
use crate::time::Clock;
use crate::types::{
    AccountId, ExecProgress, OrchestratorEvent, OrchestratorState, PauseReason, PowerEvent,
    RelativePath, ScanMode, UnixMs,
};
use crate::watcher::ScanTickRequest;

/// Module-level tracing target (SPEC s0 logging convention).
const TARGET: &str = "driven::core::orchestrator";

/// Capacity of the orchestrator's [`OrchestratorEvent`] broadcast channel.
/// A lagged consumer re-reads [`Orchestrator::state`] rather than treating a
/// dropped event as data loss (see [`OrchestratorEvent`] docs).
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Capacity of the watcher scan-tick mpsc the run loop consumes (DESIGN
/// s5.9.1). The watcher already debounces + rate-caps to one request per
/// minute per source, so a small buffer is ample; a full buffer simply
/// drops the surplus tick (the next scan re-derives the diff anyway).
const WATCHER_CHANNEL_CAPACITY: usize = 64;

/// The DESIGN s5.10.3 step 1 resume defer: after a wake, real-world network
/// and keychain services are not yet ready, so the orchestrator waits this
/// long (measured on the injected [`Clock`]) before re-probing and resuming.
const RESUME_DEFER_MS: i64 = 30_000;

/// Settings key holding the [`driven_vss::OrphanRegistry`] JSON - the ledger of
/// Driven-created VSS shadow copies, the cleanup authority for the >1h orphan
/// sweep (ROADMAP M3.5). Not in SPEC s22's enumerated keys; an internal
/// bookkeeping value the orchestrator owns end-to-end.
const VSS_ORPHAN_SETTING_KEY: &str = "vss.orphans";

/// Process-wide lock serializing the `vss.orphans` registry read-modify-write
/// (P2-D). Two account orchestrators in one process share ONE settings store;
/// without serialization their concurrent read -> merge -> write races and one
/// account's snapshot record is lost (last writer wins over a STALE read). A
/// `tokio::sync::Mutex` (held across the `.await`s of the DB read + write) makes
/// each account's whole RMW atomic with respect to the others. `OnceLock` so
/// every orchestrator in the process shares the same instance.
fn orphan_registry_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// A per-orchestrator in-memory ledger of shadow copies recorded SYNCHRONOUSLY
/// at create time (P1-A). The provider's record-at-create hook pushes a
/// `(guid -> created_ms)` entry here the instant a shadow is created - before
/// the (possibly long) locked-file upload - so the next `record_vss_orphans`
/// flushes it to the durable registry even if a crash falls between create and
/// the per-source record. Drained (not just read) on flush so it never grows
/// unbounded. `std::sync::Mutex` because the hook is SYNC (it runs inline on the
/// executor's blocking-friendly map path and must not await). Owned per
/// orchestrator (not a process-wide static) so concurrent accounts - and tests
/// running in parallel - never drain each other's entries; the DURABLE
/// `vss.orphans` registry is the only shared, cross-account state (serialized by
/// [`orphan_registry_lock`]).
type OrphanCreateLedger = Arc<std::sync::Mutex<std::collections::HashMap<String, i64>>>;

/// Push a freshly-created shadow GUID into `ledger` (P1-A). Sync + cheap;
/// called from the provider's record-at-create hook.
fn record_orphan_at_create(ledger: &OrphanCreateLedger, guid: &str, created_ms: i64) {
    let mut ledger = match ledger.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    ledger.entry(guid.to_string()).or_insert(created_ms);
}

/// Drain + return every entry `ledger` holds (P1-A flush).
fn drain_orphan_create_ledger(ledger: &OrphanCreateLedger) -> Vec<(String, i64)> {
    let mut ledger = match ledger.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    ledger.drain().collect()
}

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
    /// Windows VSS mode (SPEC s22 `windows.vss_mode`): `auto` (snapshot a
    /// locked file's volume on demand), `always` (snapshot every read), or
    /// `never` (skip locked files). Read once per cycle and applied to the
    /// per-cycle snapshot provider (ROADMAP M3.5, DESIGN s5.3). Inert off
    /// Windows / when un-elevated. The persisted source of truth is the
    /// `windows` settings key, wired in by the app shell (M5/M6); the field is
    /// here now so the orchestrator honours it.
    pub vss_mode: VssMode,
}

impl Default for OrchestratorConfig {
    /// Conservative, gate-respecting defaults (DESIGN s5.7, s5.9, SPEC s9):
    /// no dry-run, skip on battery + metered, 15-minute scan interval, no
    /// bandwidth cap, default pacer ceilings, VSS `auto`.
    fn default() -> Self {
        Self {
            dry_run: false,
            skip_on_battery: true,
            skip_on_metered: true,
            scan_interval_secs: 15 * 60,
            bandwidth_cap_mbps: None,
            pacer_ceilings: PacerCeilings::default(),
            vss_mode: VssMode::Auto,
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
/// Tracks startup-reconcile completion per source (DESIGN s5.6, P1-1).
///
/// Holds the set of source ids that have reconciled SUCCESSFULLY. A source
/// whose reconcile failed is simply absent from the set and is retried on the
/// next cycle; the whole pass is considered complete only once every
/// currently-enabled source id is present.
#[derive(Debug, Default)]
struct ReconcileProgress {
    /// Source ids whose startup reconcile completed without error.
    done: std::collections::HashSet<crate::types::SourceId>,
}

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
    /// Startup-reconcile progress (DESIGN s5.6). Tracks which sources have
    /// been reconciled SUCCESSFULLY so the pass is idempotent and a transient
    /// Drive/DB error on one source does not permanently disable
    /// reconciliation (P1-1): only the sources that succeeded are skipped next
    /// cycle, the failed ones are retried. `None` means "not yet started";
    /// `Some(set)` accumulates the source ids that have reconciled. The pass
    /// is fully done once every currently-enabled source is in the set.
    /// `Mutex` not `RwLock` because the read-check-and-insert must be atomic.
    reconciled: Mutex<ReconcileProgress>,
    /// Manual out-of-band trigger (SPEC s5 "Sync now", DESIGN s5.1).
    /// Capacity-1 mpsc: a `try_send` that finds the buffer full means a
    /// trigger is already pending, so the surplus is COALESCED into the one
    /// queued follow-up (the run loop runs exactly one extra cycle no matter
    /// how many triggers land mid-cycle). The receiver is taken once by
    /// [`Orchestrator::run`].
    trigger_tx: mpsc::Sender<TickSource>,
    trigger_rx: Mutex<Option<mpsc::Receiver<TickSource>>>,
    /// Debounced watcher scan-tick stream (DESIGN s5.9.1). The app shell
    /// bridges the real [`SourceWatcher::subscribe`](crate::watcher::SourceWatcher::subscribe)
    /// receiver into `watcher_tx`; tests push [`ScanTickRequest`]s directly.
    /// The run loop owns the single consumer (taken once).
    watcher_tx: mpsc::Sender<ScanTickRequest>,
    watcher_rx: Mutex<Option<mpsc::Receiver<ScanTickRequest>>>,
    /// Graceful-shutdown signal (SPEC s5 "run until cancelled"). The run loop
    /// selects on a change; the current in-flight cycle is allowed to finish
    /// (DESIGN s5.10.2) before the loop returns `Ok(())`.
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    /// The per-cycle Windows VSS snapshot provider (ROADMAP M3.5, DESIGN
    /// s5.3), or `None` when VSS is disabled (the historical behaviour off
    /// Windows / when un-elevated). The orchestrator owns the snapshot
    /// LIFECYCLE: it releases every per-cycle snapshot via
    /// [`VssProvider::end_cycle`] after the per-source loop (on EVERY exit
    /// path), and the executor (holding a CLONE of this same `Arc`) reads
    /// locked files from the snapshots in between. Set via [`Self::with_vss`].
    vss: Option<Arc<dyn VssProvider>>,
    /// Per-orchestrator record-at-create ledger (P1-A). The recorder hook wired
    /// into the provider by [`Self::with_vss`] pushes each freshly-created
    /// shadow GUID here synchronously; `record_vss_orphans` drains it into the
    /// durable registry. Owned (not global) so concurrent accounts do not drain
    /// each other's entries.
    vss_create_ledger: OrphanCreateLedger,
    /// Guards the startup orphan-snapshot cleanup so it runs at most once per
    /// process (ROADMAP M3.5: release Driven-created shadows >1h old that an
    /// unclean shutdown stranded). `Mutex<bool>` not an atomic so the
    /// check-then-set is a single critical section.
    orphan_cleanup_done: Mutex<bool>,
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
        // Capacity-1 manual trigger so a `try_send` coalesces a burst of
        // mid-cycle "Sync now" clicks into exactly one queued follow-up.
        let (trigger_tx, trigger_rx) = mpsc::channel(1);
        let (watcher_tx, watcher_rx) = mpsc::channel(WATCHER_CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
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
            reconciled: Mutex::new(ReconcileProgress::default()),
            trigger_tx,
            trigger_rx: Mutex::new(Some(trigger_rx)),
            watcher_tx,
            watcher_rx: Mutex::new(Some(watcher_rx)),
            shutdown_tx,
            shutdown_rx,
            vss: None,
            vss_create_ledger: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            orphan_cleanup_done: Mutex::new(false),
        }
    }

    /// Attach the per-cycle Windows VSS snapshot provider (ROADMAP M3.5).
    ///
    /// Pass the SAME `Arc<dyn VssProvider>` that was threaded into the
    /// executor's [`ExecutorDeps`](crate::executor::ExecutorDeps) so the
    /// orchestrator's `end_cycle` release and the executor's snapshot reads
    /// share one provider. Off Windows / when un-elevated the provider reports
    /// unavailable and every cycle degrades exactly as the no-VSS path does.
    pub fn with_vss(mut self, vss: Arc<dyn VssProvider>) -> Self {
        // P1-A: wire the record-at-create hook so a shadow's GUID lands in this
        // orchestrator's create-ledger the instant it is created (before the
        // ensuing locked-file upload), then flush to the durable registry on the
        // next per-source record. The hook is sync (the provider calls it inline
        // on its blocking-friendly map path), so it only touches the in-memory
        // ledger - never the async settings store.
        let ledger = self.vss_create_ledger.clone();
        vss.set_recorder(Arc::new(move |guid: &str, created_ms: i64| {
            record_orphan_at_create(&ledger, guid, created_ms);
        }));
        self.vss = Some(vss);
        self
    }

    /// Release any Driven-created shadow copies older than one hour that an
    /// unclean shutdown (`kill -9`, power loss) stranded - the RAII [`Drop`]
    /// never ran for those (ROADMAP M3.5 acceptance). Runs at most once per
    /// process, before the first cycle does any snapshot work.
    ///
    /// Ownership is proven, never guessed: the persisted [`OrphanRegistry`]
    /// (settings key `vss.orphans`, keyed by shadow GUID + creation time) is
    /// the cleanup authority. We release ONLY recorded GUIDs older than the >1h
    /// cutoff via `DeleteSnapshots` (the COM call is Windows + elevated only; a
    /// not-found shadow is an idempotent no-op), then drop the pruned entries
    /// from the registry. We never enumerate or heuristically guess ownership.
    async fn cleanup_orphan_snapshots_once(&self) {
        {
            let mut done = self.orphan_cleanup_done.lock().await;
            if *done {
                return;
            }
            *done = true;
        }
        // P2-D: serialize the whole read -> delete -> write against any
        // concurrent account orchestrator's registry RMW so neither clobbers the
        // other's view with a stale write.
        let _guard = orphan_registry_lock().lock().await;
        // Read the persisted registry (empty when absent / malformed).
        let mut registry = self.read_vss_orphan_registry().await;
        if registry.snapshots.is_empty() {
            return;
        }
        let now = self.clock.now_ms();
        let orphans = registry.orphans_older_than(now, driven_vss::DEFAULT_ORPHAN_MAX_AGE_MS);
        if orphans.is_empty() {
            return;
        }
        tracing::info!(
            target: TARGET,
            account_id = %self.account_id,
            count = orphans.len(),
            "VSS: releasing orphaned shadow copies (>1h, unclean-shutdown leftovers)"
        );
        for id in &orphans {
            match driven_vss::VssSnapshot::delete_by_id(id) {
                // Released, or already gone, or off-Windows/un-elevated (the
                // stub errors `Unavailable`): in every case drop the entry so a
                // permanently-undeletable id never wedges the registry.
                Ok(()) => registry.forget(id),
                Err(driven_vss::VssError::Unavailable(_)) => {
                    // Off Windows / un-elevated: cannot delete now. Keep the
                    // entry so an elevated run can sweep it later.
                }
                Err(err) => {
                    tracing::warn!(target: TARGET, account_id = %self.account_id, snapshot = %id, %err, "VSS: orphan release failed; keeping for retry");
                }
            }
        }
        self.write_vss_orphan_registry(&registry).await;
    }

    /// Persist the snapshots the provider currently holds into the orphan
    /// registry (settings key `vss.orphans`), so a later run can release any
    /// this process's RAII drop fails to (a `kill -9` between create and
    /// `end_cycle`). Called after the per-source loop, BEFORE `end_cycle`
    /// releases them. A clean cycle's `end_cycle` releases them in-process; the
    /// registry is the safety net for the unclean case. Merges with any
    /// existing entries and de-dupes by GUID.
    /// Returns the GUIDs recorded (for the post-release `forget`).
    async fn record_vss_orphans(&self) -> Vec<String> {
        let Some(vss) = self.vss.as_ref() else {
            // No provider: still drain the create-ledger so a stray entry never
            // accumulates (in practice empty without a provider).
            drain_orphan_create_ledger(&self.vss_create_ledger);
            return Vec::new();
        };
        // P1-A: merge the snapshots the provider currently holds with any the
        // record-at-create hook captured (drained so they are recorded exactly
        // once). A shadow that was created and then released within the same
        // source - leaving `recorded_snapshots` empty - is still caught here.
        let mut to_record: std::collections::HashMap<String, i64> =
            drain_orphan_create_ledger(&self.vss_create_ledger)
                .into_iter()
                .collect();
        for snap in vss.recorded_snapshots() {
            to_record
                .entry(snap.snapshot_id)
                .or_insert(snap.created_at_ms);
        }
        if to_record.is_empty() {
            return Vec::new();
        }
        // P2-D: serialize the read-modify-write against concurrent accounts.
        let _guard = orphan_registry_lock().lock().await;
        let mut registry = self.read_vss_orphan_registry().await;
        let mut ids = Vec::with_capacity(to_record.len());
        for (id, created) in to_record {
            ids.push(id.clone());
            registry.record(id, created);
        }
        self.write_vss_orphan_registry(&registry).await;
        ids
    }

    /// Drop GUIDs from the registry once `end_cycle` released them in-process
    /// (the clean path), so the registry only ever holds shadows a crash
    /// stranded. No-op for an empty list.
    async fn forget_vss_orphans(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        // P2-D: serialize the read-modify-write against concurrent accounts.
        let _guard = orphan_registry_lock().lock().await;
        let mut registry = self.read_vss_orphan_registry().await;
        for id in ids {
            registry.forget(id);
        }
        self.write_vss_orphan_registry(&registry).await;
    }

    /// Read the persisted [`OrphanRegistry`] from the `vss.orphans` setting; an
    /// absent or malformed value yields an empty registry (never an error - a
    /// corrupt ledger must not wedge the cycle).
    async fn read_vss_orphan_registry(&self) -> driven_vss::OrphanRegistry {
        match self.state.get_setting(VSS_ORPHAN_SETTING_KEY).await {
            Ok(Some(value)) => serde_json::from_value(value).unwrap_or_default(),
            Ok(None) => driven_vss::OrphanRegistry::default(),
            Err(err) => {
                tracing::warn!(target: TARGET, account_id = %self.account_id, %err, "VSS: reading orphan registry failed; treating as empty");
                driven_vss::OrphanRegistry::default()
            }
        }
    }

    /// Write the [`OrphanRegistry`] back to the `vss.orphans` setting. A write
    /// failure is logged but never aborts the cycle.
    async fn write_vss_orphan_registry(&self, registry: &driven_vss::OrphanRegistry) {
        let value = match serde_json::to_value(registry) {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(target: TARGET, account_id = %self.account_id, %err, "VSS: serialising orphan registry failed");
                return;
            }
        };
        if let Err(err) = self.state.set_setting(VSS_ORPHAN_SETTING_KEY, &value).await {
            tracing::warn!(target: TARGET, account_id = %self.account_id, %err, "VSS: persisting orphan registry failed");
        }
    }

    /// Returns the watcher-tick sender so the owning app shell can bridge a
    /// real [`SourceWatcher`](crate::watcher::SourceWatcher)'s debounced
    /// receiver into the orchestrator's run loop (DESIGN s5.9.1). Tests use it
    /// to push a [`ScanTickRequest`] directly.
    pub fn watcher_sender(&self) -> mpsc::Sender<ScanTickRequest> {
        self.watcher_tx.clone()
    }

    /// Signals the run loop to shut down gracefully (SPEC s5). The current
    /// in-flight cycle finishes (DESIGN s5.10.2); then [`Orchestrator::run`]
    /// returns `Ok(())`.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
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
        //
        // P1-5: `probe()` is now breaker-aware - it skips any service whose
        // circuit breaker is Open (DESIGN s5.8.3), so this probe never hits a
        // known-down Drive every tick. The probe-before-breaker ordering here
        // is deliberate and unchanged: it keeps "whole network offline ->
        // Pause(Offline)" distinct from "network up, Drive down -> Backoff"
        // (the Drive breaker check below), preserving the DESIGN s5.7
        // precedence offline > metered > battery > breaker.
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
        let sources = self.state.list_enabled_sources_for(self.account_id).await?;

        // P1-1: reconcile only the sources that have NOT yet reconciled
        // successfully. The progress set is the durable-within-process record
        // of which sources are done; a source whose reconcile errors is left
        // out of the set and retried next cycle, so a transient first-cycle
        // failure never permanently disables reconciliation. Mark each source
        // done the moment its reconcile succeeds (not at the start), and
        // surface the first error AFTER attempting the rest.
        let mut first_err: Option<anyhow::Error> = None;
        for source in &sources {
            {
                // Skip sources already reconciled this process lifetime.
                if self.reconciled.lock().await.done.contains(&source.id) {
                    continue;
                }
            }
            tracing::debug!(target: TARGET, source_id = %source.id, "startup reconcile");
            match self.executor.reconcile(source).await {
                Ok(()) => {
                    self.reconciled.lock().await.done.insert(source.id);
                }
                Err(err) => {
                    tracing::warn!(
                        target: TARGET,
                        source_id = %source.id,
                        %err,
                        "startup reconcile failed; will retry next cycle"
                    );
                    if first_err.is_none() {
                        first_err = Some(err);
                    }
                }
            }
        }

        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Writes a durable `activity_log` ERROR row per NFC collision the planner
    /// dropped (SPEC s24 `local.unicode_collision`, the M2-deferred item in
    /// CODEX_NOTES.md).
    ///
    /// One row per colliding path so the Activity dashboard can list each
    /// clash; `event_type` carries the stable code and `message` carries the
    /// path. A failed write is logged but never aborts the cycle - the upload
    /// pipeline must not be held hostage by an activity-log hiccup. The source
    /// stays visibly degraded (the error row is surfaced) rather than the
    /// collider being silently skipped.
    async fn record_collisions(
        &self,
        source_id: crate::types::SourceId,
        collisions: &[RelativePath],
    ) {
        let now = self.clock.now_ms();
        for collision in collisions {
            tracing::warn!(target: TARGET, source_id = %source_id, path = %collision, "local.unicode_collision; skipping colliding file");
            let row = NewActivity {
                ts: now,
                source_id: Some(source_id),
                level: ActivityLevel::Error,
                event_type: "local.unicode_collision".to_string(),
                file_count: None,
                bytes: None,
                message: Some(collision.as_str().to_string()),
            };
            if let Err(err) = self.state.write_activity(row).await {
                tracing::warn!(target: TARGET, source_id = %source_id, path = %collision, %err, "failed to write unicode_collision activity row");
            }
        }
    }

    /// Writes a durable `activity_log` row per failed / re-queued op (recheck2
    /// P2) so a production user has per-file failure evidence rather than only
    /// tracing. A `Failed` op lands an Error row keyed by its error code; a
    /// `Skipped` op (re-queued, retries next cycle) lands a Warn row. `Done`
    /// ops are not recorded. Mirrors `record_collisions`; a write hiccup is
    /// logged but never aborts the cycle.
    async fn record_outcome_activity(
        &self,
        source_id: crate::types::SourceId,
        outcomes: &[OpOutcome],
    ) {
        let now = self.clock.now_ms();
        for outcome in outcomes {
            let (level, event_type, path) = match outcome {
                OpOutcome::Done { .. } => continue,
                OpOutcome::Failed {
                    relative_path,
                    code,
                } => (ActivityLevel::Error, code.to_string(), relative_path),
                OpOutcome::Skipped {
                    relative_path,
                    reason,
                } => (
                    ActivityLevel::Warn,
                    reason.error_code().to_string(),
                    relative_path,
                ),
            };
            let row = NewActivity {
                ts: now,
                source_id: Some(source_id),
                level,
                event_type,
                file_count: None,
                bytes: None,
                message: Some(path.as_str().to_string()),
            };
            if let Err(err) = self.state.write_activity(row).await {
                tracing::warn!(target: TARGET, source_id = %source_id, %err, "failed to write outcome activity row");
            }
        }
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
        // a DURABLE activity_log ERROR row (not just a trace line) so the
        // Activity dashboard shows the source as degraded until the user
        // resolves the NFC-normalized name clash. V1 policy is
        // skip-the-colliding-file (the planner already emitted no op), not
        // fail-closed on the whole source.
        if !plan.collisions.is_empty() {
            self.record_collisions(source.id, &plan.collisions).await;
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
        self.record_outcome_activity(source.id, &outcomes).await;
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

        // P2-7 / recheck2 P1: persist the completion timestamps ONLY when the
        // scan + execute (and, on a deep-verify cycle, the verify pass) ALL
        // succeeded for this source. Advancing `last_full_scan_at` /
        // `last_deep_verify_at` while an op failed is a data-loss trap: a
        // deep-verify re-hash mismatch whose re-upload then failed leaves the
        // old `file_state` (matching size+mtime) intact, so the fast-scan path
        // skips the file, while an advanced `last_deep_verify_at` stops
        // `deep_verify_due` from re-checking it for a whole interval - the
        // changed/corrupt bytes would not retry for days. So if ANY op failed
        // we leave the timestamps unadvanced: the source stays scan/verify-due
        // and retries next cycle, and the durable activity-log error rows
        // (recorded above) surface it. A dry run already returned early.
        let any_failed = outcomes
            .iter()
            .any(|o| matches!(o, OpOutcome::Failed { .. }));
        if any_failed {
            let failed = outcomes
                .iter()
                .filter(|o| matches!(o, OpOutcome::Failed { .. }))
                .count();
            tracing::warn!(target: TARGET, source_id = %source.id, failed, "deferring scan/verify timestamp advance: failed op(s) keep the source due so the next cycle retries them");
            return Ok(());
        }
        let now = self.clock.now_ms();
        let deep_verify_at = if deep_verify { Some(now) } else { None };
        if let Err(err) = self
            .state
            .mark_source_scanned(source.id, now, deep_verify_at)
            .await
        {
            tracing::warn!(target: TARGET, source_id = %source.id, %err, "failed to persist scan/verify timestamps");
        }
        if let Err(err) = self.state.mark_account_synced(self.account_id, now).await {
            tracing::warn!(target: TARGET, account_id = %self.account_id, %err, "failed to persist account last_synced_at");
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
        // P1-6 (DESIGN s5.6, s5.7): the startup reconcile (DESIGN s5.6) is a
        // REMOTE pass - it issues Drive find/metadata calls to adopt orphaned
        // objects - so it MUST come AFTER the power / network / manual gates,
        // never before. Evaluating the gates first guarantees an offline /
        // battery / metered / paused / breaker-open cycle issues ZERO remote
        // calls (the load-bearing "offline -> zero remote calls" invariant).
        // The reconcile is split into a local-only phase that always runs and a
        // remote phase gated open below; in M3 the local-only phase is empty
        // (the executor's reconcile is entirely remote), so there is nothing to
        // run before the gate, but the seam is here for M4's local reconcile.
        self.transition(OrchestratorState::PowerCheck).await;

        // P1 (M3.5 recheck2): apply the CURRENT vss_mode to the attached provider
        // before any VSS path runs this cycle. `with_vss` only installs the
        // recorder, and the provider's construction mode can differ from the
        // persisted/reconfigured config, so without this a startup
        // `vss_mode = never` could still create snapshots and `always` could
        // silently behave as `auto` until the first `reconfigure`.
        if let Some(vss) = self.vss.as_ref() {
            vss.set_mode(self.config.read().await.vss_mode);
        }

        // P1-4 (M3.5 codex): release any orphaned VSS shadow copies an unclean
        // shutdown stranded, once per process. This is a LOCAL operation (it
        // issues no Drive call), so it MUST run BEFORE the gates - a start that
        // is offline / paused / metered / on-battery must still sweep stale
        // shadows, or a leaked snapshot survives until the next gates-open run.
        self.cleanup_orphan_snapshots_once().await;

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

        // Remote reconcile phase (DESIGN s5.6): now that the gates are open we
        // may safely issue Drive calls. Guarded to run at most once before the
        // first executing cycle.
        self.reconcile_once().await?;

        // Run every enabled source, then ALWAYS release this cycle's VSS
        // snapshots (RAII via `end_cycle`) - even when a source errored
        // mid-loop. The cycle owns the snapshot lifecycle (ROADMAP M3.5): one
        // snapshot per volume, reused across sources, released here. Capturing
        // the loop result before releasing keeps `end_cycle` off the `?` early
        // return that would otherwise leak the shadow copies until next
        // startup's orphan sweep.
        let sources = self.state.list_enabled_sources_for(self.account_id).await?;
        let loop_result: anyhow::Result<()> = async {
            for source in &sources {
                let deep_verify = self.deep_verify_due(source);
                self.run_one_source(source, deep_verify).await?;
                // P1-2 (M3.5 codex): durably record any shadow copy this source
                // just created BEFORE moving to the next source, so a crash
                // strands at most one source's snapshot rather than every
                // snapshot created so far this cycle. `record_vss_orphans` is a
                // no-op when no provider is attached or none are held.
                self.record_vss_orphans().await;

                // P2-E: if a manual pause was signalled mid-cycle, stop AT THIS
                // safe boundary (current op done + recorded, before the next
                // source) so a held shadow copy is released promptly by the
                // post-loop `end_vss_cycle` rather than lingering for the rest
                // of a long multi-source / huge-locked-file cycle. The gate
                // check already keeps the NEXT cycle paused; this only shortens
                // how long the CURRENT cycle holds snapshots.
                if *self.pause_tx.borrow() {
                    tracing::info!(target: TARGET, account_id = %self.account_id, "manual pause mid-cycle; releasing VSS snapshots at source boundary");
                    break;
                }
            }
            Ok(())
        }
        .await;
        // Persist this cycle's shadow copies into the orphan registry BEFORE
        // releasing them, so a `kill -9` between here and `end_cycle` leaves a
        // durable record the next startup's >1h sweep can release. Release
        // in-process (the clean path), then forget the just-released GUIDs so
        // the registry does not accumulate already-gone entries. A crash
        // between record and forget leaves them in the registry - exactly the
        // safety net we want. All three run on every exit path (incl. a
        // mid-loop error).
        let recorded = self.record_vss_orphans().await;
        self.end_vss_cycle();
        self.forget_vss_orphans(&recorded).await;
        loop_result?;

        self.transition(OrchestratorState::Idle {
            last_run_at: Some(self.clock.now_ms()),
        })
        .await;
        Ok(())
    }

    /// Release every VSS snapshot created during this cycle (ROADMAP M3.5).
    /// Idempotent + a no-op when VSS is disabled; called after the per-source
    /// loop on every exit path.
    fn end_vss_cycle(&self) {
        if let Some(vss) = self.vss.as_ref() {
            vss.end_cycle();
        }
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

/// Awaits the next item from an optional mpsc receiver inside a
/// `tokio::select!` arm.
///
/// When the receiver is present, this resolves to whatever `recv()` returns
/// (`Some(item)` or `None` on channel close). When the receiver is absent
/// (already taken / a closed branch the loop set to `None`), it parks forever
/// via [`std::future::pending`] so the select arm is inert rather than busy-
/// resolving to `None` in a tight loop. `&mut Option<Receiver>` keeps the
/// receiver borrowed across the await without moving it out of the loop's
/// local state.
async fn recv_opt<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Awaits the next power-state snapshot from an optional broadcast receiver
/// inside a `tokio::select!` arm.
///
/// `Closed` means the [`PowerSource`] was dropped; this drops the receiver
/// (`*rx = None`) so subsequent polls park forever via
/// [`std::future::pending`] instead of busy-resolving `Closed` in a hot loop.
/// `Lagged` is surfaced to the caller (a benign re-read trigger). An absent
/// receiver parks forever, leaving the select arm inert.
async fn power_recv_opt(
    rx: &mut Option<broadcast::Receiver<driven_power::PowerState>>,
) -> Result<driven_power::PowerState, broadcast::error::RecvError> {
    match rx {
        Some(receiver) => {
            let result = receiver.recv().await;
            if matches!(result, Err(broadcast::error::RecvError::Closed)) {
                *rx = None;
            }
            result
        }
        None => std::future::pending().await,
    }
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
        // The real long-lived continuous-backup engine (SPEC s5, DESIGN s5.1).
        // A SINGLE `tokio::select!` loop multiplexes every wake source:
        //   (a) the scheduled-scan interval timer (the authoritative fallback),
        //   (b) debounced watcher scan-ticks (DESIGN s5.9.1),
        //   (c) the manual "Sync now" trigger (capacity-1, coalescing),
        //   (d) OS power-state transitions (battery/AC, suspend/resume),
        //   (e) network-state transitions (folded into the power branch for M3;
        //       the real probe-event seam is deferred to M4 - see CODEX_NOTES),
        //   (f) the graceful-shutdown signal.
        //
        // In-flight guard: there is exactly ONE `run()` task, and each selected
        // wake runs `run_cycle(..).await` INLINE. A single task awaiting inline
        // can never overlap two cycles - while a cycle runs, further wakes
        // simply buffer in their channels. The capacity-1 trigger channel caps
        // a mid-cycle burst of "Sync now" clicks to exactly ONE queued
        // follow-up. Shutdown is graceful: it is observed only between cycles
        // (the select arms), so the current cycle always finishes first
        // (DESIGN s5.10.2).
        let mut watcher_rx = self.watcher_rx.lock().await.take();
        let mut trigger_rx = self.trigger_rx.lock().await.take();
        let mut power_rx = Some(self.power.subscribe());
        let mut shutdown_rx = self.shutdown_rx.clone();

        // The scheduled interval reads the config at spawn time. A reconfigure
        // takes effect on the next `run()`; within a run the cadence is fixed
        // (re-deriving it per tick would reset the timer and could starve the
        // scan). `Skip` (not the default `Burst`) so a cycle that overruns one
        // period does not storm catch-up ticks afterwards.
        let interval_secs = self.config.read().await.scan_interval_secs.max(1);
        let mut scheduled = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        scheduled.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the immediate first tick so the loop does not run a cycle the
        // instant it is spawned purely from the interval (a watcher/manual
        // trigger or the next period drives the first real cycle).
        scheduled.tick().await;

        loop {
            // Pick the next wake. Each arm yields an `Option<TickSource>`:
            // `Some(tick)` runs a cycle; `None` means "loop control only"
            // (a closed branch we drop, or shutdown which breaks below).
            let next: Option<TickSource> = tokio::select! {
                _ = scheduled.tick() => Some(TickSource::Scheduled),

                req = recv_opt(&mut watcher_rx) => match req {
                    Some(req) => {
                        tracing::debug!(target: TARGET, account_id = %self.account_id, source_id = %req.source_id, reason = ?req.reason, "watcher scan-tick");
                        Some(TickSource::Watcher)
                    }
                    // Watcher channel closed: drop the branch, keep running on
                    // the scheduled fallback (DESIGN s5.9.4).
                    None => { watcher_rx = None; None }
                },

                trig = recv_opt(&mut trigger_rx) => match trig {
                    Some(reason) => Some(reason),
                    None => { trigger_rx = None; None }
                },

                recvd = power_recv_opt(&mut power_rx) => match recvd {
                    Ok(state) => {
                        // A steady-state power/network transition (battery<->AC,
                        // metered toggling, reachable<->offline) just asks for a
                        // re-evaluation: run a cycle whose gate check PAUSES on
                        // battery/offline and PROCEEDS (resumes) on AC/online.
                        // No special-casing here - the gates are the single
                        // source of truth (DESIGN s5.7). Genuine sleep/wake edges
                        // arrive as `PowerEvent`s on a separate path
                        // (`on_power_event`), not on this steady-state channel,
                        // so we do NOT synthesize one here.
                        tracing::debug!(target: TARGET, account_id = %self.account_id, ac = state.ac_connected, reachable = state.network_reachable, metered = state.on_metered_network, "power/network transition; re-evaluating gates");
                        Some(TickSource::Scheduled)
                    }
                    // `Lagged` is benign: we missed an intermediate snapshot but
                    // the next cycle's gate check re-reads `current()` (the
                    // documented recovery contract), so still run a cycle.
                    Err(broadcast::error::RecvError::Lagged(_)) => Some(TickSource::Scheduled),
                    // Closed: the source was dropped. `power_recv_opt` has set
                    // the receiver to `None`, so this arm is now inert and the
                    // scheduled loop keeps running.
                    Err(broadcast::error::RecvError::Closed) => None,
                },

                res = shutdown_rx.changed() => {
                    // `changed()` resolves on a flip OR on sender drop; either
                    // way, if the flag is set we exit. A sender-drop without a
                    // set flag also means "no one can ever signal again", so we
                    // exit cleanly rather than spin.
                    match res {
                        Ok(()) if *shutdown_rx.borrow() => {
                            tracing::info!(target: TARGET, account_id = %self.account_id, "shutdown signalled; exiting run loop");
                            break;
                        }
                        Ok(()) => None,
                        Err(_) => {
                            tracing::info!(target: TARGET, account_id = %self.account_id, "shutdown sender dropped; exiting run loop");
                            break;
                        }
                    }
                }
            };

            if let Some(tick) = next {
                // Inline await = the in-flight guard. A failed cycle is logged,
                // never fatal: the next tick retries and the Error surfaces via
                // the activity log + state machine.
                if let Err(err) = self.run_cycle(tick).await {
                    tracing::warn!(target: TARGET, account_id = %self.account_id, ?tick, %err, "cycle failed; continuing");
                }
            }
        }

        Ok(())
    }

    async fn trigger(&self, reason: TickSource) {
        // Out-of-band cycle request. Hand it to the run loop rather than run a
        // cycle inline here: running `run_cycle` directly from `trigger()` while
        // the loop is already mid-cycle would start a SECOND concurrent cycle -
        // the exact overlap the single-inflight guard exists to prevent.
        //
        // `try_send` into the capacity-1 channel: a full buffer means a trigger
        // is already queued, so this one is COALESCED into that single pending
        // follow-up (DESIGN s5.1). If the loop is not running (no receiver), the
        // send errors and is dropped - the next scheduled tick covers it.
        match self.trigger_tx.try_send(reason) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!(target: TARGET, account_id = %self.account_id, ?reason, "trigger coalesced into pending follow-up");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!(target: TARGET, account_id = %self.account_id, ?reason, "trigger dropped; run loop not active");
            }
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
        // P1-5 (M3.5 codex): thread the (possibly changed) VSS mode to the
        // attached provider so `vss_mode = never` actually disables snapshots
        // and `always` actually forces them - the setting was previously inert
        // because the provider froze its mode at construction.
        if let Some(vss) = self.vss.as_ref() {
            vss.set_mode(config.vss_mode);
        }
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
        /// When `> 0`, the next `reconcile` call returns a transient error and
        /// decrements this counter WITHOUT recording the source as adopted -
        /// drives the P1-1 "first reconcile fails, retried next cycle" test.
        reconcile_failures_remaining: AtomicU64,
        /// When `> 0`, every `execute` op returns `OpOutcome::Failed` (a
        /// `DriveChecksumMismatch`) instead of `Done` - drives the recheck2
        /// "failed op defers the timestamp advance + records activity" test.
        fail_ops: AtomicU64,
        /// When `true`, `execute` returns `Err` (a hard error that the cycle's
        /// `?` propagates) - drives the M3.5 "VSS released even on a mid-loop
        /// error" test.
        execute_returns_err: std::sync::atomic::AtomicBool,
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
            if self.execute_returns_err.load(Ordering::SeqCst) {
                return Err(anyhow::anyhow!("forced execute error (test)"));
            }
            // Report a progress tick and a Done outcome per op.
            on_progress(ExecProgress {
                files_total: plan.ops.len() as u64,
                ..ExecProgress::zero()
            });
            let fail = self.fail_ops.load(Ordering::SeqCst) > 0;
            let outcomes = plan
                .ops
                .iter()
                .map(|op| {
                    let relative_path = match op {
                        crate::types::Op::HashThenUpload { relative_path, .. } => {
                            relative_path.clone()
                        }
                        crate::types::Op::Trash { relative_path, .. } => relative_path.clone(),
                    };
                    if fail {
                        OpOutcome::Failed {
                            relative_path,
                            code: crate::types::ErrorCode::DriveChecksumMismatch,
                        }
                    } else {
                        OpOutcome::Done { relative_path }
                    }
                })
                .collect();
            Ok(outcomes)
        }

        async fn reconcile(&self, source: &SourceRow) -> anyhow::Result<()> {
            // Count every attempt. A configured transient failure returns Err
            // WITHOUT recording the source as adopted, so the P1-1 test can
            // assert the failed source is retried on the next cycle.
            self.reconciles.fetch_add(1, Ordering::SeqCst);
            if self.reconcile_failures_remaining.load(Ordering::SeqCst) > 0 {
                self.reconcile_failures_remaining
                    .fetch_sub(1, Ordering::SeqCst);
                return Err(anyhow::anyhow!("transient reconcile error"));
            }
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
        /// Records every `write_activity` so the collision test can assert the
        /// durable `local.unicode_collision` ERROR row was written.
        activity: StdMutex<Vec<NewActivity>>,
        /// Records every `mark_account_synced` for the P2-7 assertion.
        account_synced: StdMutex<Vec<(AccountId, UnixMs)>>,
        /// In-memory settings k/v (used by the M3.5 VSS orphan-registry tests).
        settings: StdMutex<HashMap<String, serde_json::Value>>,
    }

    impl FakeState {
        fn with_sources(sources: Vec<SourceRow>) -> Self {
            Self {
                sources: StdMutex::new(sources),
                files: StdMutex::new(HashMap::new()),
                activity: StdMutex::new(Vec::new()),
                account_synced: StdMutex::new(Vec::new()),
                settings: StdMutex::new(HashMap::new()),
            }
        }

        /// Snapshot the current source rows (post-persist) for the P2-7 test.
        fn sources_snapshot(&self) -> Vec<SourceRow> {
            self.sources.lock().unwrap().clone()
        }

        /// Snapshot the recorded activity rows.
        fn activity_rows(&self) -> Vec<NewActivity> {
            self.activity.lock().unwrap().clone()
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
        async fn mark_account_synced(&self, id: AccountId, at: UnixMs) -> anyhow::Result<()> {
            self.account_synced.lock().unwrap().push((id, at));
            Ok(())
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
        async fn mark_source_scanned(
            &self,
            id: SourceId,
            full_scan_at: UnixMs,
            deep_verify_at: Option<UnixMs>,
        ) -> anyhow::Result<()> {
            // Mutate the in-memory source rows so a subsequent
            // `list_enabled_sources_for` observes the persisted timestamps
            // (matching the COALESCE semantics of the sqlite impl: a `None`
            // deep_verify_at leaves the existing value).
            let mut sources = self.sources.lock().unwrap();
            for source in sources.iter_mut() {
                if source.id == id {
                    source.last_full_scan_at = Some(full_scan_at);
                    if let Some(v) = deep_verify_at {
                        source.last_deep_verify_at = Some(v);
                    }
                }
            }
            Ok(())
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
        async fn write_activity(&self, row: NewActivity) -> anyhow::Result<ActivityId> {
            let mut log = self.activity.lock().unwrap();
            log.push(row);
            Ok(ActivityId(log.len() as i64))
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
        async fn get_setting(&self, key: &str) -> anyhow::Result<Option<serde_json::Value>> {
            Ok(self.settings.lock().unwrap().get(key).cloned())
        }
        async fn set_setting(&self, key: &str, value: &serde_json::Value) -> anyhow::Result<()> {
            self.settings
                .lock()
                .unwrap()
                .insert(key.to_string(), value.clone());
            Ok(())
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

    // --- M3.5 VSS per-cycle lifecycle ---------------------------------------

    /// The orchestrator releases the cycle's VSS snapshots via
    /// [`VssProvider::end_cycle`] after the per-source loop completes (ROADMAP
    /// M3.5: one snapshot per volume, reused across sources, released at cycle
    /// end). Asserts on the FakeVss release counter.
    #[tokio::test]
    async fn vss_snapshots_released_at_cycle_end() {
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
        let vss = Arc::new(driven_vss::FakeVssProvider::unavailable());
        let orch = orch.with_vss(vss.clone());

        orch.run_cycle(TickSource::Scheduled).await.unwrap();
        assert_eq!(
            vss.end_cycle_calls(),
            1,
            "end_cycle must be called exactly once after the source loop"
        );

        // A second cycle releases again (per-cycle lifecycle).
        orch.run_cycle(TickSource::Scheduled).await.unwrap();
        assert_eq!(vss.end_cycle_calls(), 2);
    }

    /// recheck2 P1: `with_vss` only installs the recorder, so the provider's
    /// CONSTRUCTION mode can differ from the persisted/reconfigured config.
    /// `run_cycle` must apply the current `config.vss_mode` to the provider
    /// before any VSS path - otherwise a startup `vss_mode = never` would let
    /// an `Auto`-constructed provider still snapshot.
    #[tokio::test]
    async fn run_cycle_applies_config_vss_mode_to_provider() {
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        let config = OrchestratorConfig {
            vss_mode: driven_vss::VssMode::Never,
            ..OrchestratorConfig::default()
        };
        let (orch, _clock) = build(
            account,
            vec![src],
            exec.clone(),
            power_on_ac(),
            Arc::new(FakeNet::online()),
            config,
        );
        // Provider CONSTRUCTED as `Auto` - deliberately different from the
        // config's `Never`, so the test proves run_cycle applied the config.
        let vss = Arc::new(driven_vss::FakeVssProvider::mapped_under(
            driven_vss::VssMode::Auto,
            "/snap",
        ));
        assert_eq!(vss.mode(), driven_vss::VssMode::Auto, "constructed as Auto");
        let orch = orch.with_vss(vss.clone());

        orch.run_cycle(TickSource::Scheduled).await.unwrap();

        assert_eq!(
            vss.mode(),
            driven_vss::VssMode::Never,
            "run_cycle applies config.vss_mode (Never) to the provider"
        );
    }

    /// `end_cycle` runs even when a source errors mid-loop (the RAII-on-error
    /// contract: a `?` early return must not leak the cycle's shadow copies).
    /// Forces `execute` to return `Err`, which `run_one_source` propagates and
    /// `run_cycle`'s `?` would early-return on - the release must still happen.
    #[tokio::test]
    async fn vss_snapshots_released_even_when_a_source_errors() {
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        exec.execute_returns_err.store(true, Ordering::SeqCst);
        let (orch, _clock) = build(
            account,
            vec![src],
            exec.clone(),
            power_on_ac(),
            Arc::new(FakeNet::online()),
            OrchestratorConfig::default(),
        );
        let vss = Arc::new(driven_vss::FakeVssProvider::unavailable());
        let orch = orch.with_vss(vss.clone());

        let result = orch.run_cycle(TickSource::Scheduled).await;
        assert!(
            result.is_err(),
            "a forced execute error must fail the cycle"
        );
        assert_eq!(
            vss.end_cycle_calls(),
            1,
            "end_cycle must still run when a source errors mid-loop"
        );
    }

    /// A CLEAN cycle records this cycle's shadow copies into the `vss.orphans`
    /// registry (the crash safety net), releases them in-process via
    /// `end_cycle`, then forgets the released GUIDs - so the registry ends
    /// EMPTY after a clean cycle. The round-trip exercises the real
    /// `StateRepo::get_setting`/`set_setting` persistence wired in M3.5.
    #[tokio::test]
    async fn vss_clean_cycle_records_then_forgets_orphans() {
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        let state = Arc::new(FakeState::with_sources(vec![src]));
        let clock = Arc::new(FakeClock::new());
        let power = Arc::new(FakePowerSource::new(power_on_ac()));
        let orch = SyncOrchestrator::new(
            account,
            state.clone(),
            exec,
            power,
            Arc::new(FakeNet::online()),
            clock,
            OrchestratorConfig::default(),
        );
        // The fake reports one created snapshot this cycle.
        let recorded = vec![driven_vss::RecordedSnapshot {
            snapshot_id: "{deadbeef-0000-0000-0000-000000000001}".to_string(),
            created_at_ms: 0,
        }];
        let vss = Arc::new(driven_vss::FakeVssProvider::unavailable().with_recorded(recorded));
        let orch = orch.with_vss(vss);

        orch.run_cycle(TickSource::Scheduled).await.unwrap();

        // After a clean cycle the registry is empty (recorded then forgotten).
        let stored = state.get_setting(VSS_ORPHAN_SETTING_KEY).await.unwrap();
        if let Some(value) = stored {
            let reg: driven_vss::OrphanRegistry = serde_json::from_value(value).unwrap();
            assert!(
                reg.snapshots.is_empty(),
                "a clean cycle must leave no orphans in the registry; got {:?}",
                reg.snapshots
            );
        }
    }

    /// P1-2 (M3.5 codex): orphans are recorded PER SOURCE inside the loop, not
    /// only once after it, so a crash strands at most one source's just-created
    /// snapshot. Observable via `recorded_snapshots` call count: with N sources
    /// the orchestrator calls it once per source PLUS once after the loop =
    /// N + 1 (before the fix it was called exactly once). Two sources here, so
    /// the count must be 3.
    #[tokio::test]
    async fn vss_records_orphans_per_source_not_only_after_loop() {
        let account = AccountId::new_v4();
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        std::fs::write(dir_a.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir_b.path().join("b.txt"), b"b").unwrap();
        let sources = vec![
            source_in(account, dir_a.path()),
            source_in(account, dir_b.path()),
        ];
        let exec = Arc::new(RecordingExecutor::default());
        let state = Arc::new(FakeState::with_sources(sources));
        let clock = Arc::new(FakeClock::new());
        let power = Arc::new(FakePowerSource::new(power_on_ac()));
        let orch = SyncOrchestrator::new(
            account,
            state.clone(),
            exec,
            power,
            Arc::new(FakeNet::online()),
            clock,
            OrchestratorConfig::default(),
        );
        let recorded = vec![driven_vss::RecordedSnapshot {
            snapshot_id: "{deadbeef-0000-0000-0000-000000000002}".to_string(),
            created_at_ms: 0,
        }];
        let vss = Arc::new(driven_vss::FakeVssProvider::unavailable().with_recorded(recorded));
        let orch = orch.with_vss(vss.clone());

        orch.run_cycle(TickSource::Scheduled).await.unwrap();

        assert_eq!(
            vss.recorded_calls(),
            3,
            "two sources must record orphans per-source (2) plus once after the loop (1)"
        );
    }

    /// P1-A: a shadow recorded SYNCHRONOUSLY at create time (via the recorder
    /// hook the orchestrator wires into the provider) is flushed to the durable
    /// `vss.orphans` registry by the per-source `record_vss_orphans` - so a
    /// crash DURING the source's (long) locked-file upload still leaves the GUID
    /// recorded. We simulate the create by invoking the wired provider's
    /// `map_for_volume` (which fires the recorder into the process-wide
    /// create-ledger), then run a cycle and assert the GUID landed in the
    /// registry. The per-source record drains the ledger; the post-loop record
    /// + forget never sees that GUID, so it survives the clean cycle as the
    /// crash safety net.
    #[tokio::test]
    async fn vss_record_at_create_flushes_to_registry() {
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        let state = Arc::new(FakeState::with_sources(vec![src]));
        let clock = Arc::new(FakeClock::new());
        let power = Arc::new(FakePowerSource::new(power_on_ac()));
        let orch = SyncOrchestrator::new(
            account,
            state.clone(),
            exec,
            power,
            Arc::new(FakeNet::online()),
            clock,
            OrchestratorConfig::default(),
        );
        // A mapping provider: its `map_for_volume` simulates a created shadow
        // and fires the recorder the orchestrator wired in `with_vss`. It holds
        // NO `recorded_snapshots`, so ONLY the record-at-create path can put a
        // GUID in the registry.
        let snap_dir = tempfile::tempdir().unwrap();
        let vss = Arc::new(driven_vss::FakeVssProvider::mapped_under(
            driven_vss::VssMode::Always,
            snap_dir.path().to_path_buf(),
        ));
        let orch = orch.with_vss(vss.clone());

        // Simulate the executor consulting the provider for a locked file: this
        // fires the record-at-create hook synchronously into the ledger.
        let mapped = vss.map_for_volume(std::path::Path::new("C:/live/locked.pst"));
        assert!(matches!(mapped, driven_vss::SnapshotOutcome::Mapped(_)));

        orch.run_cycle(TickSource::Scheduled).await.unwrap();

        // The create-recorded GUID must be durably in the registry after the
        // clean cycle (it was recorded per-source and never forgotten).
        let stored = state
            .get_setting(VSS_ORPHAN_SETTING_KEY)
            .await
            .unwrap()
            .expect("registry persisted");
        let reg: driven_vss::OrphanRegistry = serde_json::from_value(stored).unwrap();
        assert!(
            reg.snapshots
                .iter()
                .any(|s| s.snapshot_id == "{fake-snapshot-guid}"),
            "the record-at-create GUID must be flushed to the registry; got {:?}",
            reg.snapshots
        );
    }

    /// Startup orphan cleanup releases recorded shadows older than the 1h
    /// cutoff. Off Windows / un-elevated `delete_by_id` returns `Unavailable`,
    /// so the old entry is KEPT for an elevated run (never silently dropped),
    /// while a fresh entry is never selected. Verifies the prune SELECTION + the
    /// once-per-process guard, the cross-OS-testable part of the sweep.
    #[tokio::test]
    async fn vss_startup_cleanup_selects_only_old_orphans() {
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        let src = source_in(account, dir.path());
        let exec = Arc::new(RecordingExecutor::default());
        let state = Arc::new(FakeState::with_sources(vec![src]));
        let clock = Arc::new(FakeClock::new());
        // Pre-seed the registry: one 2h-old orphan, one fresh.
        let now = clock.now_ms();
        let mut reg = driven_vss::OrphanRegistry::new();
        reg.record(
            "{old-orphan}",
            now - 2 * driven_vss::DEFAULT_ORPHAN_MAX_AGE_MS,
        );
        reg.record("{fresh}", now);
        state
            .set_setting(VSS_ORPHAN_SETTING_KEY, &serde_json::to_value(&reg).unwrap())
            .await
            .unwrap();

        let power = Arc::new(FakePowerSource::new(power_on_ac()));
        let orch = SyncOrchestrator::new(
            account,
            state.clone(),
            exec,
            power,
            Arc::new(FakeNet::online()),
            clock,
            OrchestratorConfig::default(),
        );
        // An AVAILABLE fake so cleanup runs its read+prune (delete_by_id then
        // hits the real off-Windows stub -> Unavailable -> keeps the entry).
        let vss = Arc::new(driven_vss::FakeVssProvider::unavailable().with_recorded(vec![]));
        let orch = orch.with_vss(vss);

        orch.run_cycle(TickSource::Scheduled).await.unwrap();

        // The registry still holds the old orphan (delete_by_id is Unavailable
        // off Windows, so it is kept for an elevated sweep) AND the fresh one.
        // The point asserted: the cleanup ran without dropping the fresh entry
        // and without panicking - the prune SELECTION is exercised.
        let stored = state
            .get_setting(VSS_ORPHAN_SETTING_KEY)
            .await
            .unwrap()
            .expect("registry persisted");
        let reg_after: driven_vss::OrphanRegistry = serde_json::from_value(stored).unwrap();
        assert!(
            reg_after
                .snapshots
                .iter()
                .any(|s| s.snapshot_id == "{fresh}"),
            "the fresh (sub-1h) orphan must never be selected for release"
        );
    }

    /// P2-E: a manual pause SIGNALLED MID-CYCLE releases the VSS snapshots at
    /// the next safe source boundary (after the current op, before the next
    /// source) - not only at full cycle end. With two sources and a pause
    /// flipped while source #1 is in flight, source #2 must NOT run and the
    /// cycle must still release VSS (`end_cycle`), so a huge locked-file cycle
    /// does not pin a shadow copy for its whole remaining length.
    #[tokio::test]
    async fn pause_mid_cycle_releases_vss_at_source_boundary() {
        let account = AccountId::new_v4();
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        std::fs::write(dir_a.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir_b.path().join("b.txt"), b"b").unwrap();
        let sources = vec![
            source_in(account, dir_a.path()),
            source_in(account, dir_b.path()),
        ];

        let executes = Arc::new(AtomicU64::new(0));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = tokio::sync::mpsc::unbounded_channel();
        let exec = Arc::new(BlockingExecutor {
            executes: executes.clone(),
            entered_tx,
            release_rx: tokio::sync::Mutex::new(release_rx),
        });

        let state = Arc::new(FakeState::with_sources(sources));
        let clock = Arc::new(FakeClock::new());
        let power = Arc::new(FakePowerSource::new(power_on_ac()));
        // Keep a typed clone so we can assert `end_cycle` was called.
        let vss = Arc::new(driven_vss::FakeVssProvider::unavailable());
        let orch = Arc::new(
            SyncOrchestrator::new(
                account,
                state,
                exec,
                power,
                Arc::new(FakeNet::online()),
                clock,
                OrchestratorConfig::default(),
            )
            .with_vss(vss.clone()),
        );

        // Run the cycle concurrently so the test can flip pause mid-flight.
        let cycle = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run_cycle(TickSource::Scheduled).await })
        };

        // Source #1 enters `execute`; pause now, then release it.
        tokio::time::timeout(std::time::Duration::from_secs(30), entered_rx.recv())
            .await
            .expect("source #1 must enter execute")
            .expect("entered channel open");
        orch.set_paused(true).await;
        let _ = release_tx.send(());

        // The cycle must finish (the pause check breaks the loop after source
        // #1) without source #2 ever entering `execute`.
        tokio::time::timeout(std::time::Duration::from_secs(30), cycle)
            .await
            .expect("cycle must finish promptly after the mid-cycle pause")
            .expect("join")
            .expect("run_cycle ok");

        assert_eq!(
            executes.load(Ordering::SeqCst),
            1,
            "the second source must NOT run after a mid-cycle pause"
        );
        assert!(
            entered_rx.try_recv().is_err(),
            "source #2 must not have entered execute"
        );
        // VSS was released this cycle despite the early break (the post-loop
        // `end_vss_cycle` ran), so a held shadow does not linger.
        assert!(
            vss.end_cycle_calls() >= 1,
            "the mid-cycle pause must still release VSS at cycle exit"
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
    async fn first_reconcile_error_is_retried_not_permanently_disabled() {
        // P1-1: a transient error on the startup reconcile must NOT
        // permanently disable reconciliation. The first cycle's reconcile
        // fails (and the cycle surfaces the error); the source stays
        // un-adopted, so the NEXT cycle retries the reconcile and adopts it.
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        let src = source_in(account, dir.path());
        let src_id = src.id;
        let exec = Arc::new(RecordingExecutor::default());
        // Fail exactly the first reconcile attempt.
        exec.reconcile_failures_remaining.store(1, Ordering::SeqCst);
        let (orch, _clock) = build(
            account,
            vec![src],
            exec.clone(),
            power_on_ac(),
            Arc::new(FakeNet::online()),
            OrchestratorConfig::default(),
        );

        // Cycle 1: reconcile fails, so the cycle returns the error and the
        // source is NOT marked reconciled.
        let first = orch.run_cycle(TickSource::Scheduled).await;
        assert!(
            first.is_err(),
            "a failed startup reconcile surfaces the error this cycle"
        );
        assert_eq!(
            exec.reconciles.load(Ordering::SeqCst),
            1,
            "reconcile was attempted once"
        );
        assert!(
            exec.reconciled_sources.lock().unwrap().is_empty(),
            "a failed reconcile must not record the source as adopted"
        );

        // Cycle 2: reconciliation is still PENDING (not permanently disabled),
        // so it is retried and now succeeds, adopting the orphan.
        orch.run_cycle(TickSource::Scheduled).await.unwrap();
        assert_eq!(
            exec.reconciles.load(Ordering::SeqCst),
            2,
            "the failed source's reconcile is retried on the next cycle"
        );
        assert_eq!(
            exec.reconciled_sources.lock().unwrap().as_slice(),
            &[src_id],
            "the retried reconcile adopts the orphan"
        );

        // Cycle 3: now that it succeeded, the once-before-first-cycle guard
        // holds - no further reconcile.
        orch.run_cycle(TickSource::Scheduled).await.unwrap();
        assert_eq!(
            exec.reconciles.load(Ordering::SeqCst),
            2,
            "a succeeded reconcile is not re-run"
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
    async fn deep_verify_timestamp_persisted_so_next_cycle_not_due() {
        // P2-7: a source that has never deep-verified is due, so the cycle
        // runs a deep-verify. After the cycle the completion timestamp must be
        // PERSISTED (last_deep_verify_at + last_full_scan_at on the source,
        // last_synced_at on the account) so `deep_verify_due` no longer
        // reports the source as due - i.e. it does NOT re-verify every cycle.
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        // A never-deep-verified source: `None` => due now (FakeClock at 0).
        let mut src = source_in(account, dir.path());
        src.last_deep_verify_at = None;
        src.last_full_scan_at = None;
        let src_id = src.id;
        let exec = Arc::new(RecordingExecutor::default());
        let state = Arc::new(FakeState::with_sources(vec![src.clone()]));
        let clock = Arc::new(FakeClock::new());
        // Advance the clock so the persisted timestamps are a recognizable
        // non-zero value distinct from the source's `None` start.
        clock.advance(std::time::Duration::from_millis(5_000));
        let now = clock.now_ms();
        let orch = SyncOrchestrator::new(
            account,
            state.clone(),
            exec.clone(),
            Arc::new(FakePowerSource::new(power_on_ac())),
            Arc::new(FakeNet::online()),
            clock.clone(),
            OrchestratorConfig::default(),
        );

        // Pre-condition: the source IS due for a deep-verify.
        assert!(
            orch.deep_verify_due(&src),
            "a never-deep-verified source is due"
        );

        orch.run_cycle(TickSource::Scheduled).await.unwrap();
        assert!(matches!(orch.state().await, OrchestratorState::Idle { .. }));

        // The persisted source row now carries both completion timestamps.
        let persisted = state
            .sources_snapshot()
            .into_iter()
            .find(|s| s.id == src_id)
            .expect("source still present");
        assert_eq!(
            persisted.last_deep_verify_at,
            Some(now),
            "deep-verify completion is persisted"
        );
        assert_eq!(
            persisted.last_full_scan_at,
            Some(now),
            "full-scan completion is persisted"
        );

        // The account's last_synced_at is stamped exactly once.
        assert_eq!(
            state.account_synced.lock().unwrap().as_slice(),
            &[(account, now)],
            "account last_synced_at is stamped on a successful source run"
        );

        // The whole point: the NEXT cycle is NOT immediately due again.
        assert!(
            !orch.deep_verify_due(&persisted),
            "after a deep-verify cycle the source is no longer due"
        );
    }

    #[tokio::test]
    async fn failed_op_defers_timestamp_advance_and_records_activity() {
        // recheck2 P1/P2: a failed op (e.g. a deep-verify hash mismatch whose
        // re-upload failed) must NOT advance the scan/verify timestamps - else
        // the fast-scan path skips the file (size+mtime unchanged) while
        // `deep_verify_due` won't re-check it for a whole interval, so the
        // changed/corrupt bytes never retry - and it must leave a durable
        // activity_log Error row so the failure is user-visible.
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        // A file in the source so the scan yields an op the executor can fail
        // (an empty dir would plan zero ops -> zero failures -> timestamps
        // would advance, which is correct but not what this test exercises).
        std::fs::write(dir.path().join("changed.bin"), b"new bytes").unwrap();
        let mut src = source_in(account, dir.path());
        src.last_deep_verify_at = None;
        src.last_full_scan_at = None;
        let src_id = src.id;
        let exec = Arc::new(RecordingExecutor::default());
        exec.fail_ops.store(1, Ordering::SeqCst);
        let state = Arc::new(FakeState::with_sources(vec![src.clone()]));
        let clock = Arc::new(FakeClock::new());
        clock.advance(std::time::Duration::from_millis(5_000));
        let orch = SyncOrchestrator::new(
            account,
            state.clone(),
            exec.clone(),
            Arc::new(FakePowerSource::new(power_on_ac())),
            Arc::new(FakeNet::online()),
            clock.clone(),
            OrchestratorConfig::default(),
        );

        orch.run_cycle(TickSource::Scheduled).await.unwrap();

        // The timestamps were NOT advanced -> the source stays due and retries.
        let persisted = state
            .sources_snapshot()
            .into_iter()
            .find(|s| s.id == src_id)
            .expect("source still present");
        assert_eq!(
            persisted.last_deep_verify_at, None,
            "a failed op must NOT advance last_deep_verify_at (the source stays due)"
        );
        assert_eq!(
            persisted.last_full_scan_at, None,
            "a failed op must NOT advance last_full_scan_at"
        );
        assert!(
            state.account_synced.lock().unwrap().is_empty(),
            "a failed op must NOT stamp account last_synced_at"
        );
        assert!(
            orch.deep_verify_due(&persisted),
            "the source is still deep-verify-due after a failed cycle"
        );

        // A durable activity Error row surfaces the failed op.
        let expected_code = crate::types::ErrorCode::DriveChecksumMismatch.to_string();
        let rows = state.activity_rows();
        assert!(
            rows.iter()
                .any(|r| matches!(r.level, crate::state::ActivityLevel::Error)
                    && r.event_type == expected_code),
            "a durable activity Error row records the failed op: {rows:?}"
        );
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
        // P1-6: the gate is evaluated BEFORE remote reconciliation, so an
        // offline cycle issues ZERO remote calls - including the startup
        // reconcile, which would otherwise hit Drive find/metadata. This is the
        // direct proof reconcile moved behind the gate (a `live_object_count`
        // check alone is vacuous - reconcile only reads).
        assert_eq!(
            exec.reconciles.load(Ordering::SeqCst),
            0,
            "offline must not issue the remote reconcile (zero remote calls)"
        );
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

    // --- P1-8: collision -> durable activity_log ERROR row -----------------

    #[tokio::test]
    async fn collision_writes_durable_activity_error_row() {
        // A dropped NFC collider must produce a DURABLE activity_log ERROR row
        // with code `local.unicode_collision` and the colliding path, not just
        // a trace line. (The two-files-on-disk walk that originates a collision
        // is filesystem-normalization-dependent and deduped at the scanner; the
        // P1-8 deliverable being asserted here is the orchestrator's durable
        // surfacing of an already-detected collision.)
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        let src = source_in(account, dir.path());
        let src_id = src.id;
        let state = Arc::new(FakeState::with_sources(vec![src]));
        let exec = Arc::new(RecordingExecutor::default());
        let clock = Arc::new(FakeClock::new());
        let orch = SyncOrchestrator::new(
            account,
            state.clone(),
            exec,
            Arc::new(FakePowerSource::new(power_on_ac())),
            Arc::new(FakeNet::online()),
            clock,
            OrchestratorConfig::default(),
        );

        let collider = RelativePath::try_from("dir/caf\u{e9}.txt".to_string()).unwrap();
        orch.record_collisions(src_id, std::slice::from_ref(&collider))
            .await;

        let rows = state.activity_rows();
        assert_eq!(rows.len(), 1, "exactly one collision row written");
        let row = &rows[0];
        assert_eq!(row.level, ActivityLevel::Error, "collision is an ERROR row");
        assert_eq!(
            row.event_type, "local.unicode_collision",
            "row carries the stable collision code"
        );
        assert_eq!(
            row.message.as_deref(),
            Some("dir/caf\u{e9}.txt"),
            "row carries the colliding path"
        );
        assert_eq!(row.source_id, Some(src_id), "row is scoped to the source");
    }

    // --- P1-7: the real run() event loop ------------------------------------

    /// An executor that blocks inside `execute` on a caller-controlled barrier
    /// so a test can hold a cycle "in flight" while it fires further triggers -
    /// the only way to deterministically exercise the mid-cycle coalescing +
    /// single-inflight guard against an otherwise-instantaneous fake.
    struct BlockingExecutor {
        executes: Arc<AtomicU64>,
        /// Signalled (one message per cycle) as soon as `execute` is entered.
        entered_tx: tokio::sync::mpsc::UnboundedSender<()>,
        /// Awaited inside `execute`; the test sends one `()` to release each
        /// in-flight cycle.
        release_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<()>>,
    }

    #[async_trait]
    impl Executor for BlockingExecutor {
        async fn execute(
            &self,
            _source: &SourceRow,
            _plan: &Plan,
            _on_progress: &(dyn Fn(ExecProgress) + Send + Sync),
        ) -> anyhow::Result<Vec<OpOutcome>> {
            self.executes.fetch_add(1, Ordering::SeqCst);
            let _ = self.entered_tx.send(());
            // Block until the test releases this cycle.
            let _ = self.release_rx.lock().await.recv().await;
            Ok(vec![])
        }

        async fn reconcile(&self, _source: &SourceRow) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Build an orchestrator with a non-empty source (so the executor runs) and
    /// the given executor + config, returned as an `Arc` ready to spawn `run`.
    fn build_arc(
        exec: Arc<dyn Executor>,
        power: Arc<FakePowerSource>,
        config: OrchestratorConfig,
    ) -> (Arc<SyncOrchestrator>, tempfile::TempDir) {
        let account = AccountId::new_v4();
        let dir = tempfile::tempdir().unwrap();
        // A file so the plan is non-empty and `execute` is actually called.
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let src = source_in(account, dir.path());
        let state = Arc::new(FakeState::with_sources(vec![src]));
        let clock = Arc::new(FakeClock::new());
        let orch = Arc::new(SyncOrchestrator::new(
            account,
            state,
            exec,
            power,
            Arc::new(FakeNet::online()),
            clock,
            config,
        ));
        (orch, dir)
    }

    #[tokio::test(start_paused = true)]
    async fn run_loop_scheduled_tick_runs_a_cycle() {
        // With virtual time paused, `tokio::time::interval` auto-advances when
        // the loop is otherwise idle, so the scheduled tick fires
        // deterministically with no wall-clock wait and runs a cycle.
        let executes = Arc::new(AtomicU64::new(0));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = tokio::sync::mpsc::unbounded_channel();
        let exec = Arc::new(BlockingExecutor {
            executes: executes.clone(),
            entered_tx,
            release_rx: tokio::sync::Mutex::new(release_rx),
        });
        let cfg = OrchestratorConfig {
            scan_interval_secs: 1,
            ..OrchestratorConfig::default()
        };
        let (orch, _dir) = build_arc(exec, Arc::new(FakePowerSource::new(power_on_ac())), cfg);

        let handle = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run().await })
        };

        // Wait for the scheduled tick to drive the first cycle into `execute`.
        tokio::time::timeout(std::time::Duration::from_secs(30), entered_rx.recv())
            .await
            .expect("scheduled tick must run a cycle")
            .expect("entered channel open");
        let _ = release_tx.send(());

        orch.shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(30), handle)
            .await
            .expect("run loop must exit after shutdown")
            .expect("join")
            .expect("run ok");
        assert!(
            executes.load(Ordering::SeqCst) >= 1,
            "scheduled tick ran a cycle"
        );
    }

    #[tokio::test]
    async fn run_loop_watcher_tick_triggers_a_cycle() {
        // A debounced watcher ScanTickRequest pushed onto the orchestrator's
        // watcher channel drives exactly one cycle.
        let executes = Arc::new(AtomicU64::new(0));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = tokio::sync::mpsc::unbounded_channel();
        let exec = Arc::new(BlockingExecutor {
            executes: executes.clone(),
            entered_tx,
            release_rx: tokio::sync::Mutex::new(release_rx),
        });
        // A long scan interval so the scheduled tick never fires during the test
        // - the watcher tick is the only thing that can run a cycle.
        let cfg = OrchestratorConfig {
            scan_interval_secs: 3_600,
            ..OrchestratorConfig::default()
        };
        let (orch, _dir) = build_arc(exec, Arc::new(FakePowerSource::new(power_on_ac())), cfg);
        let watcher = orch.watcher_sender();
        let src_id = orch
            .state
            .list_enabled_sources_for(orch.account_id)
            .await
            .unwrap()[0]
            .id;

        let handle = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run().await })
        };

        watcher
            .send(ScanTickRequest {
                source_id: src_id,
                reason: crate::watcher::ScanTickReason::Edit,
            })
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(30), entered_rx.recv())
            .await
            .expect("watcher tick must run a cycle")
            .expect("entered channel open");
        let _ = release_tx.send(());

        orch.shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(30), handle)
            .await
            .expect("run loop must exit after shutdown")
            .expect("join")
            .expect("run ok");
        assert_eq!(
            executes.load(Ordering::SeqCst),
            1,
            "watcher tick ran one cycle"
        );
    }

    #[tokio::test]
    async fn run_loop_manual_trigger_mid_cycle_coalesces_to_one_followup() {
        // While a cycle is blocked in-flight, a BURST of manual triggers must
        // coalesce into exactly ONE follow-up cycle (capacity-1 trigger
        // channel) - never a third concurrent/extra cycle. This proves both the
        // single-inflight guard (no overlap) and the coalescing.
        let executes = Arc::new(AtomicU64::new(0));
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (release_tx, release_rx) = tokio::sync::mpsc::unbounded_channel();
        let exec = Arc::new(BlockingExecutor {
            executes: executes.clone(),
            entered_tx,
            release_rx: tokio::sync::Mutex::new(release_rx),
        });
        let cfg = OrchestratorConfig {
            scan_interval_secs: 3_600,
            ..OrchestratorConfig::default()
        };
        let (orch, _dir) = build_arc(exec, Arc::new(FakePowerSource::new(power_on_ac())), cfg);

        let handle = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run().await })
        };

        // Fire the first trigger; wait until its cycle is in flight (blocked).
        orch.trigger(TickSource::Manual).await;
        tokio::time::timeout(std::time::Duration::from_secs(30), entered_rx.recv())
            .await
            .expect("first trigger must start a cycle")
            .expect("entered open");

        // Mid-cycle burst: three more triggers. The capacity-1 channel holds at
        // most one, so these coalesce into a single follow-up.
        orch.trigger(TickSource::Manual).await;
        orch.trigger(TickSource::Manual).await;
        orch.trigger(TickSource::Manual).await;

        // Release the first cycle; the single coalesced follow-up then runs.
        let _ = release_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(30), entered_rx.recv())
            .await
            .expect("the coalesced trigger must run exactly one follow-up cycle")
            .expect("entered open");
        let _ = release_tx.send(());

        orch.shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(30), handle)
            .await
            .expect("run loop must exit after shutdown")
            .expect("join")
            .expect("run ok");

        // Exactly two cycles ran: the initial trigger + ONE coalesced follow-up,
        // never three. (A small settle to ensure no stray third cycle is racing
        // - executes is monotonic, so re-reading after the join is sufficient.)
        assert_eq!(
            executes.load(Ordering::SeqCst),
            2,
            "a mid-cycle burst coalesces to exactly one follow-up"
        );
    }

    #[tokio::test]
    async fn run_loop_battery_pauses_then_ac_resumes() {
        // A power transition to battery PAUSES (gate closed); a transition back
        // to AC RESUMES and runs a cycle. Driven entirely through the run loop's
        // power-transition branch and asserted via the event stream (not a
        // vacuous counter), so it genuinely tests the named loop behavior.
        //
        // A non-blocking RecordingExecutor (not the barrier executor) is used:
        // the resend-until-observed pattern below can queue several power wakes,
        // and a blocking executor would stall the loop on the 2nd buffered wake
        // before shutdown is honored. RecordingExecutor finishes each cycle
        // instantly, so buffered wakes drain and shutdown is processed between
        // cycles.
        let exec = Arc::new(RecordingExecutor::default());
        // Long interval so only the power transitions drive cycles.
        let cfg = OrchestratorConfig {
            scan_interval_secs: 3_600,
            ..OrchestratorConfig::default()
        };
        let power = Arc::new(FakePowerSource::new(power_on_battery()));
        let (orch, _dir) = build_arc(exec.clone(), power.clone(), cfg);

        // Subscribe BEFORE driving any transition so we observe every
        // StateChanged the loop emits (the broadcast only delivers post-subscribe).
        let mut events = orch.subscribe();

        let handle = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run().await })
        };

        // Battery: the loop's power arm re-evaluates the gates and transitions
        // to Paused{Battery}. Drain the event stream until we SEE that pause -
        // this proves the loop delivered the transition AND paused.
        //
        // The loop subscribes to the power broadcast INSIDE the spawned `run()`,
        // so a single `set()` fired before that subscription lands would be
        // missed (broadcast only delivers post-subscribe). To stay race-free
        // without reaching into the loop's internals, re-send the battery
        // snapshot on a short cadence until the pause is observed; each resend is
        // an idempotent transition (same gate decision). Bounded by a timeout so
        // a genuine "never pauses" regression fails instead of hanging.
        let paused = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                power.set(power_on_battery());
                match tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
                    .await
                {
                    Ok(Ok(OrchestratorEvent::StateChanged {
                        state:
                            OrchestratorState::Paused {
                                reason: PauseReason::Battery,
                            },
                    })) => break true,
                    Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) | Err(_) => {
                        continue
                    }
                    Ok(Err(broadcast::error::RecvError::Closed)) => break false,
                }
            }
        })
        .await
        .expect("the loop must process the battery transition and pause");
        assert!(
            paused,
            "battery transition pauses the loop (Paused{{Battery}})"
        );
        assert_eq!(
            exec.executes.load(Ordering::SeqCst),
            0,
            "on battery the gate is closed; no cycle executes"
        );

        // AC: the loop re-evaluates, PROCEEDS (resumes), and runs a cycle ending
        // in Idle. Observe the Idle transition to prove the resume ran.
        let resumed = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                power.set(power_on_ac());
                match tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
                    .await
                {
                    Ok(Ok(OrchestratorEvent::StateChanged {
                        state: OrchestratorState::Idle { .. },
                    })) => break true,
                    Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) | Err(_) => {
                        continue
                    }
                    Ok(Err(broadcast::error::RecvError::Closed)) => break false,
                }
            }
        })
        .await
        .expect("AC resume must run a cycle to Idle");
        assert!(
            resumed,
            "AC transition resumes the loop and a cycle runs to Idle"
        );

        orch.shutdown();
        tokio::time::timeout(std::time::Duration::from_secs(30), handle)
            .await
            .expect("run loop must exit after shutdown")
            .expect("join")
            .expect("run ok");
        assert!(
            exec.executes.load(Ordering::SeqCst) >= 1,
            "AC resume ran at least one cycle after the battery pause"
        );
    }

    #[tokio::test]
    async fn run_loop_shutdown_exits_cleanly() {
        // A shutdown signal makes `run()` return `Ok(())` promptly.
        let exec = Arc::new(RecordingExecutor::default());
        let cfg = OrchestratorConfig {
            scan_interval_secs: 3_600,
            ..OrchestratorConfig::default()
        };
        let (orch, _dir) = build_arc(exec, Arc::new(FakePowerSource::new(power_on_ac())), cfg);

        let handle = {
            let orch = orch.clone();
            tokio::spawn(async move { orch.run().await })
        };

        orch.shutdown();
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), handle)
            .await
            .expect("run loop must exit promptly on shutdown")
            .expect("join");
        assert!(result.is_ok(), "clean shutdown returns Ok(())");
    }
}
