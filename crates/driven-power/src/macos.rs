//! macOS [`PowerSource`] backend (DESIGN s5.7, s5.10.1).
//!
//! AC / battery state comes from IOKit's IOPowerSources API
//! (`IOPSCopyPowerSourcesInfo` + `IOPSCopyPowerSourcesList`, reading each
//! source's description dictionary). Metered-network detection is delegated
//! to [`crate::network`]. The source polls every 30 s (DESIGN s5.7) and
//! broadcasts a fresh [`PowerState`] whenever any field changes.
//!
//! Sleep / wake (DESIGN s5.10.1) IS wired here (issue #33) via IOKit's
//! `IORegisterForSystemPower`, running on a dedicated `CFRunLoop` thread.
//! This is the documented C sleep/wake API (`kIOMessageSystemWillSleep` /
//! `kIOMessageSystemHasPoweredOn`); it matches both the existing IOKit power
//! reader in this file and the Windows callback shape, and - unlike
//! `NSWorkspace` notifications - needs no AppKit main-thread run loop, so it
//! is self-contained. The callback maps the IOKit message to a
//! [`SleepWakeEvent`] and broadcasts it; the orchestrator subscribes via
//! [`PowerSource::subscribe_sleep_wake`]. This backend is COMPILE-VERIFIED
//! via CI's macOS job (the implementer builds on Windows); the FFI is written
//! against Apple's documented IOKit signatures + `core-foundation-sys`
//! run-loop bindings.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation_sys::runloop::{
    kCFRunLoopCommonModes, CFRunLoopAddSource, CFRunLoopGetCurrent, CFRunLoopRef, CFRunLoopRun,
    CFRunLoopSourceRef, CFRunLoopStop,
};

use async_trait::async_trait;
use tokio::sync::broadcast;
use tokio::sync::Mutex;

use crate::network::{reachable_hint, MacosMeteredMonitor, MeteredStatus};
use crate::{PowerSource, PowerState, SleepWakeEvent, SleepWakeMonitor};

/// Poll cadence for the OS power query (DESIGN s5.7).
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Broadcast channel capacity (transitions are rare).
const BROADCAST_CAPACITY: usize = 16;

/// Capacity of the sleep/wake edge broadcast channel. Edges are extremely
/// rare (one per real suspend / resume), so a tiny buffer is ample.
const SLEEP_WAKE_CAPACITY: usize = 8;

// IOKit system-power message codes (IOKit/IOMessage.h; values are ABI-stable).
/// The system is pondering an idle sleep and lets apps veto; we always allow
/// (ack) it so we never delay the user's sleep. Not itself a suspend edge.
const K_IO_MESSAGE_CAN_SYSTEM_SLEEP: u32 = 0xe000_0270;
/// The system is initiating a non-abortable sleep (the suspend edge). We
/// broadcast `Suspending` and must ack so the system proceeds promptly.
const K_IO_MESSAGE_SYSTEM_WILL_SLEEP: u32 = 0xe000_0280;
/// The system finished waking (the resume edge). We broadcast `Resumed`.
const K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON: u32 = 0xe000_0300;

/// `MACH_PORT_NULL` - the failure sentinel of `IORegisterForSystemPower`.
const MACH_PORT_NULL: io_connect_t = 0;

// IOKit / Mach types. `mach_port_t` (io_connect_t / io_object_t /
// io_service_t) is `unsigned int` on Darwin.
#[allow(non_camel_case_types)]
type io_connect_t = u32;
#[allow(non_camel_case_types)]
type io_object_t = u32;
#[allow(non_camel_case_types)]
type io_service_t = u32;
/// Opaque `IONotificationPortRef`.
type IONotificationPortRef = *mut c_void;
/// The `IOServiceInterestCallback` IOKit invokes on a power message.
type IOServiceInterestCallback = unsafe extern "C" fn(
    refcon: *mut c_void,
    service: io_service_t,
    message_type: u32,
    message_argument: *mut c_void,
);

// IOKit sleep/wake C API (IOKit.framework). Declared directly (like the
// IOPowerSources reader above). A redundant `#[link]` (harmless, deduped by
// the linker) keeps this block self-contained.
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        the_port_ref: *mut IONotificationPortRef,
        callback: IOServiceInterestCallback,
        notifier: *mut io_object_t,
    ) -> io_connect_t;
    fn IODeregisterForSystemPower(notifier: *mut io_object_t) -> i32;
    fn IONotificationPortGetRunLoopSource(notify: IONotificationPortRef) -> CFRunLoopSourceRef;
    fn IONotificationPortDestroy(notify: IONotificationPortRef);
    fn IOAllowPowerChange(kernel_port: io_connect_t, notification_id: isize) -> i32;
    fn IOServiceClose(connect: io_connect_t) -> i32;
}

