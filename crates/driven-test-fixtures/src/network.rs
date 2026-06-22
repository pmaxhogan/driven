//! [`FakeNetwork`] - test harness for the network-resilience subsystem
//! (DESIGN s5.8).
//!
//! M3 lands the real network-resilience layer (probe topology, per-service
//! circuit breakers, captive-portal detection, etc.). The fixture here is
//! the shared piece M3's tests and any earlier orchestrator tests use to
//! simulate every failure mode catalogued in DESIGN s5.8.1 without
//! standing up a real reqwest stack.
//!
//! Usage shape:
//!
//! - A test mutates [`FakeNetwork::set_state`] to drive the simulated
//!   network into one of the [`NetworkState`] variants.
//! - The network-probe layer (M3) reads [`FakeNetwork::state`] from the
//!   orchestrator's [`PowerSource`](driven_power::PowerSource) /
//!   network-probe wiring and translates the variant into the
//!   appropriate probe response.
//!
//! Today the harness is intentionally state-bag-shaped; richer behaviour
//! (per-probe response shaping, latency injection, partial-success
//! windows) lands alongside the M3 network layer that consumes it. Tests
//! that need just "network up vs down" should reach for the
//! [`PowerState::network_reachable`](driven_power::PowerState::network_reachable)
//! flag on a [`FakePowerSource`](crate::power::FakePowerSource) instead.

use parking_lot::RwLock;
use std::sync::Arc;

/// Identifies a service-specific dependency (DESIGN s5.8.2 probe
/// topology). The probe layer keeps one circuit-breaker per service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceName {
    /// Google Drive (`/drive/v3/about` probe, plus all upload / download
    /// traffic).
    Drive,
    /// Driven's own update manifest endpoint
    /// (`driven.maxhogan.dev/updates/_health`).
    UpdateEndpoint,
    /// `api.github.com` release-notes fetch.
    Github,
    /// The Cloudflare-hosted telemetry sink.
    Telemetry,
}

/// Simulated network state. One of these is "in effect" at a time;
/// transitions are driven explicitly by the test.
///
/// Variants map to DESIGN s5.8.1 failure modes:
///
/// - [`NetworkState::Online`] - all probes succeed.
/// - [`NetworkState::Offline`] - the OS reports no connectivity.
/// - [`NetworkState::NoInternet`] - OS connectivity is up but the
///   captive-portal / `generate_204` probe fails.
/// - [`NetworkState::DnsFail`] - resolver returns no answer.
/// - [`NetworkState::CaptivePortal`] - captive portal detected.
/// - [`NetworkState::Lossy`] - packet loss + added latency. Drop rate
///   `drop_pct` in `0..=100`; added latency in ms.
/// - [`NetworkState::Intermittent`] - flapping between up and down with
///   the given on/off cadence (seconds).
/// - [`NetworkState::ServiceDown`] - everything is healthy except the
///   named service.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NetworkState {
    /// All probes succeed. Default starting state.
    #[default]
    Online,
    /// OS reports no active network connection.
    Offline,
    /// Connected to a network but `generate_204` fails (link-local only).
    NoInternet,
    /// DNS resolver returns no answer for known-good domains.
    DnsFail,
    /// Captive-portal `generate_204` returns non-204.
    CaptivePortal,
    /// Lossy network. Drop probability per request and added latency.
    Lossy {
        /// Drop probability in `0..=100`. `100` drops every request.
        drop_pct: u8,
        /// Added latency in milliseconds applied to every request that
        /// is not dropped.
        added_latency_ms: u32,
    },
    /// Flapping network. `up_secs` up, then `down_secs` down, repeating.
    Intermittent {
        /// Seconds the network is up in each cycle.
        up_secs: u32,
        /// Seconds the network is down in each cycle.
        down_secs: u32,
    },
    /// One specific service is down while everything else is healthy.
    ServiceDown {
        /// The downed service.
        service: ServiceName,
    },
}

/// In-memory network state holder used by tests and (eventually, M3) by
/// the network-probe layer's in-test build.
///
/// Cheap to [`Clone`] - shares state through an [`Arc`].
///
/// ```ignore
/// use driven_test_fixtures::network::{FakeNetwork, NetworkState, ServiceName};
///
/// let net = FakeNetwork::new();
/// assert_eq!(net.state(), NetworkState::Online);
/// net.set_state(NetworkState::ServiceDown { service: ServiceName::Drive });
/// match net.state() {
///     NetworkState::ServiceDown { service } => assert_eq!(service, ServiceName::Drive),
///     other => panic!("unexpected: {:?}", other),
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct FakeNetwork {
    state: Arc<RwLock<NetworkState>>,
}

impl FakeNetwork {
    /// Constructs a new harness in [`NetworkState::Online`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Constructs a new harness initialised to `state`.
    pub fn with_state(state: NetworkState) -> Self {
        Self {
            state: Arc::new(RwLock::new(state)),
        }
    }

    /// Returns the current simulated state.
    pub fn state(&self) -> NetworkState {
        self.state.read().clone()
    }

    /// Overwrites the current simulated state.
    pub fn set_state(&self, next: NetworkState) {
        *self.state.write() = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_online() {
        let n = FakeNetwork::new();
        assert_eq!(n.state(), NetworkState::Online);
    }

    #[test]
    fn set_and_read_round_trip() {
        let n = FakeNetwork::new();
        n.set_state(NetworkState::CaptivePortal);
        assert_eq!(n.state(), NetworkState::CaptivePortal);
    }

    #[test]
    fn service_down_carries_service() {
        let n = FakeNetwork::with_state(NetworkState::ServiceDown {
            service: ServiceName::Drive,
        });
        match n.state() {
            NetworkState::ServiceDown { service } => {
                assert_eq!(service, ServiceName::Drive);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }

    #[test]
    fn clone_shares_state() {
        let a = FakeNetwork::new();
        let b = a.clone();
        a.set_state(NetworkState::Offline);
        assert_eq!(b.state(), NetworkState::Offline);
    }
}
