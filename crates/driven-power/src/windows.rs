//! Windows [`PowerSource`] backend (DESIGN s5.7, s5.10.1).
//!
//! AC / battery state comes from `GetSystemPowerStatus`
//! (`Win32::System::Power`). Metered-network detection is delegated to
//! [`crate::network`]. The source polls every 30 s (DESIGN s5.7) and
//! broadcasts a fresh [`PowerState`] whenever any field changes; consumers
//! subscribe via [`PowerSource::subscribe`].
//!
//! Sleep / wake (DESIGN s5.10.1) IS wired here (issue #33) via
//! `PowerRegisterSuspendResumeNotification` with a `DEVICE_NOTIFY_CALLBACK`.
//! The system invokes [`suspend_resume_callback`] directly - NO hidden
//! message-pump window or dedicated thread is required (the callback form is
//! the documented "app has no window / wants a direct callback" path) - and
//! the callback maps the `PBT_*` event code to a [`SleepWakeEvent`] and
//! broadcasts it. The orchestrator subscribes via
//! [`PowerSource::subscribe_sleep_wake`] and runs the s5.10.2 / s5.10.3
//! suspend / resume sequences at the edge instead of waiting for the 30 s
//! poll.

use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::sync::Mutex;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Power::{
    GetSystemPowerStatus, PowerRegisterSuspendResumeNotification,
    PowerUnregisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY,
    SYSTEM_POWER_STATUS,
};
use windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_CALLBACK;

use crate::network::{detect_metered, reachable_hint};
use crate::{PowerSource, PowerState, SleepWakeEvent, SleepWakeMonitor};
use async_trait::async_trait;

