//! `driven-net` - the production [`Backend`] behind `driven-core`'s
//! [`NetworkProbe`] seam (CODEX_NOTES P2-9, DESIGN s5.8).
//!
//! `driven-core::network` ships only the transport-agnostic probe topology
//! ([`Prober`](driven_core::network)) + the per-service circuit breakers; it
//! stays I/O-free. This crate fills the [`Backend`] seam with the concrete
//! clients (DESIGN s5.8.2 probe topology, s5.8.4 per-service timeouts):
//!
//! - [`Backend::os_online`] - the OS connectivity API (cheapest first probe).
//! - [`Backend::probe_captive`] - an HTTP GET to
//!   `http://www.gstatic.com/generate_204` via a `reqwest::Client` with the
//!   3s-connect / 5s-total captive-portal timeouts, classifying a
//!   non-204/redirect/body as [`ProbeOutcome::CaptivePortal`].
//! - [`Backend::probe_service`] - the per-service health request on a
//!   per-service `reqwest::Client` carrying that service's timeouts, with
//!   `hickory-resolver` re-resolving DNS each call.
//! - [`Backend::drop_pool`] - discards the per-service `reqwest::Client`
//!   (its pooled connections) after the pool-teardown threshold (DESIGN
//!   s5.8.5).
//!
//! M4 scaffold: the type surface + dependency wiring are in place; the
//! method bodies are `todo!()` for the implement phase.

use async_trait::async_trait;
use driven_core::network::{Backend, ProbeOutcome, ServiceName};

/// Tracing target for the production network backend.
const TARGET: &str = "driven::net::backend";

/// The production [`Backend`] for the [`NetworkProbe`](driven_core::network)
/// topology, backed by `reqwest` (rustls) for the HTTP probes + Drive
/// traffic and `hickory-resolver` for DNS re-resolution (DESIGN s5.8.2).
///
/// Holds one `reqwest::Client` per probed service so a per-service pool
/// teardown (DESIGN s5.8.5) discards only that service's pooled connections.
/// The OS connectivity probe is the cheapest first step (DESIGN s5.8.2) and
/// short-circuits the topology when offline.
pub struct ReqwestBackend {
    _private: (),
}

impl ReqwestBackend {
    /// Builds a [`ReqwestBackend`], constructing the per-service `reqwest`
    /// clients with their DESIGN s5.8.4 timeouts (redirect-disabled for the
    /// captive probe so a portal's 302 is observable rather than followed)
    /// and the `hickory-resolver` used for the DNS re-resolution probe.
    ///
    /// Returns an error if a client / resolver cannot be constructed (TLS
    /// backend init, resolver-from-system-conf, etc.).
    pub fn new() -> anyhow::Result<Self> {
        let _ = TARGET;
        todo!("M4 implement: build per-service reqwest clients + hickory resolver")
    }
}

#[async_trait]
impl Backend for ReqwestBackend {
    async fn os_online(&self) -> bool {
        todo!("M4 implement: OS connectivity probe (INetworkListManager / NWPathMonitor / NetworkManager)")
    }

    async fn probe_captive(&self) -> ProbeOutcome {
        todo!("M4 implement: GET http://www.gstatic.com/generate_204; non-204/redirect/body -> CaptivePortal")
    }

    async fn probe_service(&self, service: ServiceName) -> ProbeOutcome {
        let _ = service;
        todo!("M4 implement: per-service health probe with hickory DNS re-resolution + per-service timeouts")
    }

    async fn drop_pool(&self, service: ServiceName) {
        let _ = service;
        todo!("M4 implement: discard the per-service reqwest client / pooled connections (DESIGN s5.8.5)")
    }
}
