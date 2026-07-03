//! Metered-network detection + reachability hint for [`PowerState`]
//! (DESIGN s5.7).
//!
//! The per-OS [`PowerSource`](crate::PowerSource) backends fold the active
//! connection's metered state into every [`PowerState`](crate::PowerState) they
//! broadcast. This module owns the OS-specific reads and the pure decision
//! logic that turns each OS's raw answer into the `on_metered_network` bool the
//! orchestrator gates on (DESIGN s5.7 `skip_on_metered`).
//!
//! ## Reachability scope boundary
//!
//! `network_reachable` here is only the *coarse* hint embedded in
//! [`PowerState`]. The authoritative reachability classification - airplane
//! mode vs captive portal vs DNS-broken vs Drive-down - is the
//! network-resilience subsystem's job (DESIGN s5.8, three-probe topology), not
//! this module's. We default it to `true` ([`reachable_hint`]) and let that
//! subsystem flip the orchestrator's gate; this hint exists so a `PowerState`
//! snapshot is self-contained for the tray / activity-log consumers that only
//! need a rough "are we online" flag.
//!
//! ## Metered detection (all three desktop OSes are now real)
//!
//! Per DESIGN s5.7 the source is per-OS, and each is now a real read:
//!
//! - **Windows:** `INetworkListManager` -> `INetworkCostManager::GetCost`.
//!   The returned `NLM_CONNECTION_COST` bitmask is interpreted by
//!   [`classify_windows_cost`].
//! - **Linux:** NetworkManager's aggregate `Metered` property
//!   (`org.freedesktop.NetworkManager`) read over D-Bus, interpreted by
//!   [`classify_nm_metered`]. When NetworkManager (or the system bus) is absent
//!   the read fails and resolves to `Unknown` -> not metered.
//! - **macOS:** there is no literal "metered" bit. A long-lived `NWPathMonitor`
//!   reports `nw_path_is_expensive` (cellular / personal hotspot) and
//!   `nw_path_is_constrained` (Low Data Mode); those are the documented proxies,
//!   interpreted by [`classify_nw_path`].
//!
//! ## Safe-default direction
//!
//! Every OS read collapses ambiguity to [`MeteredStatus::Unknown`], and
//! [`MeteredStatus::on_metered`] maps `Unknown` to `false` (not metered). The
//! direction is deliberate: the orchestrator honours `skip_on_metered` only
//! when this returns `true`, so a wrong "not metered" merely fails to skip a
//! (rare) metered link, whereas a wrong "metered" would wrongly stall ALL sync.
//!
//! ## Testability
//!
//! The OS reads themselves (COM / D-Bus / `NWPathMonitor`) can only run on
//! their native platform, but the decision logic that turns each OS's raw value
//! into a [`MeteredStatus`] is pure. The three `classify_*` functions and
//! [`MeteredStatus::on_metered`] are compiled and unit-tested on *every*
//! platform (`#[cfg(any(test, target_os = ...))]`), so the classification is
//! covered by CI on Windows, macOS, and Linux alike; only the thin OS-call
//! adapters are compile-checked-only off their native OS.

/// Tri-state metered signal derived from the per-OS network stack.
///
/// The OS APIs do not all answer a crisp yes/no: NetworkManager reports an
/// explicit "unknown", a D-Bus read can fail outright, and a freshly-started
/// `NWPathMonitor` has not delivered its first path yet. `Unknown` captures all
/// of those; [`MeteredStatus::on_metered`] collapses it to *not metered* - the
/// safe direction (see module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum MeteredStatus {
    /// The active connection is explicitly not metered.
    Unmetered = 0,
    /// The active connection is metered (or its OS-specific proxy is set).
    Metered = 1,
    /// The metered state could not be determined; treated as not metered.
    Unknown = 2,
}

impl MeteredStatus {
    /// Collapses the tri-state into the `PowerState::on_metered_network` bool.
    ///
    /// Only an explicit [`MeteredStatus::Metered`] gates sync; `Unmetered` and
    /// `Unknown` both map to `false` (see the module's safe-default note).
    pub(crate) fn on_metered(self) -> bool {
        matches!(self, MeteredStatus::Metered)
    }