/// Poll cadence for the OS power query (DESIGN s5.7: "polled at 30s ...
/// as a fallback").
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Capacity of the broadcast channel. Transitions are rare (AC plug /
/// unplug, metered toggle), so a small buffer is ample; a lagging consumer
/// re-reads [`PowerSource::current`] per the trait contract.
const BROADCAST_CAPACITY: usize = 16;

/// Capacity of the sleep/wake edge broadcast channel. Edges are extremely
/// rare (one per real suspend / resume), so a tiny buffer is ample.
const SLEEP_WAKE_CAPACITY: usize = 8;

// `WM_POWERBROADCAST` `wParam` event codes (winuser.h). Defined locally as
// plain `u32`s (their values are ABI-stable) so the mapping does not depend on
// which `windows`-crate feature surfaces the `PBT_*` constants.
/// The system is suspending (entering sleep / hibernate).
const PBT_APMSUSPEND: u32 = 0x0004;
/// The system has resumed after a suspend the app requested awareness of.
const PBT_APMRESUMESUSPEND: u32 = 0x0007;
/// The system has resumed automatically (e.g. a scheduled wake).
const PBT_APMRESUMEAUTOMATIC: u32 = 0x0012;

/// `ERROR_SUCCESS` (`WIN32_ERROR(0)`), the success return of
/// `PowerRegisterSuspendResumeNotification`.
const ERROR_SUCCESS: u32 = 0;

/// `SYSTEM_POWER_STATUS::ACLineStatus` sentinel for "AC online".
const AC_LINE_ONLINE: u8 = 1;

/// `SYSTEM_POWER_STATUS::BatteryLifePercent` sentinel for "unknown".
const BATTERY_PERCENT_UNKNOWN: u8 = 255;

/// Real Windows power source. Construct with [`RealPowerSource::new`]; the
/// app spawns the polling loop via [`RealPowerSource::spawn_poller`].
pub struct RealPowerSource {
    latest: Arc<Mutex<PowerState>>,
    tx: broadcast::Sender<PowerState>,
    /// Broadcasts OS sleep/wake edges (DESIGN s5.10.1). The monitor started by
    /// [`Self::spawn_sleep_wake_monitor`] sends on a clone of this; subscribers
    /// read via [`PowerSource::subscribe_sleep_wake`].
    sleep_wake_tx: broadcast::Sender<SleepWakeEvent>,
}

impl RealPowerSource {
    /// Builds the source with an initial snapshot read synchronously from
    /// the OS. Returns an error if the very first `GetSystemPowerStatus`
    /// call fails (a genuinely broken host); transient later failures keep
    /// the last good snapshot.
    pub fn new() -> anyhow::Result<Self> {
        let initial = read_power_state()?;
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let (sleep_wake_tx, _sw_rx) = broadcast::channel(SLEEP_WAKE_CAPACITY);
        Ok(Self {
            latest: Arc::new(Mutex::new(initial)),
            tx,
            sleep_wake_tx,
        })
    }

    /// Spawns the 30 s polling loop. Each tick re-reads the OS power status
    /// plus metered/reachability and broadcasts a new [`PowerState`] only on
    /// change. The returned [`tokio::task::JoinHandle`] is owned by the app
    /// so it can abort the loop on shutdown.
    ///
    /// The sleep/wake EDGE seam (DESIGN s5.10.1) is separate; start it via
    /// [`Self::spawn_sleep_wake_monitor`].
    pub fn spawn_poller(&self) -> tokio::task::JoinHandle<()> {
        let latest = Arc::clone(&self.latest);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(POLL_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let next = match read_power_state() {
                    Ok(s) => s,
                    Err(err) => {
                        tracing::warn!(error = %err, "GetSystemPowerStatus poll failed; keeping last snapshot");
                        continue;
                    }
                };
                let mut guard = latest.lock().await;
                if *guard != next {
                    tracing::debug!(?next, "power state transition");
                    *guard = next.clone();
                    // A send error only means no live receivers; that is
                    // fine - the next subscriber reads `current()`.
                    let _ = tx.send(next);
                }
            }
        })
    }

    /// Starts the OS sleep/wake monitor (DESIGN s5.10.1, issue #33).
    ///
    /// Registers [`suspend_resume_callback`] via
    /// `PowerRegisterSuspendResumeNotification` with `DEVICE_NOTIFY_CALLBACK`,
    /// so the system delivers `PBT_APMSUSPEND` / `PBT_APMRESUME*` directly to
    /// the callback (no window, no message pump). The callback broadcasts the
    /// mapped [`SleepWakeEvent`] on a clone of this source's `sleep_wake_tx`;
    /// subscribers read them via [`PowerSource::subscribe_sleep_wake`].
    ///
    /// The returned [`SleepWakeMonitor`] owns the registration handle and a
    /// heap box holding the sender clone the callback reads; dropping it (or
    /// calling [`SleepWakeMonitor::stop`]) unregisters the callback and frees
    /// the box, so a clean quit leaves no dangling OS registration.
    ///
    /// Errors if the OS refuses the registration (rare; e.g. resource
    /// exhaustion) - the caller logs it and the app degrades to the 30 s poll.
    pub fn spawn_sleep_wake_monitor(&self) -> anyhow::Result<SleepWakeMonitor> {
        // The callback needs the sender for the whole registration lifetime, so
        // leak a Box holding a clone and hand its pointer in as the Context; the
        // teardown closure frees it AFTER unregistering (so no callback can run
        // against a freed box).
        let ctx: *mut broadcast::Sender<SleepWakeEvent> =
            Box::into_raw(Box::new(self.sleep_wake_tx.clone()));

        let mut params = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(suspend_resume_callback),
            Context: ctx as *mut c_void,
        };
        let mut handle_raw: *mut c_void = std::ptr::null_mut();

        // SAFETY: `params` is valid for the duration of the call (the system
        // copies the callback + context out of it); `handle_raw` receives the
        // registration handle. On any failure we free the leaked `ctx` box so it
        // does not leak.
        let status = unsafe {
            PowerRegisterSuspendResumeNotification(
                DEVICE_NOTIFY_CALLBACK,
                HANDLE(&mut params as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS as *mut c_void),
                &mut handle_raw,
            )
        };
        if status.0 != ERROR_SUCCESS {
            // SAFETY: registration failed, so no callback will ever run against
            // `ctx`; reclaim the box to avoid a leak.
            unsafe { drop(Box::from_raw(ctx)) };
            anyhow::bail!(
                "PowerRegisterSuspendResumeNotification failed (WIN32_ERROR {})",
                status.0
            );
        }

        let teardown = SuspendResumeTeardown {
            handle: HPOWERNOTIFY(handle_raw as isize),
            ctx,
        };
        // The closure captures `teardown` as a WHOLE `Send` value (it calls a
        // method on it), so Rust 2021 disjoint capture does not reach into the
        // raw-pointer field - the closure stays `Send` for `SleepWakeMonitor`.
        Ok(SleepWakeMonitor::new(move || teardown.run()))
    }
}

/// The Windows sleep/wake teardown: the registration handle plus the leaked
/// context-box pointer the callback read. A `Send` newtype (its raw pointer is
/// touched exactly once, by [`Self::run`], single-threaded, after the OS
/// registration is removed) so it can live in the [`SleepWakeMonitor`] teardown
/// closure.
struct SuspendResumeTeardown {
    handle: HPOWERNOTIFY,
    ctx: *mut broadcast::Sender<SleepWakeEvent>,
}
// SAFETY: `handle` is a plain handle; `ctx` is only dereferenced once, by
// `run`, after `PowerUnregisterSuspendResumeNotification` has stopped any
// further callback invocations - so there is no cross-thread aliasing.
unsafe impl Send for SuspendResumeTeardown {}

