//! Native per-OS reachability read for [`Backend::os_online`](driven_core::network::Backend)
//! (DESIGN s5.8.2 probe 1), with automatic fallback to the generic TCP probe.
//!
//! DESIGN s5.8.2 names the cheapest first probe as the OS connectivity API:
//! `INetworkListManager` (Windows), NetworkManager's `Connectivity` enum
//! (Linux), `NWPathMonitor` (macOS). This module owns those OS-specific reads
//! plus the pure decision logic that turns each OS's raw answer into the
//! tri-state [`Reachability`] the backend acts on. The generic anycast-DNS TCP
//! probe in [`crate`] stays as the transparent fallback when the native read is
//! unavailable or inconclusive.
//!
//! ## Selection + fallback
//!
//! [`ReqwestBackend::os_online`](crate::ReqwestBackend) consults
//! [`detect_reachability`] FIRST. Its verdict is resolved by the pure
//! [`resolve_native`]:
//!
//! - [`Reachability::Online`] -> `os_online` returns `true` (proceed to the
//!   captive + service probes, which classify Online vs NoInternet vs
//!   CaptivePortal).
//! - [`Reachability::Offline`] -> `os_online` returns `false` (short-circuit to
//!   [`NetworkState::Offline`](driven_core::network::NetworkState): airplane
//!   mode / rfkill / no interface, DESIGN s5.8.1).
//! - [`Reachability::Unknown`] -> the native read was unavailable or ambiguous;
//!   `os_online` transparently falls back to the generic TCP probe.
//!
//! ## Never-Offline-on-connected direction
//!
//! Only a CONFIDENT no-active-connection verdict maps to `Offline`; any active
//! connection - including link-local-only, limited, or captive-portal states -
//! maps to `Online` so the downstream HTTP probes (DESIGN s5.8.2 probes 2-3)
//! do the authoritative classification. Collapsing a connected-but-portal or
//! connected-but-no-Internet link to `Offline` would hide the CaptivePortal
//! state (which drives the "Sign in to network" tray action) and the NoInternet
//! state (30s re-probe) behind Offline's "re-probe only on interface-up" path.
//! Every ambiguity (API error, resolver-unknown, not-yet-delivered path)
//! collapses to [`Reachability::Unknown`] -> TCP fallback, never a wrong
//! `Offline`.
//!
//! ## Testability
//!
//! The OS reads themselves (COM / D-Bus / `NWPathMonitor`) can only run on
//! their native platform, but the classification that turns each OS's raw value
//! into a [`Reachability`] is pure. The three `classify_*` functions and
//! [`resolve_native`] are compiled and unit-tested on EVERY platform
//! (`#[cfg(any(test, target_os = ...))]`), so the selection/fallback decision is
//! covered by CI on Windows, macOS, and Linux alike; only the thin OS-call
//! adapters are compile-checked-only off their native OS.

#[cfg(any(target_os = "windows", target_os = "linux"))]
use std::time::Duration;

/// Tracing target for the native reachability read.
#[cfg(any(target_os = "windows", target_os = "linux"))]
const TARGET: &str = "driven::net::reachability";

/// Budget for a native reachability read dispatched onto a blocking thread
/// (Windows COM / Linux D-Bus). Matches the DESIGN s5.8.4 3s OS-probe budget so
/// a wedged bus or COM stall can never exceed it; on timeout we resolve to
/// [`Reachability::Unknown`] and let the TCP fallback (itself 3s-bounded) run.
#[cfg(any(target_os = "windows", target_os = "linux"))]
const NATIVE_READ_TIMEOUT: Duration = Duration::from_secs(3);