    /// Decodes a byte previously produced by `self as u8`. Any unrecognized
    /// value decodes to [`MeteredStatus::Unknown`] (the safe default). Used by
    /// the macOS monitor, which caches the latest verdict in an `AtomicU8`.
    #[cfg(any(test, target_os = "macos"))]
    fn from_u8(value: u8) -> Self {
        match value {
            0 => MeteredStatus::Unmetered,
            1 => MeteredStatus::Metered,
            _ => MeteredStatus::Unknown,
        }
    }
}

/// Coarse reachability hint embedded in [`PowerState`]. Always `true`; DESIGN
/// s5.8 owns the authoritative classification (see module docs).
pub(crate) const fn reachable_hint() -> bool {
    true
}

/// Classifies a Windows `NLM_CONNECTION_COST` bitmask into a [`MeteredStatus`].
///
/// The flag values are the stable Win32 ABI constants from the
/// `NLM_CONNECTION_COST` enumeration (`netlistmgr.h`): `UNKNOWN` = 0,
/// `UNRESTRICTED` = 0x1, `FIXED` = 0x2, `VARIABLE` = 0x4, `OVERDATALIMIT` =
/// 0x8, `CONGESTED` = 0x10, `ROAMING` = 0x20, `APPROACHINGDATALIMIT` = 0x40.
/// Any of FIXED / VARIABLE / OVERDATALIMIT / CONGESTED / ROAMING /
/// APPROACHINGDATALIMIT means metered; a lone `UNRESTRICTED` means not metered;
/// an empty (`UNKNOWN`) mask or an all-zero-after-masking value is `Unknown`.
///
/// Pure and platform-independent so it is unit-tested on every OS (the Windows
/// adapter is the only place it is *wired*, but CI exercises it everywhere).
#[cfg(any(test, target_os = "windows"))]
pub(crate) fn classify_windows_cost(cost: u32) -> MeteredStatus {
    const UNRESTRICTED: u32 = 0x1;
    const FIXED: u32 = 0x2;
    const VARIABLE: u32 = 0x4;
    const OVERDATALIMIT: u32 = 0x8;
    const CONGESTED: u32 = 0x10;
    const ROAMING: u32 = 0x20;
    const APPROACHINGDATALIMIT: u32 = 0x40;

    const METERED: u32 =
        FIXED | VARIABLE | OVERDATALIMIT | CONGESTED | ROAMING | APPROACHINGDATALIMIT;

    if cost & METERED != 0 {
        MeteredStatus::Metered
    } else if cost & UNRESTRICTED != 0 {
        MeteredStatus::Unmetered
    } else {
        // NLM_CONNECTION_COST_UNKNOWN (0) or any value with no recognized bit.
        MeteredStatus::Unknown
    }
}

/// Classifies a NetworkManager `NMMetered` enum value into a [`MeteredStatus`].
///
/// `NMMetered` (`NetworkManager.h`): 0 = `UNKNOWN`, 1 = `YES`, 2 = `NO`,
/// 3 = `GUESS_YES`, 4 = `GUESS_NO`. A guessed-yes is still treated as metered
/// (the safe direction), a guessed-no as not metered.
///
/// Pure and platform-independent so it is unit-tested on every OS.
#[cfg(any(test, target_os = "linux"))]
pub(crate) fn classify_nm_metered(value: u32) -> MeteredStatus {
    match value {
        1 | 3 => MeteredStatus::Metered,   // YES / GUESS_YES
        2 | 4 => MeteredStatus::Unmetered, // NO / GUESS_NO
        _ => MeteredStatus::Unknown,       // 0 UNKNOWN, or anything unexpected
    }
}

/// Classifies an `NWPath`'s expensive / constrained flags into a
/// [`MeteredStatus`].
///
/// macOS exposes no literal "metered" bit. `nw_path_is_expensive` is set for
/// cellular interfaces and personal hotspots, and `nw_path_is_constrained` for
/// Low Data Mode; either one is the documented signal that the user wants data
/// conserved, so we map either to metered.
///
/// Pure and platform-independent so it is unit-tested on every OS.
#[cfg(any(test, target_os = "macos"))]
pub(crate) fn classify_nw_path(is_expensive: bool, is_constrained: bool) -> MeteredStatus {
    if is_expensive || is_constrained {
        MeteredStatus::Metered
    } else {
        MeteredStatus::Unmetered
    }
}