impl SuspendResumeTeardown {
    /// Unregister the callback, then reclaim the context box. Runs once.
    fn run(self) {
        // SAFETY: unregister FIRST so the system stops invoking the callback,
        // then reclaim the box. `handle` came from a successful registration; a
        // failed unregister is logged, not fatal.
        unsafe {
            if PowerUnregisterSuspendResumeNotification(self.handle).0 != ERROR_SUCCESS {
                tracing::warn!("PowerUnregisterSuspendResumeNotification failed");
            }
            drop(Box::from_raw(self.ctx));
        }
    }
}

/// Maps a `WM_POWERBROADCAST` `wParam` event code to a [`SleepWakeEvent`], or
/// `None` for the many codes we do not act on (`PBT_APMPOWERSTATUSCHANGE`,
/// `PBT_POWERSETTINGCHANGE`, the deprecated query events, etc.). Pure, so it is
/// unit-tested without a real suspend.
fn sleep_wake_from_pbt(event_type: u32) -> Option<SleepWakeEvent> {
    match event_type {
        PBT_APMSUSPEND => Some(SleepWakeEvent::Suspending),
        PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => Some(SleepWakeEvent::Resumed),
        _ => None,
    }
}

/// The `PDEVICE_NOTIFY_CALLBACK_ROUTINE` the system invokes on a suspend/resume
/// edge. `context` is the `*const broadcast::Sender<SleepWakeEvent>` we passed
/// at registration; `event_type` is the `PBT_*` code. Returns `ERROR_SUCCESS`.
///
/// SAFETY / robustness: the callback runs on a system thread. It only reads the
/// sender (which outlives the registration - freed only by the teardown AFTER
/// unregistration) and does a non-blocking `broadcast::send`, so it never
/// panics across the FFI boundary or blocks the power-notification path.
unsafe extern "system" fn suspend_resume_callback(
    context: *const c_void,
    event_type: u32,
    _setting: *const c_void,
) -> u32 {
    if context.is_null() {
        return ERROR_SUCCESS;
    }
    // SAFETY: `context` is the pointer registered in `spawn_sleep_wake_monitor`;
    // it points at a live `broadcast::Sender` until the teardown (which only runs
    // after unregistration, so not concurrently with this call).
    let tx = unsafe { &*(context as *const broadcast::Sender<SleepWakeEvent>) };
    if let Some(event) = sleep_wake_from_pbt(event_type) {
        // A send error only means no live subscribers - benign.
        let _ = tx.send(event);
    }
    ERROR_SUCCESS
}

#[async_trait]
impl PowerSource for RealPowerSource {
    async fn current(&self) -> PowerState {
        self.latest.lock().await.clone()
    }

    fn subscribe(&self) -> broadcast::Receiver<PowerState> {
        self.tx.subscribe()
    }

    fn subscribe_sleep_wake(&self) -> broadcast::Receiver<SleepWakeEvent> {
        self.sleep_wake_tx.subscribe()
    }
}

/// Reads a full [`PowerState`] snapshot: AC / battery from
/// `GetSystemPowerStatus`, metered / reachability from [`crate::network`].
fn read_power_state() -> anyhow::Result<PowerState> {
    let (ac_connected, battery_percent) = read_ac_and_battery()?;
    Ok(PowerState {
        ac_connected,
        battery_percent,
        on_metered_network: detect_metered().on_metered(),
        network_reachable: reachable_hint(),
    })
}