/// The native OS connectivity verdict (DESIGN s5.8.2 probe 1).
///
/// Tri-state because the OS APIs do not all answer a crisp online/offline: the
/// read can fail outright (COM error, no NetworkManager), NetworkManager reports
/// an explicit "unknown", and a freshly-started `NWPathMonitor` has not
/// delivered its first path yet. [`Reachability::Unknown`] captures all of
/// those and routes to the TCP fallback (see module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Reachability {
    /// The OS reports an active connection (internet, or link-local / limited /
    /// captive-portal). Proceed to the captive + service probes.
    Online = 0,
    /// The OS confidently reports no active connection (airplane mode / rfkill
    /// / no interface). Short-circuit the topology to Offline.
    Offline = 1,
    /// The native read was unavailable or ambiguous; fall back to the TCP
    /// probe. The safe default: never a wrong Offline.
    Unknown = 2,
}

impl Reachability {
    /// Decodes a byte previously produced by `self as u8`. Any unrecognized
    /// value decodes to [`Reachability::Unknown`] (the safe default). Used by
    /// the macOS monitor, which caches the latest verdict in an `AtomicU8`.
    #[cfg(any(test, target_os = "macos"))]
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Reachability::Online,
            1 => Reachability::Offline,
            _ => Reachability::Unknown,
        }
    }
}

/// Resolves a native [`Reachability`] verdict into the `os_online` decision:
///
/// - `Some(true)`  - the OS reports connectivity: `os_online` returns `true`.
/// - `Some(false)` - the OS confidently reports no connectivity: `os_online`
///   returns `false` (short-circuit to Offline).
/// - `None`        - inconclusive: `os_online` falls back to the TCP probe.
///
/// Pure, so the selection/fallback decision is unit-tested on every OS.
pub(crate) fn resolve_native(reachability: Reachability) -> Option<bool> {
    match reachability {
        Reachability::Online => Some(true),
        Reachability::Offline => Some(false),
        Reachability::Unknown => None,
    }
}

/// Classifies a Windows `NLM_CONNECTIVITY` bitmask into a [`Reachability`].
///
/// The flag values are the stable Win32 ABI constants from the
/// `NLM_CONNECTIVITY` enumeration (`netlistmgr.h`): `DISCONNECTED` = 0,
/// `IPV4_NOTRAFFIC` = 0x1, `IPV6_NOTRAFFIC` = 0x2, `IPV4_SUBNET` = 0x10,
/// `IPV4_LOCALNETWORK` = 0x20, `IPV4_INTERNET` = 0x40, `IPV6_SUBNET` = 0x100,
/// `IPV6_LOCALNETWORK` = 0x200, `IPV6_INTERNET` = 0x400.
///
/// An all-zero mask (`DISCONNECTED`) is the airplane-mode / no-interface state
/// -> `Offline`. ANY set bit - including a lone local-network / no-traffic bit
/// with no internet bit (which is exactly what Windows NCSI reports for a
/// machine behind a captive portal, or on a link-local-only network) - is an
/// active connection -> `Online`, so the downstream captive + service probes
/// classify CaptivePortal vs NoInternet vs Online. A successful `GetConnectivity`
/// always yields a defined verdict, so this never returns `Unknown` (only a COM
/// failure does).
///
/// Pure and platform-independent so it is unit-tested on every OS.
#[cfg(any(test, target_os = "windows"))]
pub(crate) fn classify_nlm_connectivity(connectivity: i32) -> Reachability {
    /// `NLM_CONNECTIVITY_DISCONNECTED`: no bits set == no active connection.
    const DISCONNECTED: i32 = 0;
    if connectivity == DISCONNECTED {
        Reachability::Offline
    } else {
        // Any connectivity bit (internet, local-network, subnet, or no-traffic,
        // on IPv4 or IPv6) means an active connection; defer the online-vs-
        // portal-vs-no-internet call to the HTTP probes.
        Reachability::Online
    }
}

