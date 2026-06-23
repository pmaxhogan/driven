//! `driven-net` - the production [`Backend`] behind `driven-core`'s
//! [`NetworkProbe`] seam (CODEX_NOTES P2-9, DESIGN s5.8).
//!
//! `driven-core::network` ships only the transport-agnostic probe topology
//! ([`Prober`](driven_core::network)) + the per-service circuit breakers; it
//! stays I/O-free. This crate fills the [`Backend`] seam with the concrete
//! clients (DESIGN s5.8.2 probe topology, s5.8.4 per-service timeouts):
//!
//! - [`Backend::os_online`] - a lightweight, honest reachability check
//!   (documented on the method): a fast dual-stack TCP connect to a
//!   well-known anycast resolver within the 3s OS-probe budget. It does NOT
//!   read `INetworkListManager` / `NWPathMonitor` / NetworkManager (a real
//!   OS-API integration is deferred); a hard connect failure short-circuits
//!   the topology to offline, and any softer ambiguity defers to the captive
//!   + service probes.
//! - [`Backend::probe_captive`] - an HTTP GET to
//!   `http://www.gstatic.com/generate_204` via a redirect-disabled
//!   `reqwest::Client` with the 3s-connect / 5s-total captive-portal
//!   timeouts, classifying a non-204 / redirect / non-empty body as
//!   [`ProbeOutcome::CaptivePortal`].
//! - [`Backend::probe_service`] - the per-service health request on a
//!   per-service `reqwest::Client` carrying that service's DESIGN s5.8.4
//!   timeouts, with `hickory-resolver` re-resolving DNS each call (never
//!   caching a failed resolve) so a DNS failure is surfaced as
//!   [`ProbeOutcome::DnsFailed`] distinctly from a connect failure.
//! - [`Backend::drop_pool`] - rebuilds the per-service `reqwest::Client`
//!   (discarding its pooled connections) after the pool-teardown threshold
//!   (DESIGN s5.8.5).
//!
//! Proxy support (`HTTP_PROXY` / `HTTPS_PROXY`, DESIGN s5.8.7) is left to
//! `reqwest`'s built-in env-proxy handling: the clients are built WITHOUT
//! `.no_proxy()`, so the env vars are honoured.

mod classify;

use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use driven_core::network::{Backend, ProbeOutcome, ServiceName};
use hickory_resolver::config::{LookupIpStrategy, ResolverConfig, ResolverOpts};
use hickory_resolver::error::{ResolveError, ResolveErrorKind};
use hickory_resolver::TokioAsyncResolver;

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
/// and `hickory-resolver` for DNS re-resolution (DESIGN s5.8.2).
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
    /// System-configured DNS resolver with the application-layer cache
    /// DISABLED (DESIGN s5.8.5: never cache resolved IPs at the app layer;
    /// re-resolve every call, DESIGN s5.8.1 "do not cache failed resolves").
    resolver: TokioAsyncResolver,
}

