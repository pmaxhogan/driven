//! Network-resilience surface: probe topology, per-service circuit
//! breakers, and the observed-state classification the orchestrator gates
//! on (DESIGN s5.8).
//!
//! `driven-core` stays I/O-free (lib.rs), so this module holds the
//! contract plus the *transport-agnostic* mechanism: the
//! [`StdCircuitBreaker`] state machine (DESIGN s5.8.3) and the generic
//! [`Prober`] that runs the three-probe topology (DESIGN s5.8.2). All
//! real I/O is funnelled through the [`Backend`] seam, whose production
//! implementation lives in an implementer crate and uses `reqwest` (0.12,
//! rustls) for the captive-portal / per-service HTTP probes with the
//! per-service timeouts of DESIGN s5.8.4, `hickory-resolver` (0.24) for
//! DNS re-resolution, and the OS connectivity APIs (DESIGN s5.8.2) for the
//! cheapest first probe. Tests back the seam with an in-memory
//! [`Backend`] driven by `driven-test-fixtures`'
//! [`FakeNetwork`](../../driven_test_fixtures/network/struct.FakeNetwork.html)
//! and a `FakeClock`, exercising every DESIGN s5.8.1 failure mode without
//! a real socket.
//!
//! The orchestrator (DESIGN s5.8.6) translates a [`NetworkState`] +
//! per-service [`ServiceHealth`] into a [`PauseReason`](crate::types::PauseReason)
//! (offline / metered / service-down) or lets a partial-availability batch
//! proceed for the services that are up.
//!
//! Proxy support (`HTTP_PROXY` / `HTTPS_PROXY`, DESIGN s5.8.7) is honoured
//! by `reqwest`'s built-in env-proxy handling inside the production
//! [`Backend`]; it needs no surface here.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::time::Clock;
use crate::types::UnixMs;

/// A service-specific dependency Driven probes and circuit-breaks
/// independently (DESIGN s5.8.2 probe topology, s5.8.3 circuit breaker).
///
/// One [`ServiceHealth`] / circuit-breaker is tracked per variant. A
/// same-named scenario-selector enum exists in `driven-test-fixtures`
/// (`network::ServiceName`); this is the canonical core copy. Reconciling
/// the fixture to import this is a later M3 phase (see the M3 phase-1
/// finding); they do not collide at the type level today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceName {
    /// Google Drive (`GET /drive/v3/about?fields=user` probe + all upload
    /// / download traffic).
    Drive,
    /// Driven's own update manifest endpoint
    /// (`HEAD https://driven.maxhogan.dev/updates/_health`).
    UpdateEndpoint,
    /// `api.github.com` release-notes fetch.
    Github,
    /// The Cloudflare-hosted telemetry sink (never probed; best-effort).
    Telemetry,
}

/// The orchestrator's *observed* network classification (DESIGN s5.8.1
/// failure modes, s5.8.6 substates).
///
/// This is the steady-state result of the three-probe topology, NOT a
/// fault-injection selector: it carries no `drop_pct` / `up_secs`
/// scenario parameters (those belong to the test fixture's same-named
/// enum). The orchestrator maps these onto a
/// [`PauseReason`](crate::types::PauseReason) or a degraded-but-running
/// batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkState {
    /// All probes succeeded most recently. Sync may proceed.
    #[default]
    Online,
    /// The OS reports no active connectivity (airplane mode / rfkill / no
    /// interface). Re-probe on the OS "interface up" event (DESIGN s5.8.1).
    Offline,
    /// Connected to a network but the `generate_204` probe fails
    /// (link-local only). Re-probe every 30s.
    NoInternet,
    /// The resolver returned no answer for a known-good domain (DESIGN
    /// s5.8.1 DNS broken). Re-probe every 60s; never cache failed resolves.
    DnsFailed,
    /// A captive portal intercepted the `generate_204` probe; user action
    /// required (DESIGN s5.8.1).
    CaptivePortal,
}

/// Health of one service's circuit breaker (DESIGN s5.8.3).
///
/// The breaker opens after 5 consecutive failures, fails fast while open,
/// and half-opens for a single probe on an exponential schedule
/// (30s, 1m, 2m, 5m, 10m, plateau). This enum is the public read-out the
/// orchestrator gates on per service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceHealth {
    /// Healthy; requests flow normally.
    Closed,
    /// Failing; requests fail-fast without hitting the network until
    /// `retry_at`. `retry_at` is Unix epoch ms (DESIGN s5.8.3).
    Open {
        /// Unix epoch ms at which the next half-open probe is allowed.
        retry_at: UnixMs,
    },
    /// Probing recovery; one in-flight probe will decide Closed vs Open.
    HalfOpen,
}

