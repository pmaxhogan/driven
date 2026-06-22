//! `driven-power` - battery / AC / metered-network / sleep-wake detection.
//!
//! Exposes a [`PowerSource`] trait plus a [`PowerState`] snapshot type.
//! Per-OS implementations (Windows `GetSystemPowerStatus` +
//! `WM_POWERBROADCAST`, macOS `IOPMCopyAssertionsByType` + `NSWorkspace`
//! sleep/wake notifications, Linux `/sys/class/power_supply` +
//! systemd-logind DBus) land in M3 per ROADMAP; the M1 phase 2 surface is
//! limited to the trait + state struct so the orchestrator (M2) can be
//! exercised against a `FakePowerSource` from
//! `driven-test-fixtures`.
//!
//! Mirrors SPEC s10 verbatim. The trait's [`PowerSource::subscribe`] return
//! type is [`tokio::sync::broadcast::Receiver`] as written in SPEC s10 -
//! the orchestrator (DESIGN s5.7) fan-outs power transitions to several
//! consumers (state-machine, tray icon, activity-log), and a broadcast
//! channel matches that fan-out shape one-to-one.

use async_trait::async_trait;
use tokio::sync::broadcast;

// Metered / reachability detection shared by every per-OS backend
// (DESIGN s5.7). Kept private; backends call it from their poll loop.
mod network;

// Per-OS [`PowerSource`] backends (DESIGN s5.7, s5.10.1). Exactly one is
// compiled per target; each exports a `RealPowerSource` re-exported below so
// the app wires `RealPowerSource::new()` without a `cfg` at the call site.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::RealPowerSource;
#[cfg(target_os = "macos")]
pub use macos::RealPowerSource;
/// The host's real [`PowerSource`] implementation. The app constructs it via
/// [`RealPowerSource::new`] and drives the 30 s poll loop via
/// `RealPowerSource::spawn_poller`. Exactly one per-OS definition is in
/// scope per target; tests use `FakePowerSource` from
/// `driven-test-fixtures` instead.
#[cfg(target_os = "windows")]
pub use windows::RealPowerSource;

/// Current power / network reachability snapshot (SPEC s10).
///
/// This struct is read on every orchestrator tick and embedded in
/// [`OrchestratorState`](../driven_core/index.html)'s `Paused`
/// representation when the gate flips closed. All fields together
/// determine the `PauseReason` the orchestrator surfaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PowerState {
    /// `true` when the machine is plugged into AC power. The orchestrator
    /// pauses sync when this is `false` and `settings.skip_on_battery` is
    /// set (DESIGN s5.7).
    pub ac_connected: bool,
    /// Battery charge percentage in `0..=100`, or `None` on a desktop /
    /// device with no battery present.
    pub battery_percent: Option<u8>,
    /// `true` when the active network connection is metered (Windows
    /// `NLM_CONNECTION_COST_FIXED`/`_VARIABLE`, macOS `NWPath.isExpensive`,
    /// Linux NetworkManager `Metered` per DESIGN s5.7).
    pub on_metered_network: bool,
    /// `true` when at least one network probe in DESIGN s5.8.2 succeeded
    /// most recently. `false` covers airplane mode, link-local-only,
    /// captive portal, and DNS-failed states; the orchestrator
    /// distinguishes those at the network-resilience layer, not here.
    pub network_reachable: bool,
}

/// Power-source signal contract (SPEC s10).
///
/// Implementations are kept cheap to call: the orchestrator queries
/// [`PowerSource::current`] on every tick, and [`PowerSource::subscribe`]
/// on each long-lived consumer (state-machine, tray, activity-log writer)
/// that needs to react to transitions without polling.
///
/// Production impls poll the OS at 30 s and additionally listen for
/// event-driven signals where available (`WM_POWERBROADCAST` on Windows,
/// `NSWorkspaceWillSleepNotification` on macOS, the
/// `org.freedesktop.login1.Manager.PrepareForSleep` DBus signal on
/// Linux). The
/// [`FakePowerSource`](../driven_test_fixtures/power/struct.FakePowerSource.html)
/// in `driven-test-fixtures` lets a test push transitions
/// deterministically.
#[async_trait]
pub trait PowerSource: Send + Sync {
    /// Returns the latest known [`PowerState`] snapshot.
    async fn current(&self) -> PowerState;

    /// Returns a new [`broadcast::Receiver`] that observes every
    /// subsequent state transition. Consumers that are slow enough to lag
    /// the channel will see [`broadcast::error::RecvError::Lagged`] - the
    /// orchestrator treats that as "re-read `current()` and resume" so
    /// missed intermediate states do not stall sync.
    fn subscribe(&self) -> broadcast::Receiver<PowerState>;
}
