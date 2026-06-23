//! [`DrivenHandle`] - a booted headless Driven instance (STRESS_HARNESS s2.4).
//!
//! A handle boots ONE [`SyncOrchestrator`] wired to a hermetic
//! [`SqliteStateRepo`] (a SQLite file under the scenario's tempdir), a
//! [`RemoteStore`] chosen by the scenario's requirements (the in-memory
//! fake by default, the real Google store for the credential-gated rows),
//! a [`FakeClock`], and a [`FakePowerSource`] defaulted to "AC, unmetered".
//!
//! Booting the real core - not `src-tauri` - is the whole point of the
//! harness (DESIGN s4.2 thick-core / thin-shell split). Every seam the
//! handle needs is constructible without Tauri.
//!
//! ## Divergence from the STRESS_HARNESS s2.4 sketch (surfaced finding)
//!
//! The design sketch shows separate `activity_tail` / `error_tail`
//! `broadcast::Receiver`s. The real core (DESIGN s5, SPEC s5) exposes a
//! SINGLE [`OrchestratorEvent`] broadcast via
//! [`SyncOrchestrator::subscribe`]; activity and error entries are
//! variants of that one stream rather than two channels. The handle
//! therefore exposes one [`DrivenHandle::subscribe`] returning that
//! receiver. See the report finding in the M3.7 interface PR.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::broadcast;

use driven_core::executor::{DefaultExecutor, ExecutorDeps};
use driven_core::network::{NetworkProbe, NetworkState, ServiceHealth, ServiceName};
use driven_core::orchestrator::{Orchestrator, OrchestratorConfig, SyncOrchestrator, TickSource};
use driven_core::pacer::{Pacer, PacerCeilings, ResponseClass};
use driven_core::state::{AccountRow, SqliteStateRepo, StateRepo};
use driven_core::time::Clock;
use driven_core::types::{AccountId, AccountState, OrchestratorEvent, OrchestratorState};

use driven_drive::fake::InMemoryRemoteStore;
use driven_drive::remote_store::RemoteStore;

use driven_power::{PowerSource, PowerState};

use driven_test_fixtures::clock::FakeClock;
use driven_test_fixtures::power::FakePowerSource;

/// Hermetic per-handle configuration (STRESS_HARNESS s2.4).
#[derive(Debug, Clone)]
pub struct HermeticConfig {
    /// Per-run UUID prefix scoping keychain entries to
    /// `driven-chaos/<test-run-uuid>/` so concurrent harness runs and the
    /// user's real Driven install never collide.
    pub run_uuid: String,
    /// The orchestrator config the handle boots with.
    pub orchestrator: OrchestratorConfig,
}

impl Default for HermeticConfig {
    fn default() -> Self {
        Self {
            run_uuid: uuid::Uuid::new_v4().to_string(),
            orchestrator: OrchestratorConfig::default(),
        }
    }
}

/// A booted headless Driven instance the scenarios assert against
/// (STRESS_HARNESS s2.4).
///
/// Cloning the inner `Arc`s is how a scenario's mutator thread and its
/// assertion task share the same state / remote / clock.
pub struct DrivenHandle {
    /// SQLite state layer, hermetic per-scenario file.
    pub state: Arc<dyn StateRepo>,
    /// The Drive-side store. In-memory fake by default; the real Google
    /// store for credential-gated scenarios.
    pub remote: Arc<dyn RemoteStore>,
    /// Deterministic clock seam (`FakeClock` by default).
    pub clock: Arc<dyn Clock>,
    /// Power seam, defaulted to "AC, unmetered".
    pub power: Arc<dyn PowerSource>,
    /// The booted orchestrator (one per account, DESIGN s5.1).
    pub orchestrator: Arc<SyncOrchestrator>,
    /// Hermetic config the handle was booted with.
    pub config: HermeticConfig,
    /// The account this handle drives.
    pub account_id: AccountId,
}