/// Classifies a NetworkManager `NMConnectivityState` value into a
/// [`Reachability`].
///
/// `NMConnectivityState` (`NetworkManager.h`): 0 = `UNKNOWN`, 1 = `NONE`,
/// 2 = `PORTAL`, 3 = `LIMITED`, 4 = `FULL`.
///
/// - `FULL` / `LIMITED` / `PORTAL` are all active connections -> `Online` (the
///   captive probe then distinguishes a real portal, and the service probes a
///   limited/no-Internet link).
/// - `NONE` is no connectivity -> `Offline`.
/// - `UNKNOWN` (also what NM reports when its connectivity checking is disabled)
///   and any unexpected value -> `Unknown` -> TCP fallback.
///
/// Pure and platform-independent so it is unit-tested on every OS.
#[cfg(any(test, target_os = "linux"))]
pub(crate) fn classify_nm_connectivity(state: u32) -> Reachability {
    match state {
        2..=4 => Reachability::Online, // PORTAL / LIMITED / FULL
        1 => Reachability::Offline,    // NONE
        _ => Reachability::Unknown,    // 0 UNKNOWN, or anything unexpected
    }
}

/// Classifies an `nw_path_status_t` value into a [`Reachability`].
///
/// `nw_path_status_t` (`Network/path.h`): 0 = `invalid`, 1 = `satisfied`,
/// 2 = `unsatisfied`, 3 = `satisfiable`.
///
/// - `satisfied` - the path is usable -> `Online`.
/// - `unsatisfied` - the path is NOT usable (no interface) -> `Offline`.
/// - `invalid` / `satisfiable` (usable only after establishing a connection,
///   e.g. VPN-on-demand) and any unexpected value -> `Unknown` -> TCP fallback.
///
/// Pure and platform-independent so it is unit-tested on every OS.
#[cfg(any(test, target_os = "macos"))]
pub(crate) fn classify_nw_path_status(status: i32) -> Reachability {
    match status {
        1 => Reachability::Online,  // nw_path_status_satisfied
        2 => Reachability::Offline, // nw_path_status_unsatisfied
        _ => Reachability::Unknown, // 0 invalid, 3 satisfiable, or unexpected
    }
}

// --- Blocking-read dispatch (Windows COM + Linux D-Bus) ----------------------

/// Runs a blocking native reachability read off the async executor, bounded by
/// [`NATIVE_READ_TIMEOUT`]. The COM / D-Bus read runs on a
/// [`tokio::task::spawn_blocking`] thread (a COM apartment call and zbus's
/// blocking API must not be driven from within the async runtime); a join error
/// or timeout resolves to [`Reachability::Unknown`] -> TCP fallback, so a wedged
/// bus never parks the probe path.
#[cfg(any(target_os = "windows", target_os = "linux"))]
async fn run_blocking_read(read: fn() -> Reachability) -> Reachability {
    let task = tokio::task::spawn_blocking(read);
    match tokio::time::timeout(NATIVE_READ_TIMEOUT, task).await {
        Ok(Ok(reachability)) => reachability,
        Ok(Err(join_err)) => {
            tracing::debug!(target: TARGET, error = %join_err, "native reachability read task failed; treating as unknown");
            Reachability::Unknown
        }
        Err(_elapsed) => {
            tracing::debug!(target: TARGET, "native reachability read timed out; treating as unknown");
            Reachability::Unknown
        }
    }
}

// --- Windows: INetworkListManager::GetConnectivity ---------------------------