/// A network transition the probe layer surfaces to the orchestrator
/// (carried by [`OrchestratorEvent::Network`](crate::types::OrchestratorEvent)).
///
/// Distinct from the steady-state [`NetworkState`] snapshot: an event marks
/// the *edge* of a change so the orchestrator can react (pause, resume a
/// drained queue after >30s stable, surface a captive-portal tray action)
/// without polling (DESIGN s5.8.1, s5.8.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum NetworkEvent {
    /// The overall observed [`NetworkState`] changed.
    StateChanged {
        /// The newly observed state.
        state: NetworkState,
    },
    /// A single service's circuit-breaker health changed (DESIGN s5.8.3).
    ServiceHealthChanged {
        /// The service whose breaker moved.
        service: ServiceName,
        /// Its new health.
        health: ServiceHealth,
    },
}

/// The network-probe contract the orchestrator depends on (DESIGN s5.8).
///
/// Implementations run the three-probe topology (DESIGN s5.8.2) and own
/// the per-service circuit breakers (DESIGN s5.8.3). All methods are
/// async because the probes are network I/O; the trait is the seam that
/// keeps `driven-core` itself I/O-free and lets tests drive every DESIGN
/// s5.8.1 failure mode through `driven-test-fixtures`'
/// [`FakeNetwork`](../../driven_test_fixtures/network/struct.FakeNetwork.html).
#[async_trait]
pub trait NetworkProbe: Send + Sync {
    /// Runs the full probe topology once and returns the current observed
    /// classification. Cheap enough to call before each batch and on the
    /// 30s/60s re-probe cadence (DESIGN s5.8.1).
    async fn probe(&self) -> NetworkState;

    /// Returns the current circuit-breaker health for one service without
    /// issuing a probe (a cached read of the breaker state machine).
    fn service_health(&self, service: ServiceName) -> ServiceHealth;

    /// Records the outcome of a real request against `service` so the
    /// breaker can advance its state machine (5-consecutive-failure open,
    /// half-open probe success -> close). `ok = false` counts a failure.
    fn note_outcome(&self, service: ServiceName, ok: bool);
}

/// A monotonic-clock-aware circuit breaker for one service (DESIGN
/// s5.8.3).
///
/// Declared as a trait so the implementer can back it with the injected
/// [`Clock`] (the exponential 30s..10m backoff schedule reads
/// [`Clock::now_ms`]) and tests can drive it deterministically off a
/// `FakeClock`.
pub trait CircuitBreaker: Send + Sync {
    /// Current breaker health.
    fn health(&self, clock: &dyn Clock) -> ServiceHealth;

    /// Records a successful request: closes the breaker / resets the
    /// failure count.
    fn record_success(&self);

    /// Records a failed request: increments the failure count and opens
    /// the breaker once the 5-consecutive-failure threshold is crossed,
    /// scheduling the next half-open probe per the exponential schedule.
    fn record_failure(&self, clock: &dyn Clock);
}

// ---------------------------------------------------------------------------
// Mechanism: the transport-agnostic probe topology + circuit breaker.
//
// Everything below is I/O-free. It implements the committed `NetworkProbe`
// and `CircuitBreaker` traits above and depends only on the injected
// `Clock` plus the `Backend` seam, which the production implementer crate
// fills with reqwest / hickory / OS-connectivity calls.
// ---------------------------------------------------------------------------

/// Number of consecutive failures that trips a service's breaker
/// Closed -> Open (DESIGN s5.8.3).
pub const BREAKER_OPEN_THRESHOLD: u32 = 5;

/// Number of consecutive network-level failures on one service after
/// which its connection pool is discarded (DESIGN s5.8.5). Tracked
/// independently of [`BREAKER_OPEN_THRESHOLD`]: a pool teardown can fire
/// (at 3) before the breaker opens (at 5).
pub const POOL_TEARDOWN_THRESHOLD: u32 = 3;

