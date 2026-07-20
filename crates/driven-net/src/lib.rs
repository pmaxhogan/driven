//! `driven-net` - the production [`Backend`] behind `driven-core`'s
//! [`NetworkProbe`] seam (CODEX_NOTES P2-9, DESIGN s5.8).
//!
//! `driven-core::network` ships only the transport-agnostic probe topology
//! ([`Prober`](driven_core::network)) + the per-service circuit breakers; it
//! stays I/O-free. This crate fills the [`Backend`] seam with the concrete
//! clients (DESIGN s5.8.2 probe topology, s5.8.4 per-service timeouts):
//!
//! - [`Backend::os_online`] - the DESIGN s5.8.2 probe-1 reachability check. It
//!   consults the NATIVE per-OS connectivity API FIRST
//!   (`INetworkListManager::GetConnectivity` on Windows, NetworkManager's
//!   `Connectivity` enum on Linux, `NWPathMonitor` on macOS - see
//!   [`reachability`]); a confident native verdict (online / offline) is used
//!   directly, and when the native read is unavailable or ambiguous it FALLS
//!   BACK transparently to a fast dual-stack TCP connect to a well-known anycast
//!   resolver within the 3s OS-probe budget. A hard offline verdict
//!   short-circuits the topology to offline; any softer ambiguity defers to the
//!   captive + service probes.
//! - [`Backend::probe_captive`] - an HTTP GET to
//!   `http://www.gstatic.com/generate_204` via a redirect-disabled
//!   `reqwest::Client` with the 3s-connect / 5s-total captive-portal
//!   timeouts, classifying a non-204 / redirect / non-empty body as
//!   [`ProbeOutcome::CaptivePortal`].
//! - [`Backend::probe_service`] - the per-service health request on a
//!   per-service `reqwest::Client` carrying that service's DESIGN s5.8.4
//!   timeouts, with `tokio::net::lookup_host` re-resolving DNS each call
//!   (never caching a failed resolve) so a DNS failure is surfaced as
//!   [`ProbeOutcome::DnsFailed`] distinctly from a connect failure (DESIGN
//!   s5.8.1: the DNS probe is `tokio::net::lookup_host` within the 3s budget;
//!   `hickory-resolver` is only the optional s5.8.5 escalation "if we discover
//!   OS resolver pathologies in the field", not wired in V1).
//! - [`Backend::drop_pool`] - rebuilds the per-service `reqwest::Client`
//!   (discarding its pooled connections) after the pool-teardown threshold
//!   (DESIGN s5.8.5).
//!
//! Proxy support (`HTTP_PROXY` / `HTTPS_PROXY`, DESIGN s5.8.7) is left to
//! `reqwest`'s built-in env-proxy handling: the clients are built WITHOUT
//! `.no_proxy()`, so the env vars are honoured.

mod classify;
mod reachability;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use driven_core::network::{Backend, ProbeOutcome, ServiceName};
use driven_tls::CustomCaConfig;

/// Tracing target for the production network backend.
const TARGET: &str = "driven::net::backend";

// --- DESIGN s5.8.2 probe endpoints -----------------------------------------

/// Captive-portal detection endpoint (DESIGN s5.8.2 probe 2). HTTP (not
/// HTTPS) on purpose: a portal intercepts cleartext cleanly, whereas an
/// HTTPS probe would fail on the portal's cert and tell us nothing.
const CAPTIVE_URL: &str = "http://www.gstatic.com/generate_204";

/// Host for the captive probe's DNS re-resolution (DESIGN s5.8.1: re-resolve
/// each call). Must match [`CAPTIVE_URL`]'s authority.
const CAPTIVE_HOST: &str = "www.gstatic.com";

/// Drive health probe (DESIGN s5.8.2 probe 3 / s5.8.1 "Drive API down" row).
/// Unauthenticated on purpose: ANY HTTP response - including the expected
/// `401` - proves the service is reachable. We deliberately do NOT attach an
/// access token here; the prober only needs reachability, not authorization.
const DRIVE_PROBE_URL: &str = "https://www.googleapis.com/drive/v3/about?fields=user";

/// Authority of [`DRIVE_PROBE_URL`] for DNS re-resolution.
const DRIVE_HOST: &str = "www.googleapis.com";

/// Driven's own update-manifest health endpoint (DESIGN s5.8.2 probe 3 /
/// s5.8.1 "Our update endpoint down" row). Probed with `HEAD`.
const UPDATE_PROBE_URL: &str = "https://driven.maxhogan.dev/updates/_health";