/// Reads the machine-wide connectivity via COM and classifies it.
///
/// Instantiates the `NetworkListManager` COM object and reads `GetConnectivity`,
/// handing the raw `NLM_CONNECTIVITY` bitmask to [`classify_nlm_connectivity`].
///
/// Safety + robustness: the COM calls are `unsafe`, but every failure path
/// (apartment init refused, object unavailable, `GetConnectivity` error)
/// collapses to [`Reachability::Unknown`] -> TCP fallback, never a guessed
/// `Offline`. COM is initialised per call as multi-threaded-apartment and
/// balanced with `CoUninitialize` on scope exit via an RAII guard (mirroring
/// `driven-power`'s metered read); an `RPC_E_CHANGED_MODE` (apartment already
/// initialised differently on this thread) is tolerated - the COM call still
/// works against the existing apartment - and is NOT balanced (we did not
/// perform that init).
#[cfg(target_os = "windows")]
fn read_connectivity_blocking() -> Reachability {
    use windows::core::HRESULT;
    use windows::Win32::Networking::NetworkListManager::{INetworkListManager, NetworkListManager};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    // RPC_E_CHANGED_MODE: COM already initialised on this thread with a
    // different apartment model. Not fatal - the existing apartment serves our
    // in-proc call fine, so we proceed without treating it as an error.
    const RPC_E_CHANGED_MODE: HRESULT = HRESULT(0x8001_0106u32 as i32);

    /// RAII guard that balances a SUCCESSFUL `CoInitializeEx` with exactly one
    /// `CoUninitialize` on drop. A `RPC_E_CHANGED_MODE` is a FAILURE (the thread
    /// was already initialised with a different model) and must NOT be balanced,
    /// so the guard is only armed when we performed a successful init.
    struct ComInitGuard {
        should_uninit: bool,
    }
    impl Drop for ComInitGuard {
        fn drop(&mut self) {
            if self.should_uninit {
                // SAFETY: balanced against our own successful CoInitializeEx on
                // this same thread (the guard is never moved across threads).
                unsafe { CoUninitialize() };
            }
        }
    }

    // SAFETY: standard COM init -> create-instance -> call sequence. The
    // `INetworkListManager` is owned by the `windows` smart wrapper (refcounted),
    // every fallible step short-circuits to `Unknown`, and the ComInitGuard
    // balances a successful init on every return path (drop runs even on the
    // early returns below).
    unsafe {
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        // `is_ok()` covers both S_OK and S_FALSE (>= 0). Those owe an uninit.
        // RPC_E_CHANGED_MODE: proceed but do NOT uninit (we did not init). Any
        // other hard failure aborts (and owes no uninit).
        if init.is_err() && init != RPC_E_CHANGED_MODE {
            return Reachability::Unknown;
        }
        let _com_guard = ComInitGuard {
            should_uninit: init.is_ok(),
        };

        let manager: windows::core::Result<INetworkListManager> =
            CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL);
        let manager = match manager {
            Ok(m) => m,
            Err(_) => return Reachability::Unknown,
        };

        match manager.GetConnectivity() {
            Ok(connectivity) => classify_nlm_connectivity(connectivity.0),
            Err(_) => Reachability::Unknown,
        }
    }
}

// --- Linux: NetworkManager Connectivity property over D-Bus ------------------

/// The NetworkManager proxy trait. `#[proxy]` also emits an async proxy we do
/// not use here, hence the module-level `allow(dead_code)`.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
mod nm {
    use zbus::proxy;

    #[proxy(
        interface = "org.freedesktop.NetworkManager",
        default_service = "org.freedesktop.NetworkManager",
        default_path = "/org/freedesktop/NetworkManager"
    )]
    pub(crate) trait NetworkManager {
        /// The aggregate connectivity state (the `NMConnectivityState` enum).
        #[zbus(property)]
        fn connectivity(&self) -> zbus::Result<u32>;
    }
}

/// Blocking read of NetworkManager's aggregate `Connectivity` property,
/// classified.
///
/// MUST be called off the async executor - it opens a system-bus connection and
/// blocks on a D-Bus round-trip - so [`run_blocking_read`] dispatches it via
/// [`tokio::task::spawn_blocking`] (zbus explicitly warns that its blocking API
/// panics/hangs if driven from inside an async runtime). Any failure (no system
/// bus, NetworkManager not running, property absent on an old NM) resolves to
/// [`Reachability::Unknown`] -> TCP fallback.
#[cfg(target_os = "linux")]
fn read_connectivity_blocking() -> Reachability {
    match read_nm_connectivity() {
        Ok(state) => classify_nm_connectivity(state),
        Err(err) => {
            tracing::debug!(
                target: TARGET,
                error = %err,
                "NetworkManager Connectivity read failed; treating as unknown (TCP fallback)"
            );
            Reachability::Unknown
        }
    }
}