// --- Windows: INetworkCostManager::GetCost -----------------------------------

/// Reads the machine-wide active connection cost via COM and classifies it.
///
/// Instantiates the `NetworkListManager` COM object, casts it to
/// `INetworkCostManager`, and reads `GetCost` (NULL destination -> the
/// machine-wide active connection). The raw `NLM_CONNECTION_COST` bitmask is
/// handed to [`classify_windows_cost`].
///
/// Safety + robustness: the COM calls are `unsafe`, but every failure path
/// (apartment init refused, object/interface unavailable, `GetCost` error)
/// collapses to [`MeteredStatus::Unknown`] rather than guessing "metered" - a
/// wrong "metered" would stall ALL sync, a wrong "not metered" only fails to
/// skip a rare metered link. COM is initialised per call as
/// multi-threaded-apartment and balanced with `CoUninitialize` on scope exit
/// via an RAII guard (codex C-P2-6 - the prior code leaked an apartment
/// refcount on every 30s poll); an `RPC_E_CHANGED_MODE` (apartment already
/// initialised differently on this thread by the host process) is tolerated -
/// the COM call still works against the existing apartment - and is NOT
/// balanced (we did not perform that init).
#[cfg(target_os = "windows")]
pub(crate) fn detect_metered() -> MeteredStatus {
    use windows::core::HRESULT;
    use windows::Win32::Networking::NetworkListManager::{INetworkCostManager, NetworkListManager};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    // RPC_E_CHANGED_MODE: COM already initialised on this thread with a
    // different apartment model. Not fatal - the existing apartment serves our
    // in-proc call fine, so we proceed without treating it as an error.
    const RPC_E_CHANGED_MODE: HRESULT = HRESULT(0x8001_0106u32 as i32);

    /// RAII guard that balances a SUCCESSFUL `CoInitializeEx` with exactly one
    /// `CoUninitialize` on drop (codex C-P2-6: the previous code never
    /// uninitialised, leaking a COM apartment refcount on every 30s poll).
    ///
    /// COM rule: every `CoInitializeEx` that returns success - `S_OK` OR
    /// `S_FALSE` ("already initialised on this thread", still a success that
    /// increments the per-thread refcount) - MUST be balanced by one
    /// `CoUninitialize`. A `RPC_E_CHANGED_MODE` is a FAILURE (the thread was
    /// already initialised with a different model) and must NOT be balanced, so
    /// the guard is only armed when we performed a successful init.
    struct ComInitGuard {
        // `true` only when our CoInitializeEx returned success and therefore
        // owes a CoUninitialize.
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

    // SAFETY: standard COM init -> create-instance -> query-facet -> call
    // sequence. Each pointer is owned by the `windows` smart wrappers
    // (refcounted), every fallible step short-circuits to `Unknown`, and the
    // ComInitGuard balances a successful init on every return path (drop runs
    // even on the early returns below).
    unsafe {
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        // `is_ok()` covers both S_OK and S_FALSE (>= 0). Those owe an uninit.
        // RPC_E_CHANGED_MODE: proceed but do NOT uninit (we did not init). Any
        // other hard failure aborts (and owes no uninit).
        if init.is_err() && init != RPC_E_CHANGED_MODE {
            return MeteredStatus::Unknown;
        }
        let _com_guard = ComInitGuard {
            should_uninit: init.is_ok(),
        };

        let cost_manager: windows::core::Result<INetworkCostManager> =
            CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL);
        let cost_manager = match cost_manager {
            Ok(cm) => cm,
            Err(_) => return MeteredStatus::Unknown,
        };

        let mut cost: u32 = 0;
        // NULL destination address -> the machine-wide active connection cost.
        if cost_manager.GetCost(&mut cost, std::ptr::null()).is_err() {
            return MeteredStatus::Unknown;
        }

        classify_windows_cost(cost)
    }
}

// --- Linux: NetworkManager Metered property over D-Bus -----------------------

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
        /// The aggregate `Metered` state of the primary connection (the
        /// `NMMetered` enum). Added in NetworkManager 1.0.6.
        #[zbus(property)]
        fn metered(&self) -> zbus::Result<u32>;
    }
}