// IOKit IOPowerSources API. These are plain CoreFoundation-returning C
// functions (not Objective-C), so they are declared directly rather than
// bridged via objc2. All return +1-retained CF objects we must release;
// `core-foundation`'s `wrap_under_create_rule` takes that ownership.
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRefRaw;
    fn IOPSCopyPowerSourcesList(blob: CFTypeRefRaw) -> CFArrayRef;
    fn IOPSGetPowerSourceDescription(blob: CFTypeRefRaw, ps: CFTypeRefRaw) -> CFDictionaryRef;
}

/// Opaque CoreFoundation blob pointer returned / consumed by the IOPS C API.
type CFTypeRefRaw = *const c_void;

/// Real macOS power source. Construct with [`RealPowerSource::new`]; the app
/// spawns the polling loop via [`RealPowerSource::spawn_poller`].
pub struct RealPowerSource {
    latest: Arc<Mutex<PowerState>>,
    tx: broadcast::Sender<PowerState>,
    /// Live `NWPathMonitor` caching the active path's metered proxy
    /// (`isExpensive` / `isConstrained`). Cloneable (shares an `Arc<AtomicU8>`)
    /// so the poll loop reads the latest verdict cheaply each tick.
    metered: MacosMeteredMonitor,
    /// Broadcasts OS sleep/wake edges (DESIGN s5.10.1). The monitor started by
    /// [`Self::spawn_sleep_wake_monitor`] sends on a clone of this; subscribers
    /// read via [`PowerSource::subscribe_sleep_wake`].
    sleep_wake_tx: broadcast::Sender<SleepWakeEvent>,
}

impl RealPowerSource {
    /// Builds the source with an initial snapshot read synchronously from
    /// IOKit. Never fails: a host with no battery / unreadable IOPS info
    /// resolves to "on AC, no battery".
    ///
    /// Starts the `NWPathMonitor` here; its first path arrives asynchronously,
    /// so the initial snapshot may read metered as [`MeteredStatus::Unknown`]
    /// (not metered) until the first update lands - a safe default that never
    /// wrongly stalls sync.
    pub fn new() -> anyhow::Result<Self> {
        let metered = MacosMeteredMonitor::start();
        let initial = read_power_state(metered.status());
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let (sleep_wake_tx, _sw_rx) = broadcast::channel(SLEEP_WAKE_CAPACITY);
        Ok(Self {
            latest: Arc::new(Mutex::new(initial)),
            tx,
            metered,
            sleep_wake_tx,
        })
    }

    /// Spawns the 30 s polling loop, broadcasting on change.
    ///
    /// The sleep/wake EDGE seam (DESIGN s5.10.1) is separate; start it via
    /// [`Self::spawn_sleep_wake_monitor`].
    pub fn spawn_poller(&self) -> tokio::task::JoinHandle<()> {
        let latest = Arc::clone(&self.latest);
        let tx = self.tx.clone();
        let metered = self.metered.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(POLL_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let next = read_power_state(metered.status());
                let mut guard = latest.lock().await;
                if *guard != next {
                    tracing::debug!(?next, "power state transition");
                    *guard = next.clone();
                    let _ = tx.send(next);
                }
            }
        })
    }

    /// Starts the OS sleep/wake monitor (DESIGN s5.10.1, issue #33).
    ///
    /// Spawns a dedicated OS thread that registers `IORegisterForSystemPower`,
    /// adds the notification port's run-loop source to that thread's own
    /// `CFRunLoop`, and runs it. IOKit then invokes [`system_power_callback`]
    /// on suspend / resume; the callback broadcasts the mapped
    /// [`SleepWakeEvent`] on a clone of this source's `sleep_wake_tx` (and acks
    /// the sleep so the system never waits on us).
    ///
    /// The returned [`SleepWakeMonitor`] stops the `CFRunLoop` and joins the
    /// thread (which then deregisters + destroys the IOKit port) on drop /
    /// [`SleepWakeMonitor::stop`], so a clean quit leaves no orphaned thread or
    /// IOKit registration. Errors if `IORegisterForSystemPower` fails.
    pub fn spawn_sleep_wake_monitor(&self) -> anyhow::Result<SleepWakeMonitor> {
        let tx = self.sleep_wake_tx.clone();
        // The thread hands its CFRunLoopRef back once set up (or `None` on a
        // registration failure) so this fn can build the teardown / report the
        // error synchronously.
        let (ready_tx, ready_rx) = mpsc::channel::<Option<SendCfRunLoop>>();
        let join = std::thread::Builder::new()
            .name("driven-sleepwake".to_string())
            .spawn(move || sleep_wake_thread(tx, &ready_tx))
            .map_err(|e| anyhow::anyhow!("failed to spawn sleep/wake thread: {e}"))?;

        match ready_rx.recv() {
            Ok(Some(run_loop)) => {
                let teardown = SystemPowerTeardown { run_loop, join };
                Ok(SleepWakeMonitor::new(move || teardown.run()))
            }
            // Registration failed inside the thread, or it panicked before
            // sending: join it and surface the error (degrade to the 30 s poll).
            Ok(None) | Err(_) => {
                let _ = join.join();
                anyhow::bail!("IORegisterForSystemPower failed");
            }
        }
    }
}