#[cfg(target_os = "linux")]
fn read_nm_connectivity() -> zbus::Result<u32> {
    let connection = zbus::blocking::Connection::system()?;
    let proxy = nm::NetworkManagerProxyBlocking::new(&connection)?;
    proxy.connectivity()
}

// --- macOS: NWPathMonitor (nw_path_get_status) -------------------------------

/// Live `NWPathMonitor` whose most recent path-status verdict is cached in an
/// atomic.
///
/// `NWPath` has no synchronous getter - it is only observable through a running
/// monitor's update handler (DESIGN s5.8.2 names `NWPathMonitor`). We start ONE
/// process-lifetime monitor lazily and cache `classify_nw_path_status` in an
/// `AtomicU8`; [`MacosReachabilityMonitor::status`] reads it cheaply on each
/// probe. The first path arrives asynchronously, so the very first read before
/// it is delivered returns `Unknown` -> TCP fallback (no cold-start false
/// Offline). The monitor is intentionally leaked: there is exactly one per app
/// and it must run for the whole process.
#[cfg(target_os = "macos")]
mod macos_nw {
    use super::{classify_nw_path_status, Reachability};
    use block2::RcBlock;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::{Arc, OnceLock};

    // Network.framework's nw_* objects are opaque, reference-counted pointers.
    // We treat every one as `*mut c_void` (Encode-compatible for the block
    // signature) and never dereference them ourselves - only hand them back to
    // the framework's own C accessors.
    type NwPathMonitorT = *mut c_void;
    type NwPathT = *mut c_void;
    type DispatchQueueT = *mut c_void;

    #[link(name = "Network", kind = "framework")]
    extern "C" {
        fn nw_path_monitor_create() -> NwPathMonitorT;
        fn nw_path_monitor_set_queue(monitor: NwPathMonitorT, queue: DispatchQueueT);
        fn nw_path_monitor_set_update_handler(
            monitor: NwPathMonitorT,
            update_handler: &block2::Block<dyn Fn(NwPathT)>,
        );
        fn nw_path_monitor_start(monitor: NwPathMonitorT);
        fn nw_path_get_status(path: NwPathT) -> i32;
    }

    extern "C" {
        // libdispatch, part of libSystem (always linked on macOS). The global
        // concurrent queues are process-owned - never created or released.
        fn dispatch_get_global_queue(identifier: isize, flags: usize) -> DispatchQueueT;
    }

    /// Cheap, cloneable handle over the monitor's cached verdict. Cloning only
    /// clones the `Arc<AtomicU8>`.
    #[derive(Clone)]
    pub(crate) struct MacosReachabilityMonitor {
        status: Arc<AtomicU8>,
    }

    impl MacosReachabilityMonitor {
        /// Creates and starts the `NWPathMonitor`. Best-effort: if the monitor
        /// cannot be created the handle still works, permanently reporting
        /// `Unknown` (TCP fallback).
        fn start() -> Self {
            let status = Arc::new(AtomicU8::new(Reachability::Unknown as u8));
            let cell = Arc::clone(&status);

            let handler = RcBlock::new(move |path: NwPathT| {
                let next = if path.is_null() {
                    Reachability::Unknown
                } else {
                    // SAFETY: `path` is the framework-owned `nw_path_t` passed to
                    // the update handler; `nw_path_get_status` is a total accessor
                    // that neither mutates nor frees it.
                    unsafe { classify_nw_path_status(nw_path_get_status(path)) }
                };
                cell.store(next as u8, Ordering::Relaxed);
            });

            // SAFETY: the standard NWPathMonitor create -> set handler -> set
            // queue -> start sequence. `set_update_handler` `Block_copy`s the
            // handler, so dropping our `RcBlock` when this scope ends is sound.
            // The monitor is intentionally leaked (process-lifetime singleton);
            // its captured `Arc<AtomicU8>` keeps the cache alive independently of
            // this handle.
            unsafe {
                let monitor = nw_path_monitor_create();
                if !monitor.is_null() {
                    nw_path_monitor_set_update_handler(monitor, &handler);
                    nw_path_monitor_set_queue(monitor, dispatch_get_global_queue(0, 0));
                    nw_path_monitor_start(monitor);
                }
            }

            Self { status }
        }

