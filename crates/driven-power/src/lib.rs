//! `driven-power` - battery / AC / metered-network / sleep-wake detection.
//!
//! Exposes a [`PowerSource`] trait plus a [`PowerState`] steady-state snapshot
//! and a [`SleepWakeEvent`] suspend/resume EDGE. Per-OS backends
//! ([`RealPowerSource`], one per target) read AC / battery + metered /
//! reachability on a 30 s poll (Windows `GetSystemPowerStatus`, macOS IOKit
//! `IOPowerSources`, Linux `/sys/class/power_supply`) and additionally emit
//! sleep/wake edges from the OS suspend/resume notification (DESIGN s5.10.1,
//! issue #33): Windows `PowerRegisterSuspendResumeNotification`, macOS IOKit
//! `IORegisterForSystemPower`, Linux systemd-logind `PrepareForSleep` over
//! DBus. Tests exercise the orchestrator against a `FakePowerSource` from
//! `driven-test-fixtures`.
//!
//! Mirrors SPEC s10. The trait's [`PowerSource::subscribe`] /
//! [`PowerSource::subscribe_sleep_wake`] return
//! [`tokio::sync::broadcast::Receiver`]s - the orchestrator (DESIGN s5.7,
//! s5.10) fans out power transitions and sleep/wake edges to several consumers
//! (state-machine, tray icon, activity-log), and a broadcast channel matches
//! that fan-out shape one-to-one.

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

/// An OS sleep / wake EDGE (DESIGN s5.10.1). Distinct from [`PowerState`]
/// (a steady-state AC / battery / metered / reachability snapshot): a
/// [`SleepWakeEvent`] fires exactly on the suspend / resume transition, so
/// the orchestrator can run the DESIGN s5.10.2 / s5.10.3 suspend / resume
/// sequences AT the edge instead of waiting up to 30 s for the poll to
/// notice.
///
/// Emitted by the per-OS sleep/wake monitor each backend starts via
/// [`RealPowerSource::spawn_sleep_wake_monitor`] and delivered to
/// subscribers of [`PowerSource::subscribe_sleep_wake`]; the
/// [`FakePowerSource`](../driven_test_fixtures/power/struct.FakePowerSource.html)
/// lets a test push edges deterministically. `driven-core` maps this to its
/// own `PowerEvent` at the orchestrator boundary (the two crates do not
/// share a type: `driven-power` carries no `driven-core` dependency).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SleepWakeEvent {
    /// The machine is about to sleep / hibernate (`PBT_APMSUSPEND` /
    /// `kIOMessageSystemWillSleep` / logind `PrepareForSleep(true)`).
    Suspending,
    /// The machine has resumed from sleep (`PBT_APMRESUMEAUTOMATIC` /
    /// `PBT_APMRESUMESUSPEND` / `kIOMessageSystemHasPoweredOn` / logind
    /// `PrepareForSleep(false)`).
    Resumed,
}

/// A running OS sleep/wake monitor (DESIGN s5.10.1), returned by each per-OS
/// [`RealPowerSource::spawn_sleep_wake_monitor`]. It broadcasts
/// [`SleepWakeEvent`]s to the source's [`PowerSource::subscribe_sleep_wake`]
/// receivers until it is dropped or [`SleepWakeMonitor::stop`]ped, at which
/// point it tears down the OS registration (unregister the Win32 suspend/
/// resume callback, stop + join the macOS `CFRunLoop` thread, or abort the
/// Linux logind DBus task) so a clean quit leaves no orphaned OS handle,
/// thread, or task.
///
/// A single uniform type across every OS (the teardown differs per backend
/// but the app-shell only needs to HOLD the monitor and drop it), so the
/// app-shell wiring is `cfg`-free.
#[must_use = "dropping the monitor immediately tears down OS sleep/wake notifications"]
pub struct SleepWakeMonitor {
    /// The per-OS teardown, run exactly once (on `stop()` or `Drop`).
    stop: Option<Box<dyn FnOnce() + Send>>,
}

impl SleepWakeMonitor {
    /// Build a monitor from a per-OS teardown closure. Backend-internal.
    pub(crate) fn new(stop: impl FnOnce() + Send + 'static) -> Self {
        Self {
            stop: Some(Box::new(stop)),
        }
    }

    /// Explicitly tear down the OS registration now. Idempotent and
    /// equivalent to dropping the monitor; provided so the app-shell can
    /// stop it deterministically on quit rather than waiting for the drop.
    pub fn stop(mut self) {
        if let Some(f) = self.stop.take() {
            f();
        }
    }
}

impl Drop for SleepWakeMonitor {
    fn drop(&mut self) {
        if let Some(f) = self.stop.take() {
            f();
        }
    }
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

    /// Returns a [`broadcast::Receiver`] that observes every OS sleep / wake
    /// EDGE (DESIGN s5.10.1). Distinct from [`PowerSource::subscribe`], which
    /// carries steady-state [`PowerState`] snapshots: this carries the
    /// suspend / resume [`SleepWakeEvent`] edges the orchestrator runs the
    /// s5.10.2 / s5.10.3 sequences on.
    ///
    /// The default returns a live-but-silent receiver (a process-wide channel
    /// whose sender is kept alive forever and never sent to), so an impl that
    /// cannot observe sleep/wake simply never emits an edge and the
    /// orchestrator's select arm parks on it inertly rather than seeing a
    /// spurious `Closed`. The real per-OS backends and the test fake override
    /// it with a channel their monitor / test harness broadcasts to.
    fn subscribe_sleep_wake(&self) -> broadcast::Receiver<SleepWakeEvent> {
        static SILENT: std::sync::OnceLock<broadcast::Sender<SleepWakeEvent>> =
            std::sync::OnceLock::new();
        SILENT.get_or_init(|| broadcast::channel(1).0).subscribe()
    }
}
