//! Network-resilience surface: probe topology, per-service circuit
//! breakers, and the observed-state classification the orchestrator gates
//! on (DESIGN s5.8).
//!
//! This module is the I/O-free *contract* the M3 network implementer
//! fills. The real probes use `reqwest` (0.12, rustls) and
//! `hickory-resolver` (0.24) to run the three-probe topology (DESIGN
//! s5.8.2): OS-connectivity, captive-portal (`generate_204`), and one
//! service-specific probe per dependency. Those crates live in
//! `[workspace.dependencies]` and are wired into the implementer crate
//! later; `driven-core` stays I/O-free (lib.rs), so only the trait and
//! the value types live here.
//!
//! The orchestrator (DESIGN s5.8.6) translates a [`NetworkState`] +
//! per-service [`ServiceHealth`] into a [`PauseReason`](crate::types::PauseReason)
//! (offline / metered / service-down) or lets a partial-availability batch
//! proceed for the services that are up.

use async_trait::async_trait;

use crate::time::Clock;

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
        retry_at: crate::types::UnixMs,
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

use serde::{Deserialize, Serialize};

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