impl DrivenHandle {
    /// Subscribe to the orchestrator's [`OrchestratorEvent`] broadcast for
    /// assertions instead of polling SQLite (STRESS_HARNESS s2.4).
    ///
    /// NOTE: the design sketch named two receivers (`activity_tail`,
    /// `error_tail`); the real core has one combined stream. See the
    /// module-level divergence note.
    pub fn subscribe(&self) -> broadcast::Receiver<OrchestratorEvent> {
        self.orchestrator.subscribe()
    }

    /// Snapshot the orchestrator's coarse lifecycle state for
    /// `wait_for_state`-style assertions (STRESS_HARNESS s2.4).
    pub async fn state(&self) -> OrchestratorState {
        self.orchestrator.state().await
    }

    /// Kick one sync cycle and return once the trigger is accepted
    /// (STRESS_HARNESS s2.4 `orchestrator.sync_now`). The caller awaits a
    /// state transition via [`Self::subscribe`] / [`Self::state`] to
    /// observe completion.
    pub async fn sync_now(&self) {
        self.orchestrator.trigger(TickSource::Manual).await;
    }

    /// Run exactly one cycle to completion synchronously - the deterministic
    /// alternative to [`Self::sync_now`] for assertion code that wants the
    /// cycle finished before it inspects state.
    pub async fn run_one_cycle(&self) -> anyhow::Result<()> {
        self.orchestrator.run_cycle(TickSource::Manual).await
    }

    /// Simulate `kill -9` cleanly for crash-recovery scenarios
    /// (STRESS_HARNESS s2.4). Drops the in-process orchestrator without
    /// running its graceful shutdown so the next-booted handle exercises
    /// the reconciliation pass (DESIGN s5.6). The hermetic `StateRepo`
    /// file persists across the kill, which is what the crash-recovery
    /// scenarios reconcile against.
    ///
    /// Phase-2 boots a fresh [`DrivenHandle`] over the SAME `state` path
    /// after calling this to assert the reconcile behaviour.
    pub async fn kill_orchestrator(self) -> Arc<dyn StateRepo> {
        // Dropping `self` drops the orchestrator's channels (no graceful
        // shutdown was signalled), modelling an abrupt process death. The
        // SQLite file survives, so we hand the state layer back for the
        // re-boot.
        self.state
    }
}

/// Builder that boots a [`DrivenHandle`] over hermetic seams.
///
/// The default build uses the in-memory fake remote, a `FakeClock`, and
/// AC power. Scenarios that need a different remote (credential-gated
/// rows) or a fault-injected fake override [`Self::remote`] before
/// [`Self::boot`].
pub struct DrivenHandleBuilder {
    state_db_path: std::path::PathBuf,
    remote: Option<Arc<dyn RemoteStore>>,
    power: PowerState,
    config: HermeticConfig,
}

impl DrivenHandleBuilder {
    /// Start a builder whose hermetic SQLite file lives at `state_db_path`.
    pub fn new(state_db_path: std::path::PathBuf) -> Self {
        Self {
            state_db_path,
            remote: None,
            power: power_on_ac(),
            config: HermeticConfig::default(),
        }
    }

    /// Override the remote store (e.g. a fault-injected fake or the real
    /// Google store). Defaults to a fresh [`InMemoryRemoteStore`].
    pub fn remote(mut self, remote: Arc<dyn RemoteStore>) -> Self {
        self.remote = Some(remote);
        self
    }

    /// Override the initial power state (defaults to AC, unmetered).
    pub fn power(mut self, power: PowerState) -> Self {
        self.power = power;
        self
    }

    /// Override the hermetic config (run-uuid prefix + orchestrator config).
    pub fn config(mut self, config: HermeticConfig) -> Self {
        self.config = config;
        self
    }

