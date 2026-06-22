//! Linux [`PowerSource`] backend (DESIGN s5.7, s5.10.1).
//!
//! AC / battery state is read from sysfs (`/sys/class/power_supply/*`):
//! `type == "Mains"` entries give AC online via their `online` file;
//! `type == "Battery"` entries give charge via `capacity`. Metered-network
//! detection is delegated to [`crate::network`]. The source polls every
//! 30 s (DESIGN s5.7) and broadcasts a fresh [`PowerState`] on change.
//!
//! Sleep / wake (DESIGN s5.10.1: the systemd-logind
//! `org.freedesktop.login1.Manager.PrepareForSleep(bool)` DBus signal via
//! `zbus`) is intentionally *not* wired here. Subscribing to that signal
//! from a long-running task is a sizeable per-OS hook; the orchestrator
//! consumes the resulting `PowerEvent`s on the same channel as
//! power-source events and exercises that path against `FakePowerSource` in
//! tests. See the TODO in [`RealPowerSource`].

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::broadcast;
use tokio::sync::Mutex;

use crate::network::detect_metered_and_reachable;
use crate::{PowerSource, PowerState};

/// Poll cadence for the sysfs power read (DESIGN s5.7).
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Broadcast channel capacity (transitions are rare).
const BROADCAST_CAPACITY: usize = 16;

/// sysfs root for power-supply devices. A constant so tests can reason
/// about the layout; the reader takes the root as a parameter.
const POWER_SUPPLY_ROOT: &str = "/sys/class/power_supply";

/// Real Linux power source. Construct with [`RealPowerSource::new`]; the app
/// spawns the polling loop via [`RealPowerSource::spawn_poller`].
pub struct RealPowerSource {
    latest: Arc<Mutex<PowerState>>,
    tx: broadcast::Sender<PowerState>,
}

impl RealPowerSource {
    /// Builds the source with an initial snapshot read synchronously from
    /// sysfs. Never fails: a host with no power-supply devices (server /
    /// container) resolves to "on AC, no battery".
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
    /// TODO(DESIGN s5.10.1): subscribe to the logind `PrepareForSleep` DBus
    /// signal via `zbus` to emit `Suspending` / `Resumed`. Until then the
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
    let (ac_connected, battery_percent) = read_ac_and_battery(Path::new(POWER_SUPPLY_ROOT));
    let (on_metered_network, network_reachable) = detect_metered_and_reachable();
    PowerState {
        ac_connected,
        battery_percent,
        on_metered_network,
        network_reachable,
    }
}

/// Scans `root` (`/sys/class/power_supply`) and derives `(ac_connected,
/// battery_percent)`.
///
/// - Any `type == "Mains"` device with `online == 1` -> on AC.
/// - The first `type == "Battery"` device's `capacity` -> battery percent.
/// - No Mains device at all -> assume on AC (so a battery-only kernel quirk
///   never spuriously pauses on the AC gate).
/// - No power-supply devices -> `(true, None)` (server / container).
fn read_ac_and_battery(root: &Path) -> (bool, Option<u8>) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return (true, None),
    };

    let mut saw_mains = false;
    let mut mains_online = false;
    let mut battery_percent: Option<u8> = None;

    for entry in entries.flatten() {
        let dir: PathBuf = entry.path();
        let kind = read_trimmed(&dir.join("type")).unwrap_or_default();
        match kind.as_str() {
            "Mains" => {
                saw_mains = true;
                if read_trimmed(&dir.join("online")).as_deref() == Some("1") {
                    mains_online = true;
                }
            }
            "Battery" => {
                // First battery's capacity wins; ignore later batteries.
                if let (None, Some(cap)) = (
                    battery_percent,
                    read_trimmed(&dir.join("capacity")).and_then(|s| s.parse::<u32>().ok()),
                ) {
                    battery_percent = Some(cap.min(100) as u8);
                }
            }
            _ => {}
        }
    }

    let ac_connected = if saw_mains { mains_online } else { true };
    (ac_connected, battery_percent)
}

/// Reads a sysfs file and returns its trimmed contents, or `None` if the
/// file is absent / unreadable.
fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_plausible_power_state_on_this_host() {
        let (_ac, battery) = read_ac_and_battery(Path::new(POWER_SUPPLY_ROOT));
        if let Some(pct) = battery {
            assert!(pct <= 100, "battery percent in range: {pct}");
        }
    }

    #[test]
    fn synthetic_mains_and_battery_dirs_parse() {
        let tmp = std::env::temp_dir().join(format!(
            "driven-power-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let ac0 = tmp.join("AC");
        let bat0 = tmp.join("BAT0");
        std::fs::create_dir_all(&ac0).expect("mk AC dir");
        std::fs::create_dir_all(&bat0).expect("mk BAT0 dir");
        std::fs::write(ac0.join("type"), "Mains\n").expect("write type");
        std::fs::write(ac0.join("online"), "0\n").expect("write online");
        std::fs::write(bat0.join("type"), "Battery\n").expect("write type");
        std::fs::write(bat0.join("capacity"), "73\n").expect("write capacity");

        let (ac, battery) = read_ac_and_battery(&tmp);
        assert!(!ac, "AC offline when online==0");
        assert_eq!(battery, Some(73));

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn missing_root_resolves_to_on_ac_no_battery() {
        let (ac, battery) = read_ac_and_battery(Path::new("/nonexistent/driven/power"));
        assert!(ac);
        assert_eq!(battery, None);
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
