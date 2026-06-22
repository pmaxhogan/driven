//! macOS [`PowerSource`] backend (DESIGN s5.7, s5.10.1).
//!
//! AC / battery state comes from IOKit's IOPowerSources API
//! (`IOPSCopyPowerSourcesInfo` + `IOPSCopyPowerSourcesList`, reading each
//! source's description dictionary). Metered-network detection is delegated
//! to [`crate::network`]. The source polls every 30 s (DESIGN s5.7) and
//! broadcasts a fresh [`PowerState`] whenever any field changes.
//!
//! Sleep / wake (DESIGN s5.10.1: `NSWorkspaceWillSleepNotification` /
//! `NSWorkspaceDidWakeNotification`) is intentionally *not* wired here. It
//! requires bridging `NSWorkspace.shared.notificationCenter` observers via
//! `objc2` from a long-running task - a sizeable per-OS hook. The
//! orchestrator consumes those `PowerEvent`s on the same channel as
//! power-source events and exercises that path against `FakePowerSource` in
//! tests. See the TODO in [`RealPowerSource`].

use std::ffi::c_void;
use std::sync::Arc;
use std::time::Duration;

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;

use async_trait::async_trait;
use tokio::sync::broadcast;
use tokio::sync::Mutex;

use crate::network::detect_metered_and_reachable;
use crate::{PowerSource, PowerState};

/// Poll cadence for the OS power query (DESIGN s5.7).
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Broadcast channel capacity (transitions are rare).
const BROADCAST_CAPACITY: usize = 16;

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
}

impl RealPowerSource {
    /// Builds the source with an initial snapshot read synchronously from
    /// IOKit. Never fails: a host with no battery / unreadable IOPS info
    /// resolves to "on AC, no battery".
    pub fn new() -> anyhow::Result<Self> {
        let initial = read_power_state();
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Ok(Self {
            latest: Arc::new(Mutex::new(initial)),
            tx,
        })
    }

    /// Spawns the 30 s polling loop, broadcasting on change.
    ///
    /// TODO(DESIGN s5.10.1): bridge `NSWorkspace` sleep/wake notifications
    /// via `objc2` to emit `Suspending` / `Resumed`. Until then the
    /// orchestrator relies on the 30 s poll plus network re-probe on wake.
    pub fn spawn_poller(&self) -> tokio::task::JoinHandle<()> {
        let latest = Arc::clone(&self.latest);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(POLL_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let next = read_power_state();
                let mut guard = latest.lock().await;
                if *guard != next {
                    tracing::debug!(?next, "power state transition");
                    *guard = next.clone();
                    let _ = tx.send(next);
                }
            }
        })
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
}

/// Reads a full [`PowerState`] snapshot.
fn read_power_state() -> PowerState {
    let (ac_connected, battery_percent) = read_ac_and_battery();
    let (on_metered_network, network_reachable) = detect_metered_and_reachable();
    PowerState {
        ac_connected,
        battery_percent,
        on_metered_network,
        network_reachable,
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
}