/// Authority of [`UPDATE_PROBE_URL`] for DNS re-resolution.
const UPDATE_HOST: &str = "driven.maxhogan.dev";

/// GitHub API root (DESIGN s5.8.2 probe 3 / s5.8.1 "GitHub releases down"
/// row). Any response means the API is up.
const GITHUB_PROBE_URL: &str = "https://api.github.com/";

/// Authority of [`GITHUB_PROBE_URL`] for DNS re-resolution.
const GITHUB_HOST: &str = "api.github.com";

/// The port we pair with a host for the [`tokio::net::lookup_host`] DNS probe.
/// `lookup_host` resolves a `(host, port)` pair; the port is irrelevant to the
/// name-resolution we are testing (we never connect this socket), so we use
/// the HTTPS port the probed services actually listen on.
const DNS_PROBE_PORT: u16 = 443;

/// `User-Agent` sent on every probe so server-side logs/ratelimits can
/// attribute the traffic (GitHub in particular rejects request with no UA).
const USER_AGENT: &str = concat!("driven-net/", env!("CARGO_PKG_VERSION"));

// --- DESIGN s5.8.4 timeouts -------------------------------------------------

/// Connect timeout for the OS + captive probes (DESIGN s5.8.4 row 1-2).
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Total request timeout for the captive probe (DESIGN s5.8.4 row 2).
const CAPTIVE_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect timeout for the metadata-class service probes (DESIGN s5.8.4:
/// Drive metadata / update manifest / release-notes rows all use 10s).
const SERVICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Drive metadata total / idle timeouts (DESIGN s5.8.4 "Drive metadata" row:
/// 10s connect / 30s total / 10s idle).
const DRIVE_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const DRIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Update-manifest total / idle timeouts (DESIGN s5.8.4 "Update manifest
/// fetch" row: 10s connect / 15s total / 5s idle).
const UPDATE_TOTAL_TIMEOUT: Duration = Duration::from_secs(15);
const UPDATE_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Release-notes total / idle timeouts (DESIGN s5.8.4 "Release-notes fetch"
/// row: 10s connect / 15s total / 5s idle).
const GITHUB_TOTAL_TIMEOUT: Duration = Duration::from_secs(15);
const GITHUB_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// DNS budget (DESIGN s5.8.1 "DNS broken" row: resolve within 3s).
const DNS_TIMEOUT: Duration = Duration::from_secs(3);

// --- DESIGN s5.8.5 connection hygiene --------------------------------------

/// Max idle pooled connections per host (DESIGN s5.8.5).
const POOL_MAX_IDLE_PER_HOST: usize = 4;
/// Pool idle timeout (DESIGN s5.8.5): force-close long-idle connections so a
/// silently-dropped ISP TCP session does not hang the next request.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Well-known anycast resolver endpoints for the cheap [`Backend::os_online`]
/// reachability check. TCP/53 is reachable when the machine has working
/// Internet routing; we try both a primary and a fallback, on both IPv4 and
/// IPv6 (happy-eyeballs style), so a single-stack outage does not read as
/// offline.
const OS_ONLINE_ENDPOINTS: [&str; 4] = [
    "8.8.8.8:53",                // Google DNS, IPv4
    "1.1.1.1:53",                // Cloudflare DNS, IPv4
    "[2001:4860:4860::8888]:53", // Google DNS, IPv6
    "[2606:4700:4700::1111]:53", // Cloudflare DNS, IPv6
];

/// The production [`Backend`] for the [`NetworkProbe`](driven_core::network)
/// topology, backed by `reqwest` (rustls) for the HTTP probes + Drive traffic
/// and `tokio::net::lookup_host` for DNS re-resolution (DESIGN s5.8.1 /
/// s5.8.2).
///
/// Holds one `reqwest::Client` per probed service (behind a recoverable
/// [`Mutex`]) so a per-service pool teardown (DESIGN s5.8.5) rebuilds only
/// that service's client - discarding its pooled connections - while the
/// others keep their warm pools. The captive client is shared (one endpoint)
/// and rebuildable the same way.
pub struct ReqwestBackend {
    /// Redirect-disabled HTTP client for the `generate_204` captive probe.
    captive: Mutex<reqwest::Client>,
    /// Per-service clients keyed by [`ServiceName`]. Telemetry is never
    /// probed (best-effort) so it has no client.
    drive: Mutex<reqwest::Client>,
    update_endpoint: Mutex<reqwest::Client>,
    github: Mutex<reqwest::Client>,
    /// Latches `true` the first time [`Backend::os_online`] falls back from the
    /// native reachability read to the TCP probe, so the fallback is logged at
    /// debug level exactly ONCE (not per-probe) on a host whose native API is
    /// permanently unavailable (e.g. no NetworkManager).
    native_fallback_logged: AtomicBool,
    /// Issue #34: the user-configured custom root CA (if any), retained so a
    /// [`Self::drop_pool`] rebuild re-applies the SAME additive trust as the
    /// original client build. Empty (`None`) = system trust only.
    ca: CustomCaConfig,
}

impl ReqwestBackend {
    /// Builds a [`ReqwestBackend`], constructing the per-service `reqwest`
    /// clients with their DESIGN s5.8.4 timeouts (redirect-disabled for the
    /// captive probe so a portal's 30x is observable rather than followed).
    /// DNS re-resolution uses `tokio::net::lookup_host` directly (DESIGN
    /// s5.8.1), so there is no resolver object to build.
    ///
    /// `ca` is the user-configured custom root CA (issue #34): additive on top of
    /// the OS/enterprise trust store, retained for pool rebuilds. Pass
    /// [`CustomCaConfig::none`] for system trust only.
    ///
    /// Returns an error if a client (TLS backend init) cannot be constructed, or
    /// if a configured custom CA is missing / unreadable / unparseable
    /// (fail-closed: the backend is not built with a silently-ignored CA).
    pub fn new(ca: CustomCaConfig) -> anyhow::Result<Self> {
        let captive = build_captive_client(&ca)?;
        let drive = build_service_client(ServiceName::Drive, &ca)?;
        let update_endpoint = build_service_client(ServiceName::UpdateEndpoint, &ca)?;
        let github = build_service_client(ServiceName::Github, &ca)?;

        tracing::debug!(
            target: TARGET,
            custom_ca = ca.is_enabled(),
            "ReqwestBackend constructed (per-service clients + lookup_host DNS probe)"
        );

        Ok(Self {
            captive: Mutex::new(captive),
            drive: Mutex::new(drive),
            update_endpoint: Mutex::new(update_endpoint),
            github: Mutex::new(github),
            native_fallback_logged: AtomicBool::new(false),
            ca,
        })
    }

    /// Locks an interior client, recovering from a poisoned lock rather than
    /// panicking (house rule: no `unwrap`/`expect`/`panic!` in non-test
    /// code). A poisoned client mutex only means a prior holder panicked mid-
    /// rebuild; the contained `reqwest::Client` is still a valid clone source.
    fn lock<'a>(m: &'a Mutex<reqwest::Client>) -> std::sync::MutexGuard<'a, reqwest::Client> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Returns the per-service client mutex, or `None` for the never-probed
    /// best-effort [`ServiceName::Telemetry`].
    fn service_client(&self, service: ServiceName) -> Option<&Mutex<reqwest::Client>> {
        match service {
            ServiceName::Drive => Some(&self.drive),
            ServiceName::UpdateEndpoint => Some(&self.update_endpoint),
            ServiceName::Github => Some(&self.github),
            ServiceName::Telemetry => None,
        }
    }

    /// Clones the current `reqwest::Client` for `service` (cheap: a `Client`
    /// is an `Arc` internally) so the actual HTTP call runs without holding
    /// the lock across `.await`.
    fn clone_service_client(&self, service: ServiceName) -> Option<reqwest::Client> {
        self.service_client(service).map(|m| Self::lock(m).clone())
    }

    /// The TCP-probe fallback for [`Backend::os_online`]: a fast dual-stack TCP
    /// connect to a small set of well-known anycast DNS resolvers (Google
    /// `8.8.8.8` / Cloudflare `1.1.1.1`, both IPv4 and IPv6) within the 3s
    /// OS-probe budget.
    ///
    /// If ANY connect succeeds, routing to the Internet exists -> `true`. If ALL
    /// fail (no route / refused / timeout on every endpoint and address family),
    /// there is no working route -> `false`. Endpoints span IPv4 + IPv6 so a
    /// single-stack outage does not read as fully offline (DESIGN s5.8.1 "No
    /// IPv4 / no IPv6" row). This is the transparent fallback used whenever the
    /// native OS connectivity read is unavailable or ambiguous.
    async fn tcp_os_online(&self) -> bool {
        // Try each endpoint with the 3s connect budget; the first success wins.
        for endpoint in OS_ONLINE_ENDPOINTS {
            let addr: SocketAddr = match endpoint.parse() {
                Ok(a) => a,
                // A malformed constant is a programming error, not a runtime
                // network condition; skip it rather than panic.
                Err(_) => continue,
            };
            match tokio::time::timeout(PROBE_CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr))
                .await
            {
                Ok(Ok(_stream)) => {
                    tracing::trace!(target: TARGET, %addr, "tcp_os_online: reachable");
                    return true;
                }
                Ok(Err(e)) => {
                    tracing::trace!(target: TARGET, %addr, error = %e, "tcp_os_online: connect failed");
                }
                Err(_elapsed) => {
                    tracing::trace!(target: TARGET, %addr, "tcp_os_online: connect timed out");
                }
            }
        }
        tracing::debug!(
            target: TARGET,
            "tcp_os_online: all reachability endpoints failed -> offline"
        );
        false
    }

    /// Re-resolves `host` via `tokio::net::lookup_host` with no application-
    /// layer cache (DESIGN s5.8.1: re-resolve every call, never cache a failed
    /// resolve - `lookup_host` defers to the OS resolver, whose own
    /// TTL-honouring cache is the only cache, DESIGN s5.8.5).
    ///
    /// Returns:
    /// - `Ok(())` when at least one address came back (name resolves).
    /// - `Err(ProbeOutcome::DnsFailed)` for any resolve failure - an
    ///   `io::Error` from `lookup_host` (NXDOMAIN / SERVFAIL / no nameserver /
    ///   transport error), an empty answer, or the 3s budget elapsing. Every
    ///   one blocks the probe at the DNS step and is the DESIGN s5.8.1 "DNS
    ///   broken" condition. This is deliberately kept distinct from a *connect*
    ///   failure to an already-resolved IP, which the HTTP layer classifies as
    ///   [`ProbeOutcome::NetworkError`].
    ///
    /// The whole resolve is bounded by [`DNS_TIMEOUT`] so a black-holed
    /// resolver cannot exceed the 3s DNS budget.
    async fn resolve(&self, host: &str) -> Result<(), ProbeOutcome> {
        resolve_within(
            DNS_TIMEOUT,
            host,
            tokio::net::lookup_host((host, DNS_PROBE_PORT)),
        )
        .await
    }
}

/// Bounds a DNS-lookup future by `budget` and classifies its result per the
/// DESIGN s5.8.1 "DNS broken" contract (see [`ReqwestBackend::resolve`]).
///
/// Extracted from [`ReqwestBackend::resolve`] over a generic `lookup` future
/// so the 3s no-hang bound is *deterministically* testable against the real
/// production classification logic: a never-resolving lookup must surface
/// [`ProbeOutcome::DnsFailed`] once `budget` elapses rather than hang, and the
/// success / empty-answer / error arms are each exercised without touching a
/// real resolver. The production call site passes a real
/// `tokio::net::lookup_host` future; tests pass a `pending()` /
/// `ready(...)` future on a paused tokio clock.
///
/// Returns `Ok(())` only when the lookup resolves at least one address within
/// the budget; every other path (timeout, empty answer, or `io::Error` -
/// NXDOMAIN / SERVFAIL / no-nameserver / transport failure) is the DESIGN
/// s5.8.1 "DNS broken" condition, kept deliberately distinct from a *connect*
/// failure to an already-resolved IP (which the HTTP layer classifies as
/// [`ProbeOutcome::NetworkError`] so the orchestrator can use the 60s DNS
/// re-probe cadence vs the 30s no-Internet cadence).
async fn resolve_within<F, I>(budget: Duration, host: &str, lookup: F) -> Result<(), ProbeOutcome>
where
    F: std::future::Future<Output = std::io::Result<I>>,
    I: Iterator<Item = SocketAddr>,
{
    match tokio::time::timeout(budget, lookup).await {
        // Bounding timeout elapsed: the resolver black-holed. The DNS budget
        // is exceeded -> treat as DNS failure (DESIGN s5.8.1 "must not hang").
        Err(_elapsed) => {
            tracing::warn!(target: TARGET, host, "DNS resolve exceeded {budget:?}");
            Err(ProbeOutcome::DnsFailed)
        }
        Ok(Ok(mut addrs)) => {
            if addrs.next().is_some() {
                Ok(())
            } else {
                // An empty answer (no A/AAAA records) is the DESIGN s5.8.1
                // "DNS broken" condition just like an error.
                tracing::warn!(target: TARGET, host, "DNS resolve returned no records");
                Err(ProbeOutcome::DnsFailed)
            }
        }
        Ok(Err(err)) => {
            tracing::warn!(target: TARGET, host, error = %err, "DNS resolve failed");
            Err(ProbeOutcome::DnsFailed)
        }
    }
}

#[async_trait]
impl Backend for ReqwestBackend {
    /// The DESIGN s5.8.2 probe-1 reachability check: native OS connectivity
    /// API first, TCP probe as the transparent fallback.
    ///
    /// Consults the native per-OS connectivity API
    /// ([`reachability::detect_reachability`]:
    /// `INetworkListManager::GetConnectivity` on Windows, NetworkManager's
    /// `Connectivity` enum on Linux, `NWPathMonitor` on macOS) FIRST. A
    /// confident native verdict is used directly - an active connection
    /// (internet, or link-local / limited / captive-portal) returns `true` so
    /// the captive + service probes (DESIGN s5.8.2 probes 2-3) classify Online
    /// vs NoInternet vs CaptivePortal; a confident no-connection verdict returns
    /// `false`, short-circuiting the topology to
    /// [`NetworkState::Offline`](driven_core::network::NetworkState) without
    /// firing them (airplane mode / rfkill / no interface, DESIGN s5.8.1).
    ///
    /// When the native read is unavailable or ambiguous (no NetworkManager, a
    /// COM error, a not-yet-delivered `NWPath`, or a target with no native
    /// backend), it FALLS BACK transparently to [`Self::tcp_os_online`] - a fast
    /// dual-stack TCP connect to well-known anycast resolvers. The fallback is
    /// logged once at debug level, never per-call.
    async fn os_online(&self) -> bool {
        match reachability::resolve_native(reachability::detect_reachability().await) {
            Some(online) => online,
            None => {
                // Native read inconclusive (API unavailable or an ambiguous
                // verdict): fall back to the TCP probe. Log the switch once so a
                // host with no native API does not spam the log every probe.
                if !self.native_fallback_logged.swap(true, Ordering::Relaxed) {
                    tracing::debug!(
                        target: TARGET,
                        "native reachability inconclusive, using TCP probe (logged once)"
                    );
                }
                self.tcp_os_online().await
            }
        }
    }

    /// Runs the captive-portal `generate_204` probe (DESIGN s5.8.2 probe 2).
    ///
    /// DNS is re-resolved first (DESIGN s5.8.1) so a name-resolution failure
    /// surfaces as [`ProbeOutcome::DnsFailed`] rather than a generic connect
    /// error. The request itself uses a redirect-disabled client so a
    /// portal's 30x is observed (not followed) and classified as
    /// [`ProbeOutcome::CaptivePortal`].
    async fn probe_captive(&self) -> ProbeOutcome {
        if let Err(dns_outcome) = self.resolve(CAPTIVE_HOST).await {
            return dns_outcome;
        }
        let client = Self::lock(&self.captive).clone();
        match client.get(CAPTIVE_URL).send().await {
            Ok(resp) => {
                let status = resp.status();
                // Read the body to confirm emptiness (DESIGN s5.8.1: "204 with
                // empty body"). A portal that fakes the 204 status but injects
                // HTML is then still caught.
                let body_is_empty = match resp.bytes().await {
                    Ok(bytes) => bytes.is_empty(),
                    // Could not read the body: be conservative and treat a
                    // 204 whose body we cannot confirm empty as a portal.
                    Err(_) => false,
                };
                let outcome = classify::classify_captive_204(status, body_is_empty);
                tracing::debug!(
                    target: TARGET,
                    %status,
                    body_is_empty,
                    ?outcome,
                    "captive probe result"
                );
                outcome
            }
            Err(err) => {
                let outcome = classify::classify_captive_transport_error(&err);
                tracing::debug!(target: TARGET, error = %err, ?outcome, "captive probe transport error");
                outcome
            }
        }
    }

    /// Runs `service`'s health probe (DESIGN s5.8.2 probe 3).
    ///
    /// Drive uses `GET /drive/v3/about?fields=user` (any HTTP response,
    /// including 401, means reachable); UpdateEndpoint uses `HEAD
    /// /updates/_health`; GitHub uses `GET /`. DNS is re-resolved first
    /// (DESIGN s5.8.1). [`ServiceName::Telemetry`] is never probed and returns
    /// [`ProbeOutcome::Ok`] (best-effort; the prober excludes it anyway).
    async fn probe_service(&self, service: ServiceName) -> ProbeOutcome {
        let (url, host, is_head) = match service {
            ServiceName::Drive => (DRIVE_PROBE_URL, DRIVE_HOST, false),
            ServiceName::UpdateEndpoint => (UPDATE_PROBE_URL, UPDATE_HOST, true),
            ServiceName::Github => (GITHUB_PROBE_URL, GITHUB_HOST, false),
            // Best-effort, never probed (DESIGN s5.8.2 probe 3). The prober
            // excludes Telemetry from its PROBED_SERVICES, so this arm only
            // guards a direct call: report Ok rather than fabricate a probe.
            ServiceName::Telemetry => return ProbeOutcome::Ok,
        };

        if let Err(dns_outcome) = self.resolve(host).await {
            return dns_outcome;
        }

        let client = match self.clone_service_client(service) {
            Some(c) => c,
            // Unreachable for the matched services above; defensive.
            None => return ProbeOutcome::Ok,
        };

        let request = if is_head {
            client.head(url)
        } else {
            client.get(url)
        };

        match request.send().await {
            Ok(resp) => {
                let status = resp.status();
                let outcome = classify::classify_service_status(status);
                tracing::debug!(
                    target: TARGET,
                    ?service,
                    %status,
                    ?outcome,
                    "service probe result"
                );
                outcome
            }
            Err(err) => {
                let outcome = classify::classify_service_transport_error(&err);
                tracing::debug!(
                    target: TARGET,
                    ?service,
                    error = %err,
                    ?outcome,
                    "service probe transport error"
                );
                outcome
            }
        }
    }

    /// Discards `service`'s connection pool by rebuilding its
    /// `reqwest::Client` (DESIGN s5.8.5). The old client - and every pooled
    /// connection it owns - is dropped when the guard's old value is
    /// overwritten, so the next probe opens a fresh pool.
    async fn drop_pool(&self, service: ServiceName) {
        let Some(slot) = self.service_client(service) else {
            // Telemetry has no pooled client; nothing to drop.
            return;
        };
        match build_service_client(service, &self.ca) {
            Ok(fresh) => {
                *Self::lock(slot) = fresh;
                tracing::info!(
                    target: TARGET,
                    ?service,
                    "dropped + rebuilt connection pool (DESIGN s5.8.5)"
                );
            }
            Err(err) => {
                // Rebuild failed (TLS init). Keep the existing client rather
                // than leaving the service with none; log so the field can
                // diagnose. The next teardown trigger will retry.
                tracing::warn!(
                    target: TARGET,
                    ?service,
                    error = %err,
                    "pool rebuild failed; keeping existing client"
                );
            }
        }
    }
}

/// Builds the redirect-disabled captive-portal probe client (DESIGN s5.8.4
/// captive row: 3s connect / 5s total). `ca` adds the user's custom root CA
/// (issue #34) additively; fail-closed if a configured CA cannot be loaded.
fn build_captive_client(ca: &CustomCaConfig) -> anyhow::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        // Observe a portal's 30x rather than follow it (DESIGN s5.8.1).
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(PROBE_CONNECT_TIMEOUT)
        .timeout(CAPTIVE_TOTAL_TIMEOUT)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT);
    // Honour HTTP(S)_PROXY (DESIGN s5.8.7): do NOT call .no_proxy().
    driven_tls::apply_custom_ca(builder, ca)?
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build captive-probe reqwest client: {e}"))
}