/// Exponential half-open backoff schedule in milliseconds: 30s, 1m, 2m,
/// 5m, 10m, then plateau at 10m (DESIGN s5.8.3). The breaker indexes into
/// this on each successive open, saturating at the last entry.
pub const BACKOFF_SCHEDULE_MS: [i64; 5] = [
    30_000,  // 30s
    60_000,  // 1m
    120_000, // 2m
    300_000, // 5m
    600_000, // 10m
];

/// The outcome of a single probe against the [`Backend`].
///
/// The captive-portal and service probes return this; the [`Prober`]
/// folds the per-probe outcomes into a [`NetworkState`] and feeds each
/// service breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The probe completed successfully (e.g. `generate_204` returned a
    /// bare 204, or `/drive/v3/about` returned 2xx).
    Ok,
    /// The endpoint was reached but answered wrong: a captive portal
    /// intercepted `generate_204` (non-204 / redirect / body) (DESIGN
    /// s5.8.1).
    CaptivePortal,
    /// DNS resolution failed for a known-good domain within the 3s budget
    /// (DESIGN s5.8.1 DNS broken). Distinct from a plain connect failure
    /// because the orchestrator surfaces a different message and re-probe
    /// cadence (60s vs 30s).
    DnsFailed,
    /// A network-level failure (connect timeout, RST, total timeout) that
    /// is NOT an HTTP 4xx/5xx. Counts toward the pool-teardown threshold
    /// (DESIGN s5.8.5).
    NetworkError,
    /// The endpoint answered with a server error (5xx) or otherwise
    /// indicated the *service* is down while the network is fine. Counts
    /// toward the breaker but NOT the pool-teardown threshold.
    ServiceError,
}

impl ProbeOutcome {
    /// Whether this outcome should advance the service breaker's failure
    /// count (DESIGN s5.8.3). Only [`ProbeOutcome::Ok`] resets it.
    fn is_failure(self) -> bool {
        !matches!(self, ProbeOutcome::Ok)
    }

    /// Whether this outcome counts toward the per-connection pool-teardown
    /// threshold: network-level errors only, not HTTP service errors
    /// (DESIGN s5.8.5).
    fn is_network_level(self) -> bool {
        matches!(self, ProbeOutcome::NetworkError | ProbeOutcome::DnsFailed)
    }
}

/// The transport seam the [`Prober`] runs the three-probe topology over
/// (DESIGN s5.8.2).
///
/// This is the I/O boundary that keeps `driven-core` socket-free. The
/// production implementation (implementer crate, M3 wiring) backs each
/// method with the concrete client:
///
/// - [`Backend::os_online`] - the OS connectivity API
///   (`INetworkListManager` / `NWPathMonitor` / NetworkManager), the
///   cheapest first probe.
/// - [`Backend::probe_captive`] - an HTTP GET to
///   `http://www.gstatic.com/generate_204` via a `reqwest::Client` with
///   the 3s-connect / 5s-total captive-portal timeouts (DESIGN s5.8.4),
///   classifying a non-204/redirect/body as
///   [`ProbeOutcome::CaptivePortal`].
/// - [`Backend::probe_service`] - the per-service health request (DESIGN
///   s5.8.2 list) on a per-service `reqwest::Client` carrying that
///   service's timeouts (DESIGN s5.8.4), with `hickory-resolver`
///   re-resolving DNS each call.
///
/// Tests implement this with an in-memory backend reading a
/// `FakeNetwork`, counting calls so the "offline => no further probes"
/// topology invariant (DESIGN s5.8.2) is directly assertable.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Returns the OS's connectivity verdict (DESIGN s5.8.2 probe 1).
    /// `false` short-circuits the topology to [`NetworkState::Offline`]
    /// with no further probes.
    async fn os_online(&self) -> bool;

    /// Runs the captive-portal `generate_204` probe (DESIGN s5.8.2 probe
    /// 2).
    async fn probe_captive(&self) -> ProbeOutcome;

    /// Runs `service`'s health probe (DESIGN s5.8.2 probe 3). Skipped by
    /// the [`Prober`] for services that are best-effort-only
    /// ([`ServiceName::Telemetry`]).
    async fn probe_service(&self, service: ServiceName) -> ProbeOutcome;

    /// Discards the connection pool for `service` after
    /// [`POOL_TEARDOWN_THRESHOLD`] consecutive network-level failures
    /// (DESIGN s5.8.5). The production backend drops the `reqwest::Client`
    /// (or its pooled connections); the default no-ops so backends that do
    /// not pool need not implement it.
    async fn drop_pool(&self, service: ServiceName) {
        let _ = service;
    }
}

