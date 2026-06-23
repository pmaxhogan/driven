//! Metered-network detection + reachability hint for [`PowerState`]
//! (DESIGN s5.7).
//!
//! [`detect_metered_and_reachable`] returns `(on_metered_network,
//! network_reachable)` for the active connection. It is called on every
//! 30 s power poll by each per-OS [`crate::PowerSource`] backend, so it must
//! be cheap and non-blocking.
//!
//! ## Scope boundary
//!
//! `network_reachable` here is only the *coarse* hint embedded in
//! [`PowerState`]. The authoritative reachability classification - airplane
//! mode vs captive portal vs DNS-broken vs Drive-down - is the
//! network-resilience subsystem's job (DESIGN s5.8, three-probe topology),
//! not this module's. We default `network_reachable = true` and let that
//! subsystem flip the orchestrator's gate; this hint exists so a
//! `PowerState` snapshot is self-contained for the tray / activity-log
//! consumers that only need a rough "are we online" flag.
//!
//! ## Metered detection (CODEX_NOTES P2-10)
//!
//! Per DESIGN s5.7 the real source is per-OS:
//! - Windows: `INetworkListManager` -> `INetworkConnectionCost` /
//!   `INetworkCostManager::GetCost` (`NLM_CONNECTION_COST_FIXED` /
//!   `_VARIABLE` / `_CONGESTED` / `_OVERDATALIMIT` / `_APPROACHINGDATALIMIT`
//!   -> metered; `_UNRESTRICTED` -> not metered).
//! - macOS: `NWPath.isExpensive`.
//! - Linux: NetworkManager's `Metered` property on the active connection.
//!
//! ### What is real here vs a documented conservative default
//!
//! Windows is a REAL read (COM `INetworkCostManager::GetCost`). macOS and
//! Linux remain documented conservative defaults of `false` (not metered);
//! the reasons are recorded honestly per OS in the `detect_metered` cfg arms,
//! and these are NOT "pretend it works" stubs. The safe direction of the
//! default is deliberate: the orchestrator honours `skip_on_metered` only
//! when this returns `true`, so a false "not metered" merely fails to save a
//! (rare) metered link, while a false "metered" would wrongly stall ALL sync.
//! For the same reason, the Windows path itself falls back to `false` on ANY
//! COM error rather than guessing "metered".
//!
//! - **Windows** is REAL (DESIGN s5.7's named primary path). It instantiates
//!   the `NetworkListManager` COM object, queries its `INetworkCostManager`
//!   facet, and reads `GetCost`; any of `NLM_CONNECTION_COST_FIXED` /
//!   `_VARIABLE` / `_CONGESTED` / `_OVERDATALIMIT` / `_APPROACHINGDATALIMIT` /
//!   `_ROAMING` -> metered, `_UNRESTRICTED` -> not metered, `_UNKNOWN` /
//!   any failure -> the safe `false`. The `windows` crate's
//!   `Win32_Networking_NetworkListManager` + `Win32_System_Com` features back
//!   it (see this crate's Cargo.toml).
//! - **Linux** has `zbus` declared, but NetworkManager's per-connection
//!   `Metered` property is internal NM state exposed only over DBus (enumerate
//!   the active connection, read its `Dbus` `Metered` property) - there is no
//!   cheap synchronous `/sys` or `/proc` file for it. A DBus round-trip from
//!   this synchronous, 30 s-cadence `detect_metered()` is not a "cheap read",
//!   so Linux keeps the conservative `false`.
//! - **macOS** `NWPath.isExpensive` requires a live `NWPathMonitor` running
//!   on a dispatch queue (there is no one-shot read), so it cannot be
//!   sampled synchronously here; macOS keeps the conservative `false`.

/// Returns `(on_metered_network, network_reachable)` for the active
/// connection. Cheap and non-blocking; called on every power poll.
///
/// Reachability defaults to `true` (the network-resilience subsystem owns
/// the real classification - DESIGN s5.8). Metered is delegated to the
/// per-OS [`detect_metered`]; every OS arm currently returns the documented
/// conservative `false` default (see the module docs + each arm for the
/// precise per-OS reason - CODEX_NOTES P2-10).
pub(crate) fn detect_metered_and_reachable() -> (bool, bool) {
    let metered = detect_metered();
    // Coarse hint only; DESIGN s5.8 owns authoritative reachability.
    let reachable = true;
    (metered, reachable)
}