/// Builds a per-service probe client with that service's DESIGN s5.8.4
/// timeouts and the s5.8.5 pool hygiene baked in. `ca` adds the user's custom
/// root CA (issue #34) additively; fail-closed if it cannot be loaded.
fn build_service_client(
    service: ServiceName,
    ca: &CustomCaConfig,
) -> anyhow::Result<reqwest::Client> {
    let (total, idle) = match service {
        ServiceName::Drive => (DRIVE_TOTAL_TIMEOUT, DRIVE_IDLE_TIMEOUT),
        ServiceName::UpdateEndpoint => (UPDATE_TOTAL_TIMEOUT, UPDATE_IDLE_TIMEOUT),
        ServiceName::Github => (GITHUB_TOTAL_TIMEOUT, GITHUB_IDLE_TIMEOUT),
        // Telemetry has no probe client; use the conservative update-class
        // timeouts if a client is ever built for it. The backend never calls
        // this for Telemetry (service_client returns None).
        ServiceName::Telemetry => (UPDATE_TOTAL_TIMEOUT, UPDATE_IDLE_TIMEOUT),
    };

    let builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(SERVICE_CONNECT_TIMEOUT)
        // Total request cap (DESIGN s5.8.4). Probes are small metadata calls,
        // so the bounded total applies (resumable chunk uploads, which have
        // "no overall cap", are a separate client owned by the Drive store,
        // not this probe backend).
        .timeout(total)
        // Idle-between-bytes timeout (DESIGN s5.8.4 "Idle" column): catches a
        // stalled-mid-transfer connection without an overall cap.
        .read_timeout(idle)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        // HTTP/2 with adaptive flow control (DESIGN s5.8.5: "reqwest HTTP/2").
        // We negotiate h2 via ALPN over TLS (do NOT force prior-knowledge,
        // which would break non-TLS / ALPN-less peers); adaptive window keeps
        // a single h2 connection from head-of-line-stalling large transfers.
        .http2_adaptive_window(true);
    // Honour HTTP(S)_PROXY (DESIGN s5.8.7): do NOT call .no_proxy().
    driven_tls::apply_custom_ca(builder, ca)?
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build {service:?} probe reqwest client: {e}"))
}