    /// Boot the orchestrator over the configured seams. Opens (or reopens,
    /// for crash-recovery) the hermetic SQLite file, seeds one account if
    /// the DB is fresh, wires the executor, and returns the live handle.
    pub async fn boot(self) -> anyhow::Result<DrivenHandle> {
        let state: Arc<SqliteStateRepo> =
            Arc::new(SqliteStateRepo::open(&self.state_db_path).await?);

        // Adopt the existing account on a reopened (crash-recovery) DB so the
        // booted orchestrator drives the SAME account the pre-crash run did;
        // only seed a fresh account when the DB is brand new. Seeding a new
        // random `account_id` unconditionally would leave the reopened
        // orchestrator pointed at an empty account (no sources) and silently
        // upload nothing - which is exactly what broke the kill-9 /
        // pause-mid-resumable crash-recovery scenarios.
        let account_id = match state.list_accounts().await?.into_iter().next() {
            Some(existing) => existing.id,
            None => {
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
                id
            }
        };

        let remote: Arc<dyn RemoteStore> = self
            .remote
            .unwrap_or_else(|| Arc::new(InMemoryRemoteStore::new()));
        let clock: Arc<FakeClock> = Arc::new(FakeClock::new());
        let power: Arc<dyn PowerSource> = Arc::new(FakePowerSource::new(self.power));
        let network: Arc<dyn NetworkProbe> = Arc::new(AlwaysOnlineProbe);
        let pacer: Arc<dyn Pacer> = Arc::new(NoopPacer);

        let executor = Arc::new(DefaultExecutor::with_clock(
            ExecutorDeps {
                remote: remote.clone(),
                state: state.clone(),
                pacer,
                crypto: None,
                vss: None,
                network: None,
            },
            clock.clone(),
        ));

        let orchestrator = Arc::new(SyncOrchestrator::new(
            account_id,
            state.clone(),
            executor,
            power.clone(),
            network,
            clock.clone(),
            self.config.orchestrator.clone(),
        ));

        Ok(DrivenHandle {
            state,
            remote,
            clock,
            power,
            orchestrator,
            config: self.config,
            account_id,
        })
    }
}

/// AC, unmetered, reachable - the harness default power state
/// (STRESS_HARNESS s2.4 "FakePower defaulted to AC, unmetered").
pub fn power_on_ac() -> PowerState {
    PowerState {
        ac_connected: true,
        battery_percent: Some(100),
        on_metered_network: false,
        network_reachable: true,
    }
}

/// A pass-through [`Pacer`] that never gates.
///
/// The harness asserts sync correctness, not rate-pacing; the real
/// `AimdPacer` blocks on `tokio::time` while polling the (non-advancing)
/// `FakeClock` and would deadlock. AIMD behaviour is unit-tested in
/// `pacer.rs`; here we substitute the same no-op pacer the e2e suite uses.
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

/// A minimal always-online [`NetworkProbe`]. The harness drives Drive-side
/// failure via the fake's fault injection (STRESS_HARNESS s5), not via the
/// network probe, so the default probe simply reports a healthy network.
/// Scenarios that need flapping connectivity supply their own probe.
struct AlwaysOnlineProbe;

#[async_trait]
impl NetworkProbe for AlwaysOnlineProbe {
    async fn probe(&self) -> NetworkState {
        NetworkState::Online
    }
    fn service_health(&self, _service: ServiceName) -> ServiceHealth {
        ServiceHealth::Closed
    }
    fn note_outcome(&self, _service: ServiceName, _ok: bool) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handle must boot the real headless core over hermetic seams and
    /// run one cycle (with no sources configured) without error - the
    /// interface contract the Phase-2 scenario agents build on.
    #[tokio::test]
    async fn boots_and_runs_an_empty_cycle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = DrivenHandleBuilder::new(dir.path().join("state.db"))
            .boot()
            .await
            .expect("boot headless core");

        // A no-source cycle is a clean no-op.
        handle.run_one_cycle().await.expect("empty cycle runs");

        // The event stream is live (a fresh subscriber, no panic).
        let _rx = handle.subscribe();
        // State is observable.
        let _state = handle.state().await;

        // kill_orchestrator hands back the persisted state layer for a
        // crash-recovery re-boot.
        let state = handle.kill_orchestrator().await;
        let reopened = DrivenHandleBuilder::new(dir.path().join("state.db"))
            .boot()
            .await
            .expect("re-boot over the same DB");
        // Both handles point at a live SQLite state layer.
        let _ = (state, reopened);
    }
}
