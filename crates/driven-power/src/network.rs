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
//! ## Metered detection
//!
//! Per DESIGN s5.7 the real source is per-OS:
//! - Windows: `INetworkCostManager::GetCost`
//!   (`NLM_CONNECTION_COST_FIXED` / `_VARIABLE` -> metered).
//! - macOS: `NWPath.isExpensive`.
//! - Linux: NetworkManager's `Metered` property on the active connection.
//!
//! Each of those is a heavyweight async / COM / DBus integration. To keep
//! the 30 s poll cheap and the dependency surface minimal, the per-OS
//! probes are kept behind the seam below with a conservative default of
//! "not metered". The orchestrator honours `skip_on_metered` only when this
//! returns `true`, so the safe default is to *not* pause - a false "not
//! metered" merely fails to save a (rare) metered link, while a false
//! "metered" would wrongly stall all sync. See the per-OS TODOs.

/// Returns `(on_metered_network, network_reachable)` for the active
/// connection. Cheap and non-blocking; called on every power poll.
///
/// Reachability defaults to `true` (the network-resilience subsystem owns
/// the real classification - DESIGN s5.8). Metered defaults to `false` and
/// is refined by the per-OS hook once wired (see module docs).
pub(crate) fn detect_metered_and_reachable() -> (bool, bool) {
    let metered = detect_metered();
    // Coarse hint only; DESIGN s5.8 owns authoritative reachability.
    let reachable = true;
    (metered, reachable)
}

/// Per-OS metered-network probe. Conservative default: `false`.
#[cfg(target_os = "windows")]
fn detect_metered() -> bool {
    // TODO(DESIGN s5.7): query `INetworkListManager` ->
    // `INetworkCostManager::GetCost`; treat `NLM_CONNECTION_COST_FIXED`
    // and `_VARIABLE` as metered. Deferred: requires COM apartment init on
    // the poll thread, which is a heavier integration than the cheap 30 s
    // poll wants. Until then, assume unmetered (safe default - never
    // wrongly stalls sync).
    false
}

/// Per-OS metered-network probe. Conservative default: `false`.
#[cfg(target_os = "macos")]
fn detect_metered() -> bool {
    // TODO(DESIGN s5.7): observe `NWPathMonitor` and read
    // `NWPath.isExpensive` (bridged via objc2). Deferred: needs a
    // long-lived monitor on the dispatch queue rather than a one-shot read.
    // Until then, assume unmetered (safe default).
    false
}

/// Per-OS metered-network probe. Conservative default: `false`.
#[cfg(target_os = "linux")]
fn detect_metered() -> bool {
    // TODO(DESIGN s5.7): read NetworkManager's `Metered` property on the
    // active connection over DBus (`zbus`). The enum values 1 (yes) and 3
    // (guessed-yes) map to metered. Deferred: requires an async DBus call;
    // kept behind this seam to keep the poll cheap. Until then, assume
    // unmetered (safe default).
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
        // Safe defaults until per-OS hooks land: not metered, reachable.
        assert!(!metered);
        assert!(reachable);
    }
}