/// Blocking read of NetworkManager's aggregate `Metered` property, classified.
///
/// MUST be called off the async executor - it opens a system-bus connection and
/// blocks on a D-Bus round-trip - so the poll loop dispatches it via
/// [`tokio::task::spawn_blocking`] (zbus explicitly warns that its blocking API
/// panics/hangs if driven from inside an async runtime). Any failure (no system
/// bus, NetworkManager not running, property absent on an old NM) resolves to
/// [`MeteredStatus::Unknown`] -> not metered.
#[cfg(target_os = "linux")]
pub(crate) fn detect_metered_blocking() -> MeteredStatus {
    match read_nm_metered() {
        Ok(value) => classify_nm_metered(value),
        Err(err) => {
            tracing::debug!(
                error = %err,
                "NetworkManager Metered read failed; treating connection as unknown (not metered)"
            );
            MeteredStatus::Unknown
        }
    }
}

#[cfg(target_os = "linux")]
fn read_nm_metered() -> zbus::Result<u32> {
    let connection = zbus::blocking::Connection::system()?;
    let proxy = nm::NetworkManagerProxyBlocking::new(&connection)?;
    proxy.metered()
}

// --- macOS: NWPathMonitor (nw_path_is_expensive / _is_constrained) -----------

/// Live `NWPathMonitor` whose most recent verdict is cached in an atomic.
///
/// `NWPath` has no synchronous getter - it is only observable through a running
/// monitor's update handler (DESIGN s5.7 names `NWPath.isExpensive`). We start
/// one at construction and cache `isExpensive || isConstrained` (the documented
/// metered proxy) in an `AtomicU8`; [`MacosMeteredMonitor::status`] reads it
/// cheaply on each 30 s power poll. The monitor object is intentionally leaked:
/// there is exactly one per app and it must run for the whole process.
#[cfg(target_os = "macos")]
mod macos_nw {
    use super::{classify_nw_path, MeteredStatus};
    use block2::RcBlock;
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::Arc;

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
        fn nw_path_is_expensive(path: NwPathT) -> bool;
        fn nw_path_is_constrained(path: NwPathT) -> bool;
    }

    extern "C" {
        // libdispatch, part of libSystem (always linked on macOS). The global
        // concurrent queues are process-owned - never created or released.
        fn dispatch_get_global_queue(identifier: isize, flags: usize) -> DispatchQueueT;
    }

    /// Cheap, cloneable handle over the monitor's cached verdict. Cloning only
    /// clones the `Arc<AtomicU8>`, so the poll loop and the source can share it.
    #[derive(Clone)]
    pub(crate) struct MacosMeteredMonitor {
        status: Arc<AtomicU8>,
    }

    impl MacosMeteredMonitor {
        /// Creates and starts the `NWPathMonitor`. Best-effort: if the monitor
        /// cannot be created the handle still works, permanently reporting
        /// `Unknown` (not metered).
        pub(crate) fn start() -> Self {
            let status = Arc::new(AtomicU8::new(MeteredStatus::Unknown as u8));
            let cell = Arc::clone(&status);

            let handler = RcBlock::new(move |path: NwPathT| {
                let next = if path.is_null() {
                    MeteredStatus::Unknown
                } else {
                    // SAFETY: `path` is the framework-owned `nw_path_t` passed
                    // to the update handler; `nw_path_is_expensive` /
                    // `_is_constrained` are total accessors that neither mutate
                    // nor free it.
                    unsafe {
                        classify_nw_path(nw_path_is_expensive(path), nw_path_is_constrained(path))
                    }
                };
                cell.store(next as u8, Ordering::Relaxed);
            });

            // SAFETY: the standard NWPathMonitor create -> set handler -> set
            // queue -> start sequence. `set_update_handler` `Block_copy`s the
            // handler, so dropping our `RcBlock` when this scope ends is sound.
            // The monitor is intentionally leaked (process-lifetime singleton);
            // its captured `Arc<AtomicU8>` keeps the cache alive independently
            // of this handle.
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
        pub(crate) fn status(&self) -> MeteredStatus {
            MeteredStatus::from_u8(self.status.load(Ordering::Relaxed))
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use macos_nw::MacosMeteredMonitor;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_metered_only_true_for_metered() {
        assert!(MeteredStatus::Metered.on_metered());
        assert!(!MeteredStatus::Unmetered.on_metered());
        // Unknown is the safe default: never gates sync.
        assert!(!MeteredStatus::Unknown.on_metered());
    }

    #[test]
    fn reachable_hint_is_true() {
        assert!(reachable_hint());
    }

    #[test]
    fn from_u8_round_trips_and_defaults_unknown() {
        assert_eq!(
            MeteredStatus::from_u8(MeteredStatus::Unmetered as u8),
            MeteredStatus::Unmetered
        );
        assert_eq!(
            MeteredStatus::from_u8(MeteredStatus::Metered as u8),
            MeteredStatus::Metered
        );
        assert_eq!(
            MeteredStatus::from_u8(MeteredStatus::Unknown as u8),
            MeteredStatus::Unknown
        );
        // Any out-of-range byte decodes to the safe default.
        assert_eq!(MeteredStatus::from_u8(7), MeteredStatus::Unknown);
        assert_eq!(MeteredStatus::from_u8(255), MeteredStatus::Unknown);
    }

    #[test]
    fn windows_cost_classification() {
        // UNKNOWN (empty mask) -> Unknown.
        assert_eq!(classify_windows_cost(0x0), MeteredStatus::Unknown);
        // UNRESTRICTED alone -> not metered.
        assert_eq!(classify_windows_cost(0x1), MeteredStatus::Unmetered);
        // Each metered flag on its own -> metered.
        for flag in [0x2u32, 0x4, 0x8, 0x10, 0x20, 0x40] {
            assert_eq!(
                classify_windows_cost(flag),
                MeteredStatus::Metered,
                "flag {flag:#x} should be metered"
            );
        }
        // A metered flag wins even if UNRESTRICTED is (nonsensically) also set.
        assert_eq!(classify_windows_cost(0x1 | 0x2), MeteredStatus::Metered);
        // An unrecognized high bit with no known flag -> Unknown.
        assert_eq!(classify_windows_cost(0x8000_0000), MeteredStatus::Unknown);
    }

    #[test]
    fn nm_metered_classification() {
        assert_eq!(classify_nm_metered(0), MeteredStatus::Unknown); // NM_METERED_UNKNOWN
        assert_eq!(classify_nm_metered(1), MeteredStatus::Metered); // NM_METERED_YES
        assert_eq!(classify_nm_metered(2), MeteredStatus::Unmetered); // NM_METERED_NO
        assert_eq!(classify_nm_metered(3), MeteredStatus::Metered); // NM_METERED_GUESS_YES
        assert_eq!(classify_nm_metered(4), MeteredStatus::Unmetered); // NM_METERED_GUESS_NO
                                                                      // Defensive: an unexpected enum value is Unknown, not metered.
        assert_eq!(classify_nm_metered(99), MeteredStatus::Unknown);
    }

    #[test]
    fn nw_path_classification() {
        assert_eq!(classify_nw_path(false, false), MeteredStatus::Unmetered);
        // Expensive (cellular / hotspot) or constrained (Low Data Mode) -> metered.
        assert_eq!(classify_nw_path(true, false), MeteredStatus::Metered);
        assert_eq!(classify_nw_path(false, true), MeteredStatus::Metered);
        assert_eq!(classify_nw_path(true, true), MeteredStatus::Metered);
    }

    /// The Windows real COM read must never panic and must return a plain
    /// `MeteredStatus`, regardless of the host's current connection cost. Runs
    /// on the Windows CI runner (and locally); the value is environment-dependent
    /// (a metered link legitimately yields `Metered`), so we only assert the call
    /// is total.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_detect_metered_is_total() {
        let _: MeteredStatus = detect_metered();
    }

    /// Smoke test for the macOS `NWPathMonitor` FFI: starting the monitor and
    /// reading its cached status must not panic or crash. This is the only
    /// runtime exercise of the Network.framework / `block2` / libdispatch
    /// bindings (a wrong signature or link would fail here on the macOS CI
    /// runner). The first path arrives asynchronously, so the status is normally
    /// `Unknown` at this instant - we only assert the calls are total.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_monitor_start_and_read_is_total() {
        let monitor = MacosMeteredMonitor::start();
        let _: MeteredStatus = monitor.status();
        // A second read is likewise total (and the handle clones cheaply).
        let _: MeteredStatus = monitor.clone().status();
    }
}