/// Interior state of one [`StdCircuitBreaker`].
///
/// `Default` is the Closed cold-start state (all counters 0, `open_until`
/// `None`); derived because every field's default matches that meaning.
#[derive(Debug, Clone, Copy, Default)]
struct BreakerState {
    /// Consecutive failures since the last success. Resets to 0 on
    /// success.
    consecutive_failures: u32,
    /// Consecutive network-level failures on this service's connection,
    /// tracked separately from `consecutive_failures` for the pool-
    /// teardown threshold (DESIGN s5.8.5).
    consecutive_network_failures: u32,
    /// `true` once the pool-teardown threshold was crossed and the caller
    /// has not yet acted on it; read-and-cleared via
    /// [`StdCircuitBreaker::take_pool_teardown`].
    pool_teardown_pending: bool,
    /// Index into [`BACKOFF_SCHEDULE_MS`] for the *next* open. Advances on
    /// each open, saturating at the last entry.
    backoff_idx: usize,
    /// `Some(retry_at_ms)` while the breaker is Open; `None` while Closed.
    /// When `now >= retry_at`, [`StdCircuitBreaker::health`] reports
    /// [`ServiceHealth::HalfOpen`] without mutating state - the next
    /// recorded outcome decides Closed vs a re-Open with the next backoff.
    open_until: Option<UnixMs>,
}

/// A [`Clock`]-driven [`CircuitBreaker`] implementation (DESIGN s5.8.3).
///
/// Holds its state behind a [`std::sync::Mutex`] so it is `Send + Sync`
/// and cheap to share (the [`Prober`] keeps one per service). The
/// exponential half-open schedule reads [`Clock::now_ms`]; tests drive it
/// deterministically with a `FakeClock`.
#[derive(Debug, Default)]
pub struct StdCircuitBreaker {
    state: Mutex<BreakerState>,
}

impl StdCircuitBreaker {
    /// Constructs a fresh breaker in the Closed state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks the interior state, recovering from a poisoned lock instead
    /// of panicking (house rule: no `unwrap`/`expect`/`panic!` in non-test
    /// code). A poisoned breaker has at worst a partially-updated failure
    /// count, which self-corrects on the next success.
    fn lock(&self) -> std::sync::MutexGuard<'_, BreakerState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Returns and clears the pending pool-teardown signal (DESIGN
    /// s5.8.5). The [`Prober`] calls this after recording a failure; a
    /// `true` return means it should ask the [`Backend`] to drop the
    /// service's connection pool.
    pub fn take_pool_teardown(&self) -> bool {
        let mut s = self.lock();
        std::mem::take(&mut s.pool_teardown_pending)
    }
}

impl CircuitBreaker for StdCircuitBreaker {
    fn health(&self, clock: &dyn Clock) -> ServiceHealth {
        let s = self.lock();
        match s.open_until {
            None => ServiceHealth::Closed,
            Some(retry_at) => {
                if clock.now_ms() >= retry_at {
                    // Backoff elapsed: advertise HalfOpen so the caller
                    // sends one probe. State stays Open until that probe's
                    // outcome is recorded.
                    ServiceHealth::HalfOpen
                } else {
                    ServiceHealth::Open { retry_at }
                }
            }
        }
    }

    fn record_success(&self) {
        let mut s = self.lock();
        *s = BreakerState::default();
    }

    fn record_failure(&self, clock: &dyn Clock) {
        let mut s = self.lock();
        s.consecutive_failures = s.consecutive_failures.saturating_add(1);
        if s.consecutive_failures >= BREAKER_OPEN_THRESHOLD || s.open_until.is_some() {
            // Open (or re-open from a failed half-open probe): schedule the
            // next half-open attempt per the exponential backoff, then
            // advance the index for any subsequent open.
            let delay = BACKOFF_SCHEDULE_MS[s.backoff_idx.min(BACKOFF_SCHEDULE_MS.len() - 1)];
            s.open_until = Some(clock.now_ms().saturating_add(delay));
            if s.backoff_idx + 1 < BACKOFF_SCHEDULE_MS.len() {
                s.backoff_idx += 1;
            }
        }
    }
}