/// Calls `GetSystemPowerStatus` and normalizes the raw sentinels into
/// `(ac_connected, battery_percent)`.
///
/// - `ACLineStatus == 1` -> on AC. `0` -> on battery. `255` (unknown) is
///   treated as on AC so we never falsely pause an unknown-state desktop.
/// - `BatteryLifePercent == 255` -> no battery / unknown -> `None`.
fn read_ac_and_battery() -> anyhow::Result<(bool, Option<u8>)> {
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: `GetSystemPowerStatus` writes into the provided struct and
    // returns a `BOOL`; the pointer is valid for the duration of the call.
    unsafe { GetSystemPowerStatus(&mut status) }
        .map_err(|e| anyhow::anyhow!("GetSystemPowerStatus failed: {e}"))?;

    let ac_connected = match status.ACLineStatus {
        0 => false,
        AC_LINE_ONLINE => true,
        // Unknown (255) or any other value: assume powered so an unknown
        // host is not spuriously paused on the battery gate.
        _ => true,
    };

    let battery_percent = if status.BatteryLifePercent == BATTERY_PERCENT_UNKNOWN {
        None
    } else {
        Some(status.BatteryLifePercent.min(100))
    };

    Ok((ac_connected, battery_percent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_plausible_power_state_on_this_host() {
        let (ac, battery) = read_ac_and_battery().expect("GetSystemPowerStatus on host");
        if let Some(pct) = battery {
            assert!(pct <= 100, "battery percent in range: {pct}");
        }
        // `ac` is a valid bool either way; assert the call simply succeeded.
        let _ = ac;
    }

    #[tokio::test]
    async fn current_returns_plausible_state() {
        let src = RealPowerSource::new().expect("construct RealPowerSource");
        let state = src.current().await;
        if let Some(pct) = state.battery_percent {
            assert!(pct <= 100);
        }
    }

    /// The pure PBT -> [`SleepWakeEvent`] mapping (the heart of the callback),
    /// tested without a real suspend: suspend maps to `Suspending`, both resume
    /// codes map to `Resumed`, and unrelated codes map to `None`.
    #[test]
    fn pbt_maps_to_sleep_wake_events() {
        assert_eq!(
            sleep_wake_from_pbt(PBT_APMSUSPEND),
            Some(SleepWakeEvent::Suspending)
        );
        assert_eq!(
            sleep_wake_from_pbt(PBT_APMRESUMEAUTOMATIC),
            Some(SleepWakeEvent::Resumed)
        );
        assert_eq!(
            sleep_wake_from_pbt(PBT_APMRESUMESUSPEND),
            Some(SleepWakeEvent::Resumed)
        );
        // PBT_APMPOWERSTATUSCHANGE (0xA) and any other code are not sleep/wake.
        assert_eq!(sleep_wake_from_pbt(0x000A), None);
        assert_eq!(sleep_wake_from_pbt(0x0000), None);
    }

    /// Invoking the raw `PDEVICE_NOTIFY_CALLBACK_ROUTINE` with a suspend code
    /// broadcasts `Suspending` to a live subscriber - exercising the exact FFI
    /// callback the OS calls, end to end, WITHOUT sleeping the machine (a real
    /// suspend cannot run in an automated test).
    #[tokio::test]
    async fn callback_broadcasts_mapped_edge_to_subscriber() {
        let src = RealPowerSource::new().expect("construct RealPowerSource");
        let mut rx = src.subscribe_sleep_wake();
        // Pass the source's own sender as the callback context, exactly as the
        // real registration does.
        let ctx: *mut broadcast::Sender<SleepWakeEvent> =
            Box::into_raw(Box::new(src.sleep_wake_tx.clone()));
        // SAFETY: `ctx` is a valid live sender box; freed below.
        unsafe {
            suspend_resume_callback(ctx as *const c_void, PBT_APMSUSPEND, std::ptr::null());
            suspend_resume_callback(
                ctx as *const c_void,
                PBT_APMRESUMEAUTOMATIC,
                std::ptr::null(),
            );
            // An unrelated code must NOT emit an edge.
            suspend_resume_callback(ctx as *const c_void, 0x000A, std::ptr::null());
            drop(Box::from_raw(ctx));
        }
        assert_eq!(rx.recv().await.unwrap(), SleepWakeEvent::Suspending);
        assert_eq!(rx.recv().await.unwrap(), SleepWakeEvent::Resumed);
        // No third edge from the unrelated code: the channel now has no more
        // messages (a non-blocking try_recv is Empty, not another event).
        assert!(rx.try_recv().is_err());
    }

    /// The real OS registration succeeds on this Windows host and the returned
    /// monitor tears the registration down cleanly on `stop()` (no panic, no
    /// leak of the context box) - the end-to-end lifecycle short of an actual
    /// suspend.
    #[test]
    fn monitor_registers_and_tears_down_on_this_host() {
        let src = RealPowerSource::new().expect("construct RealPowerSource");
        let monitor = src
            .spawn_sleep_wake_monitor()
            .expect("PowerRegisterSuspendResumeNotification should succeed on this host");
        // Explicit teardown (also covered by Drop); must not panic.
        monitor.stop();
        // A second source + drop-based teardown also completes cleanly.
        let src2 = RealPowerSource::new().expect("construct RealPowerSource");
        let _ = src2.spawn_sleep_wake_monitor().expect("register again");
        // dropped here -> Drop tears it down.
    }
}