/// The macOS sleep/wake teardown: the dedicated thread's run loop plus its
/// join handle. A `Send` newtype (the `CFRunLoopRef` is only used to stop the
/// loop from another thread, which `CFRunLoopStop` documents as safe).
struct SystemPowerTeardown {
    run_loop: SendCfRunLoop,
    join: std::thread::JoinHandle<()>,
}

impl SystemPowerTeardown {
    /// Stop the dedicated run loop and join its thread (which then deregisters
    /// the IOKit port). Runs once.
    fn run(self) {
        // SAFETY: `CFRunLoopStop` is documented safe to call on another
        // thread's run loop; the ref came from that thread's `CFRunLoopGetCurrent`.
        unsafe { CFRunLoopStop(self.run_loop.0) };
        let _ = self.join.join();
    }
}

/// A `Send` wrapper for a `CFRunLoopRef` so it can travel to the teardown.
struct SendCfRunLoop(CFRunLoopRef);
// SAFETY: the ref is only used with `CFRunLoopStop` (thread-safe by design).
unsafe impl Send for SendCfRunLoop {}

/// The callback context: the broadcast sender plus the IOKit root port (needed
/// to ack sleep). `root_port` is filled in AFTER `IORegisterForSystemPower`
/// returns; the callback cannot fire until the run loop runs (after we store
/// it), and it is on the same thread, so an [`AtomicU32`] is ample ordering.
struct SystemPowerCtx {
    tx: broadcast::Sender<SleepWakeEvent>,
    root_port: AtomicU32,
}

/// The dedicated-thread body: register for system power, wire the run-loop
/// source, hand the run loop back via `ready`, then run the loop until stopped.
/// On any setup failure it sends `None` and returns (degrading to the poll).
fn sleep_wake_thread(
    tx: broadcast::Sender<SleepWakeEvent>,
    ready: &mpsc::Sender<Option<SendCfRunLoop>>,
) {
    // Leak the context box; reclaimed at the end of this fn after the loop
    // stops and we have deregistered (so no callback can run against a freed box).
    let ctx: *mut SystemPowerCtx = Box::into_raw(Box::new(SystemPowerCtx {
        tx,
        root_port: AtomicU32::new(MACH_PORT_NULL),
    }));

    let mut notify_port: IONotificationPortRef = std::ptr::null_mut();
    let mut notifier: io_object_t = 0;

    // SAFETY: standard IORegisterForSystemPower call; `ctx` is the refcon the
    // callback reads, valid until we free it at the end of this fn.
    let root_port = unsafe {
        IORegisterForSystemPower(
            ctx as *mut c_void,
            &mut notify_port,
            system_power_callback,
            &mut notifier,
        )
    };
    if root_port == MACH_PORT_NULL || notify_port.is_null() {
        // Registration failed: reclaim the box, report failure, and exit.
        // SAFETY: no callback was registered, so `ctx` is untouched elsewhere.
        unsafe { drop(Box::from_raw(ctx)) };
        let _ = ready.send(None);
        return;
    }
    // Publish the root port so the callback can ack sleeps.
    // SAFETY: `ctx` is a live box we alone own here.
    unsafe { (*ctx).root_port.store(root_port, Ordering::SeqCst) };

    // SAFETY: add the port's run-loop source to THIS thread's run loop.
    let run_loop = unsafe {
        let source = IONotificationPortGetRunLoopSource(notify_port);
        let rl = CFRunLoopGetCurrent();
        CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
        rl
    };

    // Hand the run loop back so the monitor can stop it later. If the receiver
    // is gone (the spawner errored), tear down immediately.
    if ready.send(Some(SendCfRunLoop(run_loop))).is_err() {
        unsafe {
            let _ = IODeregisterForSystemPower(&mut notifier);
            IOServiceClose(root_port);
            IONotificationPortDestroy(notify_port);
            drop(Box::from_raw(ctx));
        }
        return;
    }

    // Run until the monitor teardown calls `CFRunLoopStop`.
    // SAFETY: blocks this dedicated thread; wakes on stop.
    unsafe { CFRunLoopRun() };

    // Loop stopped -> tear the IOKit registration down and reclaim the box.
    // SAFETY: after the loop stops, no further callbacks run; deregister the
    // notifier, close the root port, destroy the port, and free the context.
    unsafe {
        let _ = IODeregisterForSystemPower(&mut notifier);
        IOServiceClose(root_port);
        IONotificationPortDestroy(notify_port);
        drop(Box::from_raw(ctx));
    }
}

