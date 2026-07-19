//! Linux [`PowerSource`] backend (DESIGN s5.7, s5.10.1).
//!
//! AC / battery state is read from sysfs (`/sys/class/power_supply/*`):
//! `type == "Mains"` entries give AC online via their `online` file;
//! `type == "Battery"` entries give charge via `capacity`. Metered-network
//! detection is delegated to [`crate::network`]. The source polls every
//! 30 s (DESIGN s5.7) and broadcasts a fresh [`PowerState`] on change.
//!
//! Sleep / wake (DESIGN s5.10.1) IS wired here (issue #33) via the
//! systemd-logind `org.freedesktop.login1.Manager.PrepareForSleep(bool)` DBus
//! signal, subscribed through a `zbus` `#[proxy]`-generated signal stream on a
//! spawned task. `PrepareForSleep(true)` is the suspend edge, `(false)` the
//! resume edge; the task maps each to a [`SleepWakeEvent`] and broadcasts it.
//! The orchestrator subscribes via [`PowerSource::subscribe_sleep_wake`] and
//! runs the s5.10.2 / s5.10.3 sequences at the edge instead of waiting for the
//! 30 s poll. This backend is COMPILE-VERIFIED via CI's Linux job (the
//! implementer builds on Windows); it is written against the documented logind
//! interface + `zbus` signal-stream API.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::broadcast;
use tokio::sync::Mutex;

use crate::network::{detect_metered_blocking, reachable_hint, MeteredStatus};
use crate::{PowerSource, PowerState, SleepWakeEvent, SleepWakeMonitor};

/// Poll cadence for the sysfs power read (DESIGN s5.7).
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Broadcast channel capacity (transitions are rare).
const BROADCAST_CAPACITY: usize = 16;

/// Upper bound on the per-poll NetworkManager D-Bus read. It runs on a blocking
/// thread (via [`tokio::task::spawn_blocking`]) so it never parks a tokio
/// worker, but this cap guarantees a wedged system bus can never stall the
/// AC/battery poll for more than this: on timeout the metered value falls back
/// to [`MeteredStatus::Unknown`] (not metered) and the poll proceeds.
const METERED_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Capacity of the sleep/wake edge broadcast channel. Edges are extremely
/// rare (one per real suspend / resume), so a tiny buffer is ample.
const SLEEP_WAKE_CAPACITY: usize = 8;

/// The systemd-logind `Manager` DBus interface. `zbus`'s `#[proxy]` generates
/// `LogindManagerProxy` (with `receive_prepare_for_sleep()` -> a signal
/// `Stream`) and the `PrepareForSleepArgs` struct carrying the `start` bool.
#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait LogindManager {
    /// Emitted with `start = true` right before sleep/hibernate and
    /// `start = false` right after resume (logind `PrepareForSleep`).
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// sysfs root for power-supply devices. A constant so tests can reason
/// about the layout; the reader takes the root as a parameter.
const POWER_SUPPLY_ROOT: &str = "/sys/class/power_supply";