/// Per-OS metered-network probe (Windows) - REAL (DESIGN s5.7, CODEX_NOTES
/// P2-10).
///
/// Instantiates the `NetworkListManager` COM object, casts it to
/// `INetworkCostManager`, and reads `GetCost` for the machine-wide active
/// connection (`pdestipaddr = NULL`). The returned `u32` is a bitmask of
/// `NLM_CONNECTION_COST` flags; any of FIXED / VARIABLE / CONGESTED /
/// OVERDATALIMIT / APPROACHINGDATALIMIT / ROAMING means the link is metered.
/// `UNRESTRICTED` (and the `UNKNOWN`/empty value) means not metered.
///
/// Safety + robustness: the COM calls are `unsafe`, but every failure path
/// (apartment init refused, object/interface unavailable, `GetCost` error)
/// collapses to the safe `false` default rather than guessing "metered" -
/// a wrong "metered" would stall ALL sync, a wrong "not metered" only fails
/// to skip a rare metered link. COM is initialised per call as
/// multi-threaded-apartment; an `RPC_E_CHANGED_MODE` (apartment already
/// initialised differently on this thread by the host process) is tolerated
/// because the COM call still works against the existing apartment.
#[cfg(target_os = "windows")]
fn detect_metered() -> bool {
    use windows::core::HRESULT;
    use windows::Win32::Networking::NetworkListManager::{
        INetworkCostManager, NetworkListManager, NLM_CONNECTION_COST_APPROACHINGDATALIMIT,
        NLM_CONNECTION_COST_CONGESTED, NLM_CONNECTION_COST_FIXED,
        NLM_CONNECTION_COST_OVERDATALIMIT, NLM_CONNECTION_COST_ROAMING,
        NLM_CONNECTION_COST_VARIABLE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    // RPC_E_CHANGED_MODE: COM already initialised on this thread with a
    // different apartment model. Not fatal - the existing apartment serves
    // our in-proc call fine, so we proceed without treating it as an error.
    const RPC_E_CHANGED_MODE: HRESULT = HRESULT(0x8001_0106u32 as i32);

    // SAFETY: standard COM init -> create-instance -> query-facet -> call
    // sequence. Each pointer is owned by the `windows` smart wrappers
    // (refcounted), and every fallible step short-circuits to `false`.
    unsafe {
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        // `CoInitializeEx` returns an HRESULT-like; S_FALSE means "already
        // initialised on this thread" (still success). Only a hard failure
        // other than CHANGED_MODE should abort.
        if init.is_err() && init != RPC_E_CHANGED_MODE {
            return false;
        }

        let cost_manager: windows::core::Result<INetworkCostManager> =
            CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL);
        let cost_manager = match cost_manager {
            Ok(cm) => cm,
            Err(_) => return false,
        };

        let mut cost: u32 = 0;
        // NULL destination address -> the machine-wide active connection cost.
        if cost_manager.GetCost(&mut cost, std::ptr::null()).is_err() {
            return false;
        }

        let metered_flags = (NLM_CONNECTION_COST_FIXED.0
            | NLM_CONNECTION_COST_VARIABLE.0
            | NLM_CONNECTION_COST_CONGESTED.0
            | NLM_CONNECTION_COST_OVERDATALIMIT.0
            | NLM_CONNECTION_COST_APPROACHINGDATALIMIT.0
            | NLM_CONNECTION_COST_ROAMING.0) as u32;

        (cost & metered_flags) != 0
    }
}

/// Per-OS metered-network probe (macOS).
///
/// CONSERVATIVE DEFAULT `false` - NOT a real implementation. DESIGN s5.7
/// names `NWPath.isExpensive`, but that is only observable through a live
/// `NWPathMonitor` running on a dispatch queue (there is no one-shot,
/// synchronous read), so it cannot be sampled from this synchronous
/// 30 s-cadence probe without standing up a long-lived monitor (out of scope
/// here). `false` is the safe default until a monitor-backed reader lands.
#[cfg(target_os = "macos")]
fn detect_metered() -> bool {
    false
}

/// Per-OS metered-network probe (Linux).
///
/// CONSERVATIVE DEFAULT `false` - NOT a real implementation. DESIGN s5.7
/// names NetworkManager's per-connection `Metered` property (enum 1 = yes,
/// 3 = guessed-yes -> metered). That property is internal NM state exposed
/// only over DBus (enumerate the active connection, read its `Metered`
/// property via `zbus`); there is no cheap synchronous `/sys` or `/proc`
/// file for it. A DBus round-trip does not fit this synchronous,
/// 30 s-cadence `detect_metered()` (the task's "cheap read if doable"
/// carve-out is not met), so Linux keeps the safe `false` default until a
/// DBus-backed reader on a separate async path lands.
#[cfg(target_os = "linux")]
fn detect_metered() -> bool {
    false
}

/// Fallback for any other target (none shipped; keeps the crate buildable
/// on exotic hosts and in `cargo check --all-targets`).
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn detect_metered() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let (metered, reachable) = detect_metered_and_reachable();
        // Reachability is always the coarse `true` hint (DESIGN s5.8 owns the
        // authoritative classification).
        assert!(reachable);
        // On non-Windows, metered detection is the documented conservative
        // `false` default, so we can assert it precisely.
        #[cfg(not(target_os = "windows"))]
        assert!(!metered);
        // On Windows the read is REAL (INetworkCostManager::GetCost), so the
        // value is environment-dependent (a metered link legitimately yields
        // `true`). We only assert the call completes and returns a bool
        // without panicking; `metered` is intentionally used here so the
        // binding is not flagged unused on Windows.
        #[cfg(target_os = "windows")]
        let _ = metered;
    }

    /// The Windows real read must never panic and must return a plain bool,
    /// regardless of the host's current connection cost (metered or not).
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_metered_read_is_total_and_does_not_panic() {
        let _: bool = detect_metered();
    }
}