impl StdCircuitBreaker {
    /// Records a typed [`ProbeOutcome`], advancing both the breaker and
    /// the independent pool-teardown counter (DESIGN s5.8.3 + s5.8.5).
    ///
    /// Split from the trait's `bool`-based [`CircuitBreaker::record_failure`]
    /// because the pool-teardown distinction needs the network-level vs
    /// service-level outcome, which a bare `ok: bool` cannot carry.
    fn record_outcome(&self, outcome: ProbeOutcome, clock: &dyn Clock) {
        if !outcome.is_failure() {
            self.record_success();
            return;
        }
        // Network-level failures advance the pool-teardown counter.
        if outcome.is_network_level() {
            let mut s = self.lock();
            s.consecutive_network_failures = s.consecutive_network_failures.saturating_add(1);
            if s.consecutive_network_failures >= POOL_TEARDOWN_THRESHOLD {
                s.pool_teardown_pending = true;
                s.consecutive_network_failures = 0;
            }
        }
        self.record_failure(clock);
    }
}

/// Generic three-probe prober (DESIGN s5.8.2) implementing the committed
/// [`NetworkProbe`].
///
/// Owns one [`StdCircuitBreaker`] per probed service and runs the topology
/// over an injected [`Backend`] + [`Clock`]. It is transport-agnostic: the
/// production wiring constructs it with a reqwest/hickory/OS backend, and
/// tests construct it with an in-memory backend over a `FakeNetwork`.
///
/// Topology (DESIGN s5.8.2):
/// 1. Ask the OS. If offline, return [`NetworkState::Offline`] and fire
///    **no** further probes.
/// 2. Otherwise run the captive-portal probe and every probed service's
///    probe **in parallel**.
/// 3. Classify: a captive-portal or DNS result on the captive probe takes
///    precedence; otherwise the network is [`NetworkState::Online`] (the
///    orchestrator inspects per-service health separately to decide
///    degraded operation).
pub struct Prober<B: Backend> {
    backend: Arc<B>,
    clock: Arc<dyn Clock>,
    drive: StdCircuitBreaker,
    update_endpoint: StdCircuitBreaker,
    github: StdCircuitBreaker,
    telemetry: StdCircuitBreaker,
}

impl<B: Backend> Prober<B> {
    /// Services the topology actively probes (DESIGN s5.8.2 probe 3).
    /// [`ServiceName::Telemetry`] is best-effort and never probed, so it
    /// is excluded.
    const PROBED_SERVICES: [ServiceName; 3] = [
        ServiceName::Drive,
        ServiceName::UpdateEndpoint,
        ServiceName::Github,
    ];

    /// Constructs a prober over `backend` using `clock` for breaker
    /// timing. All breakers start Closed.
    pub fn new(backend: Arc<B>, clock: Arc<dyn Clock>) -> Self {
        Self {
            backend,
            clock,
            drive: StdCircuitBreaker::new(),
            update_endpoint: StdCircuitBreaker::new(),
            github: StdCircuitBreaker::new(),
            telemetry: StdCircuitBreaker::new(),
        }
    }

    /// Returns the breaker tracking `service`.
    fn breaker(&self, service: ServiceName) -> &StdCircuitBreaker {
        match service {
            ServiceName::Drive => &self.drive,
            ServiceName::UpdateEndpoint => &self.update_endpoint,
            ServiceName::Github => &self.github,
            ServiceName::Telemetry => &self.telemetry,
        }
    }

    /// Records a typed outcome for `service`, advancing its breaker and
    /// asking the [`Backend`] to drop the pool when the teardown threshold
    /// trips (DESIGN s5.8.5).
    async fn record_service_outcome(&self, service: ServiceName, outcome: ProbeOutcome) {
        let breaker = self.breaker(service);
        breaker.record_outcome(outcome, self.clock.as_ref());
        if breaker.take_pool_teardown() {
            self.backend.drop_pool(service).await;
        }
    }
}