/// Real Linux power source. Construct with [`RealPowerSource::new`]; the app
/// spawns the polling loop via [`RealPowerSource::spawn_poller`].
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
    /// sysfs. Never fails: a host with no power-supply devices (server /
    /// container) resolves to "on AC, no battery".
    ///
    /// The initial snapshot reports metered as [`MeteredStatus::Unknown`] (not
    /// metered): the NetworkManager read is a blocking D-Bus call that cannot be
    /// awaited here, so the first real metered verdict lands on the first poll
    /// tick. The safe default in the meantime never wrongly stalls sync.
    pub fn new() -> anyhow::Result<Self> {
        let initial = read_power_state(MeteredStatus::Unknown);
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let (sleep_wake_tx, _sw_rx) = broadcast::channel(SLEEP_WAKE_CAPACITY);
        Ok(Self {
            latest: Arc::new(Mutex::new(initial)),
            tx,
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
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(POLL_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let metered = read_metered().await;
                let next = read_power_state(metered);
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
    /// Spawns a task that connects to the SYSTEM bus, subscribes to logind's
    /// `PrepareForSleep` signal, and broadcasts the mapped [`SleepWakeEvent`]
    /// on a clone of this source's `sleep_wake_tx` for each edge; subscribers
    /// read them via [`PowerSource::subscribe_sleep_wake`].
    ///
    /// Best-effort: if the system bus / logind is unavailable (e.g. a container
    /// without a session bus), the task logs and exits, and the app degrades to
    /// the 30 s poll - so this never returns an error. The returned
    /// [`SleepWakeMonitor`] aborts the task on drop / [`SleepWakeMonitor::stop`],
    /// leaving no orphaned task on a clean quit.
    ///
    /// Must be called from within a Tokio runtime (the app-shell does, exactly
    /// as it calls [`Self::spawn_poller`]).
    pub fn spawn_sleep_wake_monitor(&self) -> anyhow::Result<SleepWakeMonitor> {
        let tx = self.sleep_wake_tx.clone();
        let handle = tokio::spawn(run_sleep_wake_task(tx));
        Ok(SleepWakeMonitor::new(move || handle.abort()))
    }
}

/// Maps a logind `PrepareForSleep(start)` argument to a [`SleepWakeEvent`]:
/// `start = true` is the suspend edge, `false` the resume edge. Pure, so it is
/// unit-tested without a DBus bus.
fn sleep_wake_from_prepare(start: bool) -> SleepWakeEvent {
    if start {
        SleepWakeEvent::Suspending
    } else {
        SleepWakeEvent::Resumed
    }
}

/// The sleep/wake task body: subscribe to logind `PrepareForSleep` and
/// broadcast each mapped edge. Any setup error (no system bus, no logind) is
/// logged and ends the task - the 30 s poll remains the fallback.
async fn run_sleep_wake_task(tx: broadcast::Sender<SleepWakeEvent>) {
    use futures_util::stream::StreamExt;

    let conn = match zbus::Connection::system().await {
        Ok(c) => c,
        Err(err) => {
            tracing::warn!(%err, "sleep/wake: cannot reach the system DBus; relying on the 30s poll");
            return;
        }
    };
    let proxy = match LogindManagerProxy::new(&conn).await {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(%err, "sleep/wake: cannot build the logind proxy; relying on the 30s poll");
            return;
        }
    };
    let mut stream = match proxy.receive_prepare_for_sleep().await {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(%err, "sleep/wake: cannot subscribe to logind PrepareForSleep; relying on the 30s poll");
            return;
        }
    };

    tracing::debug!("sleep/wake: subscribed to logind PrepareForSleep");
    while let Some(signal) = stream.next().await {
        let start = match signal.args() {
            Ok(args) => args.start,
            Err(err) => {
                tracing::warn!(%err, "sleep/wake: malformed PrepareForSleep signal; ignoring");
                continue;
            }
        };
        let event = sleep_wake_from_prepare(start);
        tracing::debug!(?event, "sleep/wake: logind PrepareForSleep edge");
        // A send error only means no live subscribers - benign.
        let _ = tx.send(event);
    }
}

/// Reads NetworkManager's metered state off the async executor, bounded by
/// [`METERED_READ_TIMEOUT`]. The blocking D-Bus read runs on a
/// [`tokio::task::spawn_blocking`] thread (zbus's blocking API must not be
/// driven from within an async runtime); a join error or timeout resolves to
/// [`MeteredStatus::Unknown`] (not metered) so the AC/battery poll is never
/// blocked by a wedged bus.
async fn read_metered() -> MeteredStatus {
    let read = tokio::task::spawn_blocking(detect_metered_blocking);
    match tokio::time::timeout(METERED_READ_TIMEOUT, read).await {
        Ok(Ok(status)) => status,
        Ok(Err(join_err)) => {
            tracing::warn!(error = %join_err, "metered read task failed; treating as unknown");
            MeteredStatus::Unknown
        }
        Err(_elapsed) => {
            tracing::warn!("metered read timed out; treating as unknown");
            MeteredStatus::Unknown
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

/// Reads a full [`PowerState`] snapshot. AC / battery come from sysfs; the
/// `metered` verdict is supplied by the caller (read separately off the async
/// executor via [`read_metered`], or [`MeteredStatus::Unknown`] for the initial
/// synchronous snapshot).
fn read_power_state(metered: MeteredStatus) -> PowerState {
    let (ac_connected, battery_percent) = read_ac_and_battery(Path::new(POWER_SUPPLY_ROOT));
    PowerState {
        ac_connected,
        battery_percent,
        on_metered_network: metered.on_metered(),
        network_reachable: reachable_hint(),
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

    /// The pure logind `PrepareForSleep(start)` -> [`SleepWakeEvent`] mapping,
    /// tested without a DBus bus: `true` (about to sleep) -> `Suspending`,
    /// `false` (just woke) -> `Resumed`.
    #[test]
    fn prepare_for_sleep_maps_to_sleep_wake_events() {
        assert_eq!(sleep_wake_from_prepare(true), SleepWakeEvent::Suspending);
        assert_eq!(sleep_wake_from_prepare(false), SleepWakeEvent::Resumed);
    }

    /// A `SleepWakeEvent` sent on the source's own sleep/wake channel reaches a
    /// `subscribe_sleep_wake` subscriber - the plumbing the DBus task drives.
    #[tokio::test]
    async fn subscribe_sleep_wake_delivers_edges() {
        let src = RealPowerSource::new().expect("construct RealPowerSource");
        let mut rx = src.subscribe_sleep_wake();
        let _ = src.sleep_wake_tx.send(SleepWakeEvent::Resumed);
        assert_eq!(rx.recv().await.unwrap(), SleepWakeEvent::Resumed);
    }
}