        /// Reads the most recent cached verdict. Cheap (a relaxed atomic load).
        pub(crate) fn status(&self) -> Reachability {
            Reachability::from_u8(self.status.load(Ordering::Relaxed))
        }
    }

    /// The process-lifetime reachability monitor, started on first read.
    static MONITOR: OnceLock<MacosReachabilityMonitor> = OnceLock::new();

    /// Returns the shared, lazily-started reachability monitor.
    pub(crate) fn reachability_monitor() -> &'static MacosReachabilityMonitor {
        MONITOR.get_or_init(MacosReachabilityMonitor::start)
    }
}

// --- Cross-OS dispatch -------------------------------------------------------

/// Reads the native OS reachability verdict (DESIGN s5.8.2 probe 1).
///
/// Per-OS behind a `cfg` (exactly one body is compiled per target); every path
/// returns a [`Reachability`] the backend resolves via [`resolve_native`].
/// Targets without a native backend return [`Reachability::Unknown`] so the
/// backend always falls back to the TCP probe there.
#[cfg(target_os = "windows")]
pub(crate) async fn detect_reachability() -> Reachability {
    run_blocking_read(read_connectivity_blocking).await
}

#[cfg(target_os = "linux")]
pub(crate) async fn detect_reachability() -> Reachability {
    run_blocking_read(read_connectivity_blocking).await
}