/// Maps an IOKit system-power message to a [`SleepWakeEvent`], or `None` for
/// messages that are not suspend/resume edges (including
/// `kIOMessageCanSystemSleep`, which we merely ack). Pure, so it is unit-tested
/// without a real suspend.
fn sleep_wake_from_io_message(message_type: u32) -> Option<SleepWakeEvent> {
    match message_type {
        K_IO_MESSAGE_SYSTEM_WILL_SLEEP => Some(SleepWakeEvent::Suspending),
        K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON => Some(SleepWakeEvent::Resumed),
        _ => None,
    }
}

/// The `IOServiceInterestCallback` IOKit invokes on a power message. Broadcasts
/// the mapped edge and ALWAYS acks a sleep query / will-sleep (via
/// `IOAllowPowerChange`) so the system never blocks 30 s waiting on us.
///
/// SAFETY / robustness: runs on the dedicated run-loop thread. It reads the
/// context (live until the loop stops), does a non-blocking `broadcast::send`,
/// and acks - never panicking across the FFI boundary or blocking the power path.
unsafe extern "C" fn system_power_callback(
    refcon: *mut c_void,
    _service: io_service_t,
    message_type: u32,
    message_argument: *mut c_void,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: `refcon` is the `SystemPowerCtx` pointer registered above; it is
    // live until the run loop stops (this callback only runs while it runs).
    let ctx = unsafe { &*(refcon as *const SystemPowerCtx) };

    if let Some(event) = sleep_wake_from_io_message(message_type) {
        // A send error only means no live subscribers - benign.
        let _ = ctx.tx.send(event);
    }

    // Acknowledge the power change so the system proceeds without waiting out
    // its timeout. Required for CanSystemSleep (allow idle sleep) and
    // WillSleep (proceed to sleep). Other messages need no ack.
    if message_type == K_IO_MESSAGE_CAN_SYSTEM_SLEEP
        || message_type == K_IO_MESSAGE_SYSTEM_WILL_SLEEP
    {
        let root_port = ctx.root_port.load(Ordering::SeqCst);
        if root_port != MACH_PORT_NULL {
            // SAFETY: `root_port` is the connection from IORegisterForSystemPower;
            // `message_argument` is the notification id IOKit passed us.
            unsafe { IOAllowPowerChange(root_port, message_argument as isize) };
        }
    }
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

/// Reads a full [`PowerState`] snapshot. AC / battery come from IOKit; the
/// `metered` verdict is the caller-supplied cached `NWPathMonitor` reading.
fn read_power_state(metered: MeteredStatus) -> PowerState {
    let (ac_connected, battery_percent) = read_ac_and_battery();
    PowerState {
        ac_connected,
        battery_percent,
        on_metered_network: metered.on_metered(),
        network_reachable: reachable_hint(),
    }
}

/// IOPS description-dictionary keys (stable public constants).
const KEY_POWER_SOURCE_STATE: &str = "Power Source State";
const KEY_CURRENT_CAPACITY: &str = "Current Capacity";
const KEY_MAX_CAPACITY: &str = "Max Capacity";
const VALUE_AC_POWER: &str = "AC Power";

/// Walks the IOPowerSources list and derives `(ac_connected,
/// battery_percent)`. A host with no power sources (typical desktop)
/// resolves to `(true, None)`.
fn read_ac_and_battery() -> (bool, Option<u8>) {
    // SAFETY: `IOPSCopyPowerSourcesInfo` returns a +1 CF blob (or null);
    // `wrap_under_create_rule` takes ownership and releases on drop.
    let blob_raw = unsafe { IOPSCopyPowerSourcesInfo() };
    if blob_raw.is_null() {
        return (true, None);
    }
    // Keep the blob alive for the whole read; release at end of scope.
    let blob = unsafe { CFType::wrap_under_create_rule(blob_raw as _) };
    let blob_ptr = blob.as_CFTypeRef() as CFTypeRefRaw;

    // SAFETY: list is a +1 CFArray of power-source CF objects, or null.
    let list_ref = unsafe { IOPSCopyPowerSourcesList(blob_ptr) };
    if list_ref.is_null() {
        return (true, None);
    }
    let list: CFArray<CFType> = unsafe { CFArray::wrap_under_create_rule(list_ref) };

    let mut any_battery = false;
    let mut on_ac = true;
    let mut percent: Option<u8> = None;

    for ps in list.iter() {
        let ps_ptr = ps.as_CFTypeRef() as CFTypeRefRaw;
        // SAFETY: returns a borrowed (non-owned) description dict or null.
        let desc_ref = unsafe { IOPSGetPowerSourceDescription(blob_ptr, ps_ptr) };
        if desc_ref.is_null() {
            continue;
        }
        let desc: CFDictionary<CFString, CFType> =
            unsafe { CFDictionary::wrap_under_get_rule(desc_ref) };

        any_battery = true;

        if let Some(state) = dict_string(&desc, KEY_POWER_SOURCE_STATE) {
            on_ac = state == VALUE_AC_POWER;
        }

        let cur = dict_i64(&desc, KEY_CURRENT_CAPACITY);
        let max = dict_i64(&desc, KEY_MAX_CAPACITY);
        if let (Some(cur), Some(max)) = (cur, max) {
            if max > 0 {
                let pct = ((cur as f64 / max as f64) * 100.0).round();
                percent = Some(pct.clamp(0.0, 100.0) as u8);
            }
        }
    }

    if any_battery {
        (on_ac, percent)
    } else {
        // No internal battery (desktop / Mac mini): treat as on AC.
        (true, None)
    }
}

/// Reads a `CFString` value from `dict` by key, as a Rust `String`.
fn dict_string(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<String> {
    let k = CFString::new(key);
    // `find` yields a borrowed `ItemRef<CFType>`; clone to an owned `CFType`
    // so `downcast` (which consumes `self`) can run.
    let v: CFType = (*dict.find(&k)?).clone();
    let s = v.downcast::<CFString>()?;
    Some(s.to_string())
}

/// Reads a `CFNumber` value from `dict` by key, as an `i64`.
fn dict_i64(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<i64> {
    let k = CFString::new(key);
    let v: CFType = (*dict.find(&k)?).clone();
    let n = v.downcast::<CFNumber>()?;
    n.to_i64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_plausible_power_state_on_this_host() {
        let (_ac, battery) = read_ac_and_battery();
        if let Some(pct) = battery {
            assert!(pct <= 100, "battery percent in range: {pct}");
        }
    }

    #[tokio::test]
    async fn current_returns_plausible_state() {
        let src = RealPowerSource::new().expect("construct RealPowerSource");
        let state = src.current().await;
        if let Some(pct) = state.battery_percent {
            assert!(pct <= 100);
        }
    }

    /// The pure IOKit-message -> [`SleepWakeEvent`] mapping (the heart of the
    /// callback), tested without a real suspend: will-sleep maps to
    /// `Suspending`, has-powered-on to `Resumed`, and the can-sleep query (a
    /// mere ack) plus any other code map to `None`.
    #[test]
    fn io_message_maps_to_sleep_wake_events() {
        assert_eq!(
            sleep_wake_from_io_message(K_IO_MESSAGE_SYSTEM_WILL_SLEEP),
            Some(SleepWakeEvent::Suspending)
        );
        assert_eq!(
            sleep_wake_from_io_message(K_IO_MESSAGE_SYSTEM_HAS_POWERED_ON),
            Some(SleepWakeEvent::Resumed)
        );
        // CanSystemSleep is only an ack point, not a suspend edge.
        assert_eq!(
            sleep_wake_from_io_message(K_IO_MESSAGE_CAN_SYSTEM_SLEEP),
            None
        );
        assert_eq!(sleep_wake_from_io_message(0), None);
    }

    /// A `SleepWakeEvent` sent on the source's own sleep/wake channel reaches a
    /// `subscribe_sleep_wake` subscriber - the plumbing the callback drives.
    #[tokio::test]
    async fn subscribe_sleep_wake_delivers_edges() {
        let src = RealPowerSource::new().expect("construct RealPowerSource");
        let mut rx = src.subscribe_sleep_wake();
        let _ = src.sleep_wake_tx.send(SleepWakeEvent::Suspending);
        assert_eq!(rx.recv().await.unwrap(), SleepWakeEvent::Suspending);
    }
}