#[async_trait]
impl<B: Backend> NetworkProbe for Prober<B> {
    async fn probe(&self) -> NetworkState {
        // Probe 1: OS connectivity. Cheapest; short-circuits the topology
        // (DESIGN s5.8.2) so an offline machine fires no HTTP probes.
        if !self.backend.os_online().await {
            return NetworkState::Offline;
        }

        // Probes 2 + 3 run in parallel (DESIGN s5.8.2). The captive probe
        // classifies the link; the per-service probes feed each breaker.
        let captive_fut = self.backend.probe_captive();
        let service_futs = futures::future::join_all(
            Self::PROBED_SERVICES
                .iter()
                .map(|&svc| async move { (svc, self.backend.probe_service(svc).await) }),
        );
        let (captive, service_results) = futures::future::join(captive_fut, service_futs).await;

        // Feed every service breaker with its probe outcome.
        for (svc, outcome) in service_results {
            self.record_service_outcome(svc, outcome).await;
        }

        // Classify the link from the captive-portal probe. Captive portal
        // and DNS failure are surfaced as distinct states (DESIGN s5.8.1);
        // a network-level failure of the captive probe while the OS claims
        // connectivity means link-local-only "no Internet".
        match captive {
            ProbeOutcome::Ok => NetworkState::Online,
            ProbeOutcome::CaptivePortal => NetworkState::CaptivePortal,
            ProbeOutcome::DnsFailed => NetworkState::DnsFailed,
            ProbeOutcome::NetworkError | ProbeOutcome::ServiceError => NetworkState::NoInternet,
        }
    }

    fn service_health(&self, service: ServiceName) -> ServiceHealth {
        self.breaker(service).health(self.clock.as_ref())
    }