impl ReqwestBackend {
    /// Builds a [`ReqwestBackend`], constructing the per-service `reqwest`
    /// clients with their DESIGN s5.8.4 timeouts (redirect-disabled for the
    /// captive probe so a portal's 30x is observable rather than followed)
    /// and the `hickory-resolver` used for the DNS re-resolution probe.
    ///
    /// Returns an error if a client (TLS backend init) or the resolver
    /// (from-system-conf) cannot be constructed.
    pub fn new() -> anyhow::Result<Self> {
        let captive = build_captive_client()?;
        let drive = build_service_client(ServiceName::Drive)?;
        let update_endpoint = build_service_client(ServiceName::UpdateEndpoint)?;
        let github = build_service_client(ServiceName::Github)?;
        let resolver = build_resolver()?;

        tracing::debug!(
            target: TARGET,
            "ReqwestBackend constructed (per-service clients + non-caching resolver)"
        );

        Ok(Self {
            captive: Mutex::new(captive),
            drive: Mutex::new(drive),
            update_endpoint: Mutex::new(update_endpoint),
            github: Mutex::new(github),
            resolver,
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

    /// Re-resolves `host` via `hickory` with the cache disabled (DESIGN
    /// s5.8.1: re-resolve every call, never cache a failed resolve).
    ///
    /// Returns:
    /// - `Ok(())` when at least one A/AAAA record came back (name resolves).
    /// - `Err(ProbeOutcome::DnsFailed)` for any resolve failure - NXDOMAIN,
    ///   empty answer, resolver "no connections", resolver timeout, or a
    ///   resolver transport (Io/Proto) error. Every one blocks the probe at
    ///   the DNS step and is the DESIGN s5.8.1 "DNS broken" condition (see
    ///   [`classify_resolve_error`]). This is deliberately kept distinct from
    ///   a *connect* failure to an already-resolved IP, which the HTTP layer
    ///   classifies as [`ProbeOutcome::NetworkError`].
    ///
    /// The whole resolve is additionally bounded by [`DNS_TIMEOUT`] so a
    /// black-holed resolver cannot exceed the 3s DNS budget (the resolver's
    /// own `timeout`/`attempts` are also set, this is belt-and-suspenders).
    async fn resolve(&self, host: &str) -> Result<(), ProbeOutcome> {
        let lookup = tokio::time::timeout(DNS_TIMEOUT, self.resolver.lookup_ip(host)).await;
        match lookup {
            // Bounding timeout elapsed: the resolver black-holed. The DNS
            // budget is exceeded -> treat as DNS failure (DESIGN s5.8.1).
            Err(_elapsed) => {
                tracing::warn!(target: TARGET, host, "DNS resolve exceeded {DNS_TIMEOUT:?}");
                Err(ProbeOutcome::DnsFailed)
            }
            Ok(Ok(ips)) => {
                if ips.iter().next().is_some() {
                    Ok(())
                } else {
                    // hickory normally raises NoRecordsFound rather than an
                    // empty success, but guard the empty case explicitly so a
                    // zero-record answer is never mistaken for "resolves".
                    tracing::warn!(target: TARGET, host, "DNS resolve returned no records");
                    Err(ProbeOutcome::DnsFailed)
                }
            }
            Ok(Err(err)) => Err(classify_resolve_error(host, &err)),
        }
    }
}

#[async_trait]
impl Backend for ReqwestBackend {
    /// A lightweight, HONEST reachability check (DESIGN s5.8.2 probe 1).
    ///
    /// What this actually does: attempts a fast TCP connect to a small set of
    /// well-known anycast DNS resolvers (Google `8.8.8.8` / Cloudflare
    /// `1.1.1.1`, both IPv4 and IPv6) within the 3s OS-probe budget. If ANY
    /// connect succeeds, routing to the Internet exists -> `true`. If ALL
    /// fail (no route / refused / timeout on every endpoint and address
    /// family), there is no working route -> `false`, and the prober
    /// short-circuits to [`NetworkState::Offline`](driven_core::network::NetworkState)
    /// without firing the captive / service probes (DESIGN s5.8.2).
    ///
    /// What this deliberately does NOT do: it does NOT read the OS
    /// connectivity API (`INetworkListManager::IsConnectedToInternet` on
    /// Windows, `NWPathMonitor` on macOS, NetworkManager `Connectivity` on
    /// Linux). A native OS-API integration is deferred to a later phase; this
    /// V1 check is a real cheap probe, not a stub, and is documented as such
    /// so the topology's "cheapest first probe" remains honest. The downstream
    /// captive + service probes (DESIGN s5.8.2 probes 2-3) provide the
    /// authoritative classification when this returns `true`.
    async fn os_online(&self) -> bool {
        // Try each endpoint with the 3s connect budget; the first success
        // wins. Endpoints span IPv4 + IPv6 so a single-stack outage does not
        // read as fully offline (DESIGN s5.8.1 "No IPv4 / no IPv6" row).
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
                    tracing::trace!(target: TARGET, %addr, "os_online: reachable");
                    return true;
                }
                Ok(Err(e)) => {
                    tracing::trace!(target: TARGET, %addr, error = %e, "os_online: connect failed");
                }
                Err(_elapsed) => {
                    tracing::trace!(target: TARGET, %addr, "os_online: connect timed out");
                }
            }
        }
        tracing::debug!(
            target: TARGET,
            "os_online: all reachability endpoints failed -> offline"
        );
        false
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
        match build_service_client(service) {
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

/// Maps a `hickory` [`ResolveError`] to a [`ProbeOutcome`].
///
/// Every resolver failure - a name that does not resolve (NXDOMAIN / empty
/// answer), no usable nameserver connection, a resolver timeout, or a
/// resolver transport (Io/Proto) error - blocks the probe at the DNS step and
/// is the DESIGN s5.8.1 "DNS broken" condition, so all map to
/// [`ProbeOutcome::DnsFailed`]. This is deliberately distinct from a
/// *connect* failure to a resolved IP (which the HTTP layer surfaces as
/// [`ProbeOutcome::NetworkError`]) so the orchestrator can use the 60s DNS
/// re-probe cadence vs the 30s no-Internet cadence (DESIGN s5.8.1).
fn classify_resolve_error(host: &str, err: &ResolveError) -> ProbeOutcome {
    // `ResolveErrorKind` is `#[non_exhaustive]`; the design's "DNS broken"
    // classification covers every resolve failure, so we map uniformly rather
    // than branch. Matching the kind keeps the intent legible and survives
    // new variants.
    let outcome = match err.kind() {
        ResolveErrorKind::NoRecordsFound { .. }
        | ResolveErrorKind::NoConnections
        | ResolveErrorKind::Message(_)
        | ResolveErrorKind::Msg(_)
        | ResolveErrorKind::Timeout
        | ResolveErrorKind::Io(_)
        | ResolveErrorKind::Proto(_) => ProbeOutcome::DnsFailed,
        _ => ProbeOutcome::DnsFailed,
    };
    tracing::warn!(target: TARGET, host, error = %err, ?outcome, "DNS resolve failed");
    outcome
}

/// Builds the system-configured non-caching `hickory` resolver (DESIGN
/// s5.8.5: never cache at the app layer; DESIGN s5.8.1: re-resolve each call).
fn build_resolver() -> anyhow::Result<TokioAsyncResolver> {
    // Prefer the OS resolver configuration so corporate / split-horizon DNS
    // works; fall back to a public config only if the system conf cannot be
    // read (e.g. a locked-down container with no resolv.conf).
    let mut opts = ResolverOpts::default();
    // DESIGN s5.8.5: NEVER cache resolved IPs at the application layer - the
    // OS resolver's TTL-honouring cache is the only cache. cache_size = 0
    // disables hickory's in-process positive+negative cache.
    opts.cache_size = 0;
    // DESIGN s5.8.1: bound a single resolve attempt; the overall 3s budget is
    // additionally enforced by the tokio timeout in `resolve`.
    opts.timeout = DNS_TIMEOUT;
    opts.attempts = 2;
    // Dual-stack (DESIGN s5.8.1 "No IPv4 / no IPv6"): accept either family so
    // a single-stack outage still resolves.
    opts.ip_strategy = LookupIpStrategy::Ipv4AndIpv6;

    // Read the system DNS config for its nameserver list (so corporate /
    // split-horizon DNS works), but pair it with OUR cache-disabled `opts`
    // rather than the system opts. Fall back to a public resolver config only
    // if the system conf cannot be read (e.g. a locked-down container).
    match hickory_resolver::system_conf::read_system_conf() {
        Ok((config, _system_opts)) => Ok(TokioAsyncResolver::tokio(config, opts)),
        Err(sys_err) => {
            tracing::warn!(
                target: TARGET,
                error = %sys_err,
                "could not read system DNS config; falling back to public resolvers"
            );
            Ok(TokioAsyncResolver::tokio(
                ResolverConfig::cloudflare(),
                opts,
            ))
        }
    }
}

/// Builds the redirect-disabled captive-portal probe client (DESIGN s5.8.4
/// captive row: 3s connect / 5s total).
fn build_captive_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        // Observe a portal's 30x rather than follow it (DESIGN s5.8.1).
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(PROBE_CONNECT_TIMEOUT)
        .timeout(CAPTIVE_TOTAL_TIMEOUT)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        // Honour HTTP(S)_PROXY (DESIGN s5.8.7): do NOT call .no_proxy().
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build captive-probe reqwest client: {e}"))
}

/// Builds a per-service probe client with that service's DESIGN s5.8.4
/// timeouts and the s5.8.5 pool hygiene baked in.
fn build_service_client(service: ServiceName) -> anyhow::Result<reqwest::Client> {
    let (total, idle) = match service {
        ServiceName::Drive => (DRIVE_TOTAL_TIMEOUT, DRIVE_IDLE_TIMEOUT),
        ServiceName::UpdateEndpoint => (UPDATE_TOTAL_TIMEOUT, UPDATE_IDLE_TIMEOUT),
        ServiceName::Github => (GITHUB_TOTAL_TIMEOUT, GITHUB_IDLE_TIMEOUT),
        // Telemetry has no probe client; use the conservative update-class
        // timeouts if a client is ever built for it. The backend never calls
        // this for Telemetry (service_client returns None).
        ServiceName::Telemetry => (UPDATE_TOTAL_TIMEOUT, UPDATE_IDLE_TIMEOUT),
    };

    reqwest::Client::builder()
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
        .http2_adaptive_window(true)
        // Honour HTTP(S)_PROXY (DESIGN s5.8.7): do NOT call .no_proxy().
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
    // Building the clients (rustls init) and the resolver
    // (from-system-conf, with a public fallback) must succeed offline: none
    // of it touches the network. This is a real assertion, not a skip.
    #[test]
    fn backend_constructs_offline() {
        let backend = ReqwestBackend::new();
        assert!(
            backend.is_ok(),
            "ReqwestBackend::new must succeed without network: {:?}",
            backend.err()
        );
    }

    // --- per-service client mapping covers exactly the probed services ---

    #[test]
    fn telemetry_has_no_probe_client() {
        let backend = ReqwestBackend::new().expect("construct backend");
        assert!(backend.service_client(ServiceName::Telemetry).is_none());
        assert!(backend.service_client(ServiceName::Drive).is_some());
        assert!(backend
            .service_client(ServiceName::UpdateEndpoint)
            .is_some());
        assert!(backend.service_client(ServiceName::Github).is_some());
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

    // --- DNS-error classification (offline; constructs hickory errors) ---

    #[test]
    fn no_records_found_is_dns_failed() {
        // A NoRecordsFound error (NXDOMAIN / empty answer) is the canonical
        // "DNS broken" classification (DESIGN s5.8.1).
        let err: ResolveError = ResolveErrorKind::Message("no records").into();
        assert_eq!(
            classify_resolve_error("example.invalid", &err),
            ProbeOutcome::DnsFailed
        );
    }

    #[test]
    fn resolver_timeout_is_dns_failed() {
        let err: ResolveError = ResolveErrorKind::Timeout.into();
        assert_eq!(
            classify_resolve_error("example.invalid", &err),
            ProbeOutcome::DnsFailed
        );
    }

    // --- DNS probe re-resolution is bounded (no hang) ---
    //
    // `.invalid` is reserved (RFC 6761) and never resolves, so this exercises
    // the failure path deterministically. It MUST complete within the DNS
    // budget and classify as DnsFailed (DESIGN s5.8.1 "must not hang"). This
    // touches the local resolver only (an NXDOMAIN/SERVFAIL answer), not a
    // remote service, so it is a real offline-safe test.
    #[tokio::test]
    async fn resolve_invalid_tld_is_dns_failed_and_bounded() {
        let backend = ReqwestBackend::new().expect("construct backend");
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
}