/// Returns the IP families an address parses into - used only to keep the
/// `OS_ONLINE_ENDPOINTS` constants honest in tests (no runtime use).
#[cfg(test)]
fn endpoint_family(endpoint: &str) -> Option<bool> {
    endpoint
        .parse::<SocketAddr>()
        .ok()
        .map(|a| a.ip().is_ipv4())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- construction is infallible in a normal environment ---
    //
    // Building the clients (rustls init) must succeed offline: none of it
    // touches the network. This is a real assertion, not a skip.
    #[test]
    fn backend_constructs_offline() {
        let backend = ReqwestBackend::new(CustomCaConfig::none());
        assert!(
            backend.is_ok(),
            "ReqwestBackend::new must succeed without network: {:?}",
            backend.err()
        );
    }

    // --- per-service client mapping covers exactly the probed services ---

    #[test]
    fn telemetry_has_no_probe_client() {
        let backend = ReqwestBackend::new(CustomCaConfig::none()).expect("construct backend");
        assert!(backend.service_client(ServiceName::Telemetry).is_none());
        assert!(backend.service_client(ServiceName::Drive).is_some());
        assert!(backend
            .service_client(ServiceName::UpdateEndpoint)
            .is_some());
        assert!(backend.service_client(ServiceName::Github).is_some());
    }

    // --- os_online is total: native read (+ TCP fallback) returns a bool ---
    //
    // Exercises the DESIGN s5.8.2 probe-1 path end to end: the native OS
    // reachability read (on this Windows host, the live
    // `INetworkListManager::GetConnectivity` COM call) resolved via
    // `resolve_native`, falling back to the TCP probe only on an inconclusive
    // verdict. The value is environment-dependent (a connected host yields
    // `true`), so we only assert the call is total and never panics across the
    // native/fallback boundary.
    #[tokio::test]
    async fn os_online_is_total() {
        let backend = ReqwestBackend::new().expect("construct backend");
        let _: bool = backend.os_online().await;
    }

    // --- the os_online endpoint constants are well-formed + dual-stack ---

    #[test]
    fn os_online_endpoints_parse_and_span_both_families() {
        let mut v4 = 0;
        let mut v6 = 0;
        for ep in OS_ONLINE_ENDPOINTS {
            match endpoint_family(ep) {
                Some(true) => v4 += 1,
                Some(false) => v6 += 1,
                None => panic!("os_online endpoint does not parse: {ep}"),
            }
        }
        assert!(v4 >= 1, "need at least one IPv4 reachability endpoint");
        assert!(v6 >= 1, "need at least one IPv6 reachability endpoint");
    }

    // --- DNS probe re-resolution is bounded (no hang) ---
    //
    // `.invalid` is reserved (RFC 6761) and never resolves, so this exercises
    // the failure path deterministically through `tokio::net::lookup_host`. It
    // MUST complete within the DNS budget and classify as DnsFailed (DESIGN
    // s5.8.1 "must not hang"). This touches the local OS resolver only (an
    // NXDOMAIN/SERVFAIL answer), not a remote service, so it is a real
    // offline-safe test.
    #[tokio::test]
    async fn resolve_invalid_tld_is_dns_failed_and_bounded() {
        let backend = ReqwestBackend::new(CustomCaConfig::none()).expect("construct backend");
        let started = std::time::Instant::now();
        let outcome = backend.resolve("driven-net-nonexistent.invalid").await;
        let elapsed = started.elapsed();
        assert_eq!(outcome, Err(ProbeOutcome::DnsFailed));
        // The tokio bounding timeout caps it at DNS_TIMEOUT (+ scheduling
        // slack); assert it did not hang far beyond the budget.
        assert!(
            elapsed < DNS_TIMEOUT + Duration::from_secs(2),
            "resolve must be bounded by the DNS budget, took {elapsed:?}"
        );
    }

    // --- localhost resolves: the success path of the lookup_host probe ---
    //
    // `localhost` always resolves on a sane host (no network egress: it hits
    // the OS resolver / hosts file). This exercises the Ok(()) arm so the
    // success path is not left untested after the hickory -> lookup_host
    // switch.
    #[tokio::test]
    async fn resolve_localhost_succeeds() {
        let backend = ReqwestBackend::new(CustomCaConfig::none()).expect("construct backend");
        let outcome = backend.resolve("localhost").await;
        assert_eq!(outcome, Ok(()), "localhost must resolve via lookup_host");
    }

    // --- DNS-no-hang: a black-holed resolver is bounded by the DNS budget ---
    //
    // The M3 acceptance row "DNS fails -> classified, must not hang beyond 3s"
    // (DESIGN s5.8.1). Driven deterministically: a `pending()` lookup that
    // *never* answers is run on a PAUSED tokio clock, so virtual time only
    // advances when the runtime is idle - it jumps straight to the bounding
    // timeout's deadline. The probe must return `DnsFailed` at exactly the DNS
    // budget, proving the `tokio::time::timeout` wrapper - not the resolver -
    // is what stops the hang.
    //
    // Mutation check (proves the test is real, not a no-op): drop the
    // `tokio::time::timeout` in `resolve_within` (await the lookup directly)
    // and this test hangs forever; flip the `Err(_elapsed)` arm to `Ok(())`
    // and the `assert_eq!` below fails. Either way it goes RED.
    #[tokio::test(start_paused = true)]
    async fn resolve_within_bounds_a_blackholed_lookup() {
        let lookup = std::future::pending::<std::io::Result<std::vec::IntoIter<SocketAddr>>>();
        let started = tokio::time::Instant::now();
        let outcome = resolve_within(DNS_TIMEOUT, "black.hole.invalid", lookup).await;
        let elapsed = started.elapsed();

        assert_eq!(
            outcome,
            Err(ProbeOutcome::DnsFailed),
            "a never-answering resolver must surface DnsFailed, not hang"
        );
        assert_eq!(
            elapsed, DNS_TIMEOUT,
            "the bounding timeout must fire at exactly the DNS budget (no longer)"
        );
    }

    // --- DNS classification: success / empty-answer / error arms ---
    //
    // Each non-timeout arm of `resolve_within` is exercised deterministically
    // with a `ready(...)` lookup (no resolver, no clock advance):
    //   - at least one address  -> Ok(())
    //   - an empty answer        -> DnsFailed (DESIGN s5.8.1)
    //   - an io::Error           -> DnsFailed (NXDOMAIN / SERVFAIL / transport)
    //
    // Mutation check: collapse any arm into another (e.g. map the empty-answer
    // case to `Ok(())`) and the matching assertion below fails RED.
    #[tokio::test]
    async fn resolve_within_classifies_each_outcome() {
        let addr: SocketAddr = "127.0.0.1:443".parse().expect("valid socket addr");

        let ok = resolve_within(
            DNS_TIMEOUT,
            "ok.example",
            std::future::ready(Ok::<_, std::io::Error>(vec![addr].into_iter())),
        )
        .await;
        assert_eq!(ok, Ok(()), "a resolved address must be Ok");

        let empty: std::io::Result<std::vec::IntoIter<SocketAddr>> = Ok(Vec::new().into_iter());
        let none = resolve_within(DNS_TIMEOUT, "empty.example", std::future::ready(empty)).await;
        assert_eq!(
            none,
            Err(ProbeOutcome::DnsFailed),
            "an empty answer is DNS broken"
        );

        let err: std::io::Result<std::vec::IntoIter<SocketAddr>> = Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "nxdomain",
        ));
        let failed = resolve_within(DNS_TIMEOUT, "nx.invalid", std::future::ready(err)).await;
        assert_eq!(
            failed,
            Err(ProbeOutcome::DnsFailed),
            "a resolver io::Error is DNS broken"
        );
    }
}