    fn note_outcome(&self, service: ServiceName, ok: bool) {
        let breaker = self.breaker(service);
        if ok {
            breaker.record_success();
        } else {
            breaker.record_failure(self.clock.as_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::test_support::FakeClock;
    use driven_test_fixtures::network::{
        FakeNetwork, NetworkState as FakeState, ServiceName as FakeService,
    };

    /// Maps a core [`ServiceName`] to the fixture's same-named enum so the
    /// in-memory backend can match a `ServiceDown { service }` scenario.
    fn to_fake_service(s: ServiceName) -> FakeService {
        match s {
            ServiceName::Drive => FakeService::Drive,
            ServiceName::UpdateEndpoint => FakeService::UpdateEndpoint,
            ServiceName::Github => FakeService::Github,
            ServiceName::Telemetry => FakeService::Telemetry,
        }
    }

    /// In-memory [`Backend`] that reads a [`FakeNetwork`] to shape each
    /// probe's response and counts calls so topology invariants ("offline
    /// => no captive / service probes", DESIGN s5.8.2) are assertable.
    #[derive(Default)]
    struct FakeBackend {
        net: FakeNetwork,
        os_calls: AtomicUsize,
        captive_calls: AtomicUsize,
        service_calls: AtomicUsize,
        pool_drops: AtomicUsize,
        /// When `Some`, [`Backend::probe_service`] returns this for ALL
        /// services regardless of the [`FakeNetwork`] state - used to
        /// drive the breaker-open and pool-teardown tests deterministically.
        force_service: Mutex<Option<ProbeOutcome>>,
    }

    impl FakeBackend {
        fn new(state: FakeState) -> Arc<Self> {
            Arc::new(Self {
                net: FakeNetwork::with_state(state),
                ..Default::default()
            })
        }

        fn force_service_outcome(&self, outcome: Option<ProbeOutcome>) {
            *self.force_service.lock().unwrap() = outcome;
        }
    }

    #[async_trait]
    impl Backend for FakeBackend {
        async fn os_online(&self) -> bool {
            self.os_calls.fetch_add(1, Ordering::SeqCst);
            !matches!(self.net.state(), FakeState::Offline)
        }

        async fn probe_captive(&self) -> ProbeOutcome {
            self.captive_calls.fetch_add(1, Ordering::SeqCst);
            match self.net.state() {
                FakeState::Online
                | FakeState::ServiceDown { .. }
                | FakeState::Lossy { .. }
                | FakeState::Intermittent { .. } => ProbeOutcome::Ok,
                FakeState::Offline => ProbeOutcome::NetworkError,
                FakeState::NoInternet => ProbeOutcome::NetworkError,
                FakeState::DnsFail => ProbeOutcome::DnsFailed,
                FakeState::CaptivePortal => ProbeOutcome::CaptivePortal,
            }
        }

        async fn probe_service(&self, service: ServiceName) -> ProbeOutcome {
            self.service_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(forced) = *self.force_service.lock().unwrap() {
                return forced;
            }
            match self.net.state() {
                FakeState::ServiceDown { service: down } if down == to_fake_service(service) => {
                    ProbeOutcome::ServiceError
                }
                FakeState::DnsFail => ProbeOutcome::DnsFailed,
                _ => ProbeOutcome::Ok,
            }
        }

        async fn drop_pool(&self, _service: ServiceName) {
            self.pool_drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn prober(backend: Arc<FakeBackend>, clock: FakeClock) -> Prober<FakeBackend> {
        Prober::new(backend, Arc::new(clock))
    }

    // --- DESIGN s5.8.1 row: airplane mode / offline (no further calls) ---

    #[tokio::test]
    async fn offline_fires_no_http_probes() {
        let backend = FakeBackend::new(FakeState::Offline);
        let p = prober(backend.clone(), FakeClock::new());

        assert_eq!(p.probe().await, NetworkState::Offline);
        // OS probe ran once; captive + service probes must NOT have fired
        // (DESIGN s5.8.2 short-circuit).
        assert_eq!(backend.os_calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.captive_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend.service_calls.load(Ordering::SeqCst), 0);
    }

    // --- DESIGN s5.8.1 row: connected but no Internet ---

    #[tokio::test]
    async fn no_internet_surfaced() {
        let backend = FakeBackend::new(FakeState::NoInternet);
        let p = prober(backend.clone(), FakeClock::new());

        assert_eq!(p.probe().await, NetworkState::NoInternet);
        // OS said online, so the captive probe DID run.
        assert_eq!(backend.captive_calls.load(Ordering::SeqCst), 1);
    }

    // --- DESIGN s5.8.1 row: DNS broken (must not hang; classified) ---

    #[tokio::test]
    async fn dns_fail_classified_without_hang() {
        let backend = FakeBackend::new(FakeState::DnsFail);
        let p = prober(backend.clone(), FakeClock::new());

        // The fake resolves instantly; the real backend bounds this to the
        // 3s DNS budget (DESIGN s5.8.1). We assert the classification and
        // that the call returned (no hang) by completing under a tight
        // tokio timeout.
        let state = tokio::time::timeout(Duration::from_secs(1), p.probe())
            .await
            .expect("probe must not hang on DNS failure");
        assert_eq!(state, NetworkState::DnsFailed);
    }

    // --- DESIGN s5.8.1 row: captive portal ---

    #[tokio::test]
    async fn captive_portal_surfaced() {
        let backend = FakeBackend::new(FakeState::CaptivePortal);
        let p = prober(backend.clone(), FakeClock::new());

        assert_eq!(p.probe().await, NetworkState::CaptivePortal);
    }

    // --- DESIGN s5.8.1 row: lossy completes (does not error out) ---

    #[tokio::test]
    async fn lossy_completes_online() {
        let backend = FakeBackend::new(FakeState::Lossy {
            drop_pct: 99,
            added_latency_ms: 10_000,
        });
        let p = prober(backend.clone(), FakeClock::new());

        // A lossy-but-present link still classifies Online; the breakers
        // and per-request backoff (orchestrator-side) absorb the loss.
        let state = tokio::time::timeout(Duration::from_secs(1), p.probe())
            .await
            .expect("lossy probe must complete");
        assert_eq!(state, NetworkState::Online);
    }

    // --- DESIGN s5.8.1 row: drive-only-down isolates ---

    #[tokio::test]
    async fn drive_only_down_isolates() {
        let backend = FakeBackend::new(FakeState::ServiceDown {
            service: FakeService::Drive,
        });
        let p = prober(backend.clone(), FakeClock::new());

        // The link is Online; only Drive's probe failed once.
        assert_eq!(p.probe().await, NetworkState::Online);
        // One failure is below the breaker threshold, so Drive stays
        // Closed, but its single failure was recorded and isolated to
        // Drive: the other services are untouched / healthy.
        assert_eq!(p.service_health(ServiceName::Drive), ServiceHealth::Closed);
        assert_eq!(
            p.service_health(ServiceName::UpdateEndpoint),
            ServiceHealth::Closed
        );
        assert_eq!(p.service_health(ServiceName::Github), ServiceHealth::Closed);
    }

    // --- DESIGN s5.8.3: 5 consecutive failures open the breaker ---

    #[tokio::test]
    async fn five_consecutive_failures_open_breaker() {
        let backend = FakeBackend::new(FakeState::Online);
        let clock = FakeClock::new();
        let p = prober(backend.clone(), clock.clone());

        // Below threshold: still Closed.
        for _ in 0..(BREAKER_OPEN_THRESHOLD - 1) {
            p.note_outcome(ServiceName::Drive, false);
        }
        assert_eq!(p.service_health(ServiceName::Drive), ServiceHealth::Closed);

        // The 5th consecutive failure opens it.
        p.note_outcome(ServiceName::Drive, false);
        match p.service_health(ServiceName::Drive) {
            ServiceHealth::Open { retry_at } => {
                // First backoff is 30s from now (clock at 0).
                assert_eq!(retry_at, BACKOFF_SCHEDULE_MS[0]);
            }
            other => panic!("expected Open, got {other:?}"),
        }

        // A success closes it again (DESIGN s5.8.3 half-open -> Closed).
        clock.advance(Duration::from_millis(BACKOFF_SCHEDULE_MS[0] as u64));
        assert_eq!(
            p.service_health(ServiceName::Drive),
            ServiceHealth::HalfOpen
        );
        p.note_outcome(ServiceName::Drive, true);
        assert_eq!(p.service_health(ServiceName::Drive), ServiceHealth::Closed);
    }

    // --- DESIGN s5.8.1 row: intermittent opens then closes the breaker ---

    #[tokio::test]
    async fn intermittent_opens_then_closes_breaker() {
        let backend = FakeBackend::new(FakeState::Online);
        let clock = FakeClock::new();
        let p = prober(backend.clone(), clock.clone());

        // Down phase: force every service probe to fail until the breaker
        // opens.
        backend.force_service_outcome(Some(ProbeOutcome::NetworkError));
        for _ in 0..BREAKER_OPEN_THRESHOLD {
            let _ = p.probe().await;
        }
        assert!(matches!(
            p.service_health(ServiceName::Drive),
            ServiceHealth::Open { .. }
        ));

        // Up phase: backoff elapses (HalfOpen), then a clean probe closes
        // the breaker.
        clock.advance(Duration::from_millis(BACKOFF_SCHEDULE_MS[0] as u64));
        assert_eq!(
            p.service_health(ServiceName::Drive),
            ServiceHealth::HalfOpen
        );
        backend.force_service_outcome(Some(ProbeOutcome::Ok));
        let _ = p.probe().await;
        assert_eq!(p.service_health(ServiceName::Drive), ServiceHealth::Closed);
    }

    // --- DESIGN s5.8.5: pool teardown at 3 network-level failures,
    //     independent of the 5-failure breaker open ---

    #[tokio::test]
    async fn pool_teardown_at_three_network_failures() {
        let backend = FakeBackend::new(FakeState::Online);
        let clock = FakeClock::new();
        let p = prober(backend.clone(), clock.clone());

        backend.force_service_outcome(Some(ProbeOutcome::NetworkError));
        // Two network-level failures: no teardown yet, breaker still Closed.
        p.probe().await;
        p.probe().await;
        assert_eq!(backend.pool_drops.load(Ordering::SeqCst), 0);
        assert_eq!(p.service_health(ServiceName::Drive), ServiceHealth::Closed);

        // Third trips the pool teardown (DESIGN s5.8.5) while the breaker
        // is still Closed (opens only at 5).
        p.probe().await;
        assert!(backend.pool_drops.load(Ordering::SeqCst) >= 1);
        assert_eq!(p.service_health(ServiceName::Drive), ServiceHealth::Closed);
    }

    // --- breaker re-open uses the next backoff step (exponential) ---

    #[tokio::test]
    async fn reopen_advances_backoff_schedule() {
        let clock = FakeClock::new();
        let breaker = StdCircuitBreaker::new();

        // Open it: 5 failures -> retry_at = now + 30s.
        for _ in 0..BREAKER_OPEN_THRESHOLD {
            breaker.record_failure(&clock);
        }
        match breaker.health(&clock) {
            ServiceHealth::Open { retry_at } => assert_eq!(retry_at, BACKOFF_SCHEDULE_MS[0]),
            other => panic!("expected Open, got {other:?}"),
        }

        // Advance to half-open, then fail the probe: re-open with the next
        // step (1m).
        clock.advance(Duration::from_millis(BACKOFF_SCHEDULE_MS[0] as u64));
        assert_eq!(breaker.health(&clock), ServiceHealth::HalfOpen);
        breaker.record_failure(&clock);
        match breaker.health(&clock) {
            ServiceHealth::Open { retry_at } => {
                assert_eq!(retry_at, BACKOFF_SCHEDULE_MS[0] + BACKOFF_SCHEDULE_MS[1]);
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }
}
