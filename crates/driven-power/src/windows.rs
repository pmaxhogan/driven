//! Windows [`PowerSource`] backend (DESIGN s5.7, s5.10.1).
//!
//! AC / battery state comes from `GetSystemPowerStatus`
//! (`Win32::System::Power`). Metered-network detection is delegated to
//! [`crate::network`]. The source polls every 30 s (DESIGN s5.7) and
//! broadcasts a fresh [`PowerState`] whenever any field changes; consumers
//! subscribe via [`PowerSource::subscribe`].
//!
//! Sleep / wake (DESIGN s5.10.1: `WM_POWERBROADCAST` `PBT_APMSUSPEND` /
//! `PBT_APMRESUMEAUTOMATIC`) is intentionally *not* wired here. It requires
//! a hidden message-pump window owned by the tray thread, which is a large
//! per-OS hook; the orchestrator consumes `PowerEvent`s on the same channel
//! as power-source events and exercises that path against
//! `FakePowerSource` in tests. See the TODO in [`RealPowerSource`].

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::sync::Mutex;

use windows::Win32::System::Power::GetSystemPowerStatus;
use windows::Win32::System::Power::SYSTEM_POWER_STATUS;

use crate::network::detect_metered_and_reachable;
use crate::{PowerSource, PowerState};
use async_trait::async_trait;

/// Poll cadence for the OS power query (DESIGN s5.7: "polled at 30s ...
/// as a fallback").
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Capacity of the broadcast channel. Transitions are rare (AC plug /
/// unplug, metered toggle), so a small buffer is ample; a lagging consumer
/// re-reads [`PowerSource::current`] per the trait contract.
const BROADCAST_CAPACITY: usize = 16;

/// `SYSTEM_POWER_STATUS::ACLineStatus` sentinel for "AC online".
const AC_LINE_ONLINE: u8 = 1;

/// `SYSTEM_POWER_STATUS::BatteryLifePercent` sentinel for "unknown".
const BATTERY_PERCENT_UNKNOWN: u8 = 255;

/// Real Windows power source. Construct with [`RealPowerSource::new`]; the
/// app spawns the polling loop via [`RealPowerSource::spawn_poller`].
pub struct RealPowerSource {
    latest: Arc<Mutex<PowerState>>,
    tx: broadcast::Sender<PowerState>,
}

impl RealPowerSource {
    /// Builds the source with an initial snapshot read synchronously from
    /// the OS. Returns an error if the very first `GetSystemPowerStatus`
    /// call fails (a genuinely broken host); transient later failures keep
    /// the last good snapshot.
    pub fn new() -> anyhow::Result<Self> {
        let initial = read_power_state()?;
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Ok(Self {
            latest: Arc::new(Mutex::new(initial)),
            tx,
        })
    }

    /// Spawns the 30 s polling loop. Each tick re-reads the OS power status
    /// plus metered/reachability and broadcasts a new [`PowerState`] only on
    /// change. The returned [`tokio::task::JoinHandle`] is owned by the app
    /// so it can abort the loop on shutdown.
    ///
    /// TODO(DESIGN s5.10.1): add the `WM_POWERBROADCAST` hidden-window
    /// message pump to emit `Suspending` / `Resumed` events. Until then the
    /// orchestrator relies on the 30 s poll plus network re-probe on wake.
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

/// Reads a full [`PowerState`] snapshot: AC / battery from
/// `GetSystemPowerStatus`, metered / reachability from [`crate::network`].
fn read_power_state() -> anyhow::Result<PowerState> {
    let (ac_connected, battery_percent) = read_ac_and_battery()?;
    let (on_metered_network, network_reachable) = detect_metered_and_reachable();
    Ok(PowerState {
        ac_connected,
        battery_percent,
        on_metered_network,
        network_reachable,
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
}