#[cfg(target_os = "macos")]
pub(crate) async fn detect_reachability() -> Reachability {
    // A cheap relaxed atomic load off the process-lifetime NWPathMonitor; no
    // blocking work, so no spawn_blocking needed.
    macos_nw::reachability_monitor().status()
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub(crate) async fn detect_reachability() -> Reachability {
    Reachability::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the selection/fallback decision (pure, tested on every OS) ---

    #[test]
    fn resolve_native_maps_each_verdict() {
        assert_eq!(resolve_native(Reachability::Online), Some(true));
        assert_eq!(resolve_native(Reachability::Offline), Some(false));
        // Unknown is the fallback signal: os_online drops to the TCP probe.
        assert_eq!(resolve_native(Reachability::Unknown), None);
    }

    #[test]
    fn from_u8_round_trips_and_defaults_unknown() {
        assert_eq!(
            Reachability::from_u8(Reachability::Online as u8),
            Reachability::Online
        );
        assert_eq!(
            Reachability::from_u8(Reachability::Offline as u8),
            Reachability::Offline
        );
        assert_eq!(
            Reachability::from_u8(Reachability::Unknown as u8),
            Reachability::Unknown
        );
        // Any out-of-range byte decodes to the safe default.
        assert_eq!(Reachability::from_u8(7), Reachability::Unknown);
        assert_eq!(Reachability::from_u8(255), Reachability::Unknown);
    }

    // --- Windows NLM_CONNECTIVITY classification ---

    #[test]
    fn nlm_connectivity_classification() {
        // DISCONNECTED (no bits) -> Offline (airplane mode / no interface).
        assert_eq!(classify_nlm_connectivity(0x0), Reachability::Offline);
        // IPv4 / IPv6 internet -> Online.
        assert_eq!(classify_nlm_connectivity(0x40), Reachability::Online); // IPV4_INTERNET
        assert_eq!(classify_nlm_connectivity(0x400), Reachability::Online); // IPV6_INTERNET
                                                                            // Local-network / subnet / no-traffic with NO internet bit is still an
                                                                            // active connection (captive-portal / link-local case) -> Online, NOT
                                                                            // Offline. This is the regression guard: NCSI reports a portal like this.
        assert_eq!(classify_nlm_connectivity(0x20), Reachability::Online); // IPV4_LOCALNETWORK
        assert_eq!(classify_nlm_connectivity(0x10), Reachability::Online); // IPV4_SUBNET
        assert_eq!(classify_nlm_connectivity(0x1), Reachability::Online); // IPV4_NOTRAFFIC
        assert_eq!(classify_nlm_connectivity(0x200), Reachability::Online); // IPV6_LOCALNETWORK
                                                                            // Combined internet + local bits -> Online.
        assert_eq!(
            classify_nlm_connectivity(0x40 | 0x400),
            Reachability::Online
        );
    }

    // --- Linux NMConnectivityState classification ---

    #[test]
    fn nm_connectivity_classification() {
        assert_eq!(classify_nm_connectivity(0), Reachability::Unknown); // UNKNOWN -> fallback
        assert_eq!(classify_nm_connectivity(1), Reachability::Offline); // NONE
                                                                        // PORTAL and LIMITED are active connections: let the HTTP probes classify
                                                                        // the portal / no-Internet link rather than short-circuiting to Offline.
        assert_eq!(classify_nm_connectivity(2), Reachability::Online); // PORTAL
        assert_eq!(classify_nm_connectivity(3), Reachability::Online); // LIMITED
        assert_eq!(classify_nm_connectivity(4), Reachability::Online); // FULL
                                                                       // Defensive: an unexpected value is Unknown, not Offline.
        assert_eq!(classify_nm_connectivity(99), Reachability::Unknown);
    }

    // --- macOS nw_path_status classification ---

    #[test]
    fn nw_path_status_classification() {
        assert_eq!(classify_nw_path_status(0), Reachability::Unknown); // invalid -> fallback
        assert_eq!(classify_nw_path_status(1), Reachability::Online); // satisfied
        assert_eq!(classify_nw_path_status(2), Reachability::Offline); // unsatisfied
                                                                       // satisfiable (usable only after connecting, e.g. VPN-on-demand) is not
                                                                       // an active path -> Unknown -> fallback, never a wrong Offline.
        assert_eq!(classify_nw_path_status(3), Reachability::Unknown); // satisfiable
        assert_eq!(classify_nw_path_status(9), Reachability::Unknown); // unexpected
    }

    // --- Per-OS native-read smoke tests (compile everywhere, run natively) ---

    /// The Windows real `INetworkListManager::GetConnectivity` COM read must
    /// never panic and must return a plain [`Reachability`], regardless of the
    /// host's current connectivity. Runs on the Windows CI runner (and locally);
    /// the value is environment-dependent (a real connected host yields
    /// `Online`), so we only assert the call is total.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_read_connectivity_is_total() {
        let _: Reachability = read_connectivity_blocking();
    }

    /// The Linux NetworkManager `Connectivity` D-Bus read must never panic and
    /// must return a plain [`Reachability`] (`Unknown` when NetworkManager or the
    /// system bus is absent). Runs on the Linux CI runner; the value is
    /// environment-dependent, so we only assert totality. A plain `#[test]` (no
    /// tokio runtime) so zbus's blocking API is not driven from an async runtime.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_read_connectivity_is_total() {
        let _: Reachability = read_connectivity_blocking();
    }

    /// Smoke test for the macOS `NWPathMonitor` FFI: starting the process-
    /// lifetime monitor and reading its cached status must not panic or crash.
    /// The first path arrives asynchronously, so the status is normally
    /// `Unknown` at this instant - we only assert the calls are total. This is
    /// the only runtime exercise of the Network.framework / `block2` /
    /// libdispatch bindings (a wrong signature or link would fail here on the
    /// macOS CI runner).
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reachability_monitor_is_total() {
        let _: Reachability = macos_nw::reachability_monitor().status();
        // A second read is likewise total (and the handle clones cheaply).
        let _: Reachability = macos_nw::reachability_monitor().clone().status();
    }
}
