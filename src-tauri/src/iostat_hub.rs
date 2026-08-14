//! Live disk / network throughput sampling behind the Activity dashboard's
//! split graphs (2026-08-14 follow-up).
//!
//! Every account's executor credits the ONE app-global
//! [`IoCounters`](driven_core::iostat::IoCounters) (disk bytes read for
//! backup work; wire bytes the destination acked). The sampler task here
//! diffs those cumulative counters once a second into per-second deltas,
//! keeps a trailing ring for the dashboard's initial load, and pushes each
//! non-idle sample to the webview as a live event - the graphs move in real
//! time, INCLUDING during the reconcile-phase resume that used to be
//! invisible on every throughput surface.
//!
//! Unlike `memlog`'s deliberately-detached watchdog, this task holds an
//! `AppHandle` and emits events, so it follows the updater/telemetry
//! lifecycle pattern: tracked on [`AppState`] with a shutdown signal and
//! joined by the quit drain (R3-P1-1 no-orphan rule).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use driven_core::iostat::{IoCounters, IoSnapshot};
use driven_core::time::{Clock, SystemClock};
use serde::Serialize;
use tauri::AppHandle;

const TARGET: &str = "driven::app::iostat";

/// Sampling cadence. One second gives the graphs real-time motion while the
/// per-tick work is two atomic loads and (at most) one small event.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Ring capacity: 5 minutes of 1s samples, matching the Activity
/// dashboard's existing 5-minute sparkline window.
const RING_CAP: usize = 300;

/// One per-interval throughput sample (DELTAS, not cumulative totals).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IoSample {
    /// Wall-clock ms of the sample (end of its interval).
    pub ts_ms: i64,
    /// Plaintext bytes Driven read from local files during the interval.
    pub disk_bytes: u64,
    /// Wire bytes the destination acked during the interval.
    pub net_bytes: u64,
}

/// The wire shape of the `io_throughput_series` command reply.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IoThroughputSeriesDto {
    /// Interval both the ring and the live events use, in ms.
    pub bucket_ms: u64,
    /// Trailing samples, oldest first.
    pub samples: Vec<IoSample>,
}

/// The app-global counters + trailing sample ring.
#[derive(Debug)]
pub struct IoStatHub {
    counters: Arc<IoCounters>,
    samples: Mutex<VecDeque<IoSample>>,
}

impl Default for IoStatHub {
    fn default() -> Self {
        Self {
            counters: Arc::new(IoCounters::default()),
            samples: Mutex::new(VecDeque::with_capacity(RING_CAP)),
        }
    }
}

impl IoStatHub {
    /// The shared counters every executor credits.
    pub fn counters(&self) -> Arc<IoCounters> {
        self.counters.clone()
    }

    /// The trailing window for the dashboard's initial load.
    pub fn series(&self) -> IoThroughputSeriesDto {
        IoThroughputSeriesDto {
            bucket_ms: SAMPLE_INTERVAL.as_millis() as u64,
            samples: self
                .samples
                .lock()
                .map(|q| q.iter().copied().collect())
                .unwrap_or_default(),
        }
    }

    fn push(&self, sample: IoSample) {
        if let Ok(mut q) = self.samples.lock() {
            if q.len() >= RING_CAP {
                q.pop_front();
            }
            q.push_back(sample);
        }
    }
}

/// One sampler tick: diff the cumulative counters against `prev`, record the
/// delta sample, and say whether it should be PUSHED to the webview.
///
/// Idle suppression: a zero-delta sample is recorded in the ring (the graph
/// must show quiet gaps as zero) but only EMITTED when the previous emitted
/// sample was non-zero - one trailing zero lets the UI decay the live line to
/// the floor, and then a fully idle app pushes no further events (instead of
/// ~86k no-op events/day through the bridge).
pub fn tick(
    hub: &IoStatHub,
    prev: &mut IoSnapshot,
    last_emitted_was_zero: &mut bool,
    now_ms: i64,
) -> Option<IoSample> {
    let cur = hub.counters().snapshot();
    let sample = IoSample {
        ts_ms: now_ms,
        disk_bytes: cur.disk_read_bytes.saturating_sub(prev.disk_read_bytes),
        net_bytes: cur.net_wire_bytes.saturating_sub(prev.net_wire_bytes),
    };
    *prev = cur;
    hub.push(sample);

    let is_zero = sample.disk_bytes == 0 && sample.net_bytes == 0;
    let emit = !is_zero || !*last_emitted_was_zero;
    if emit {
        *last_emitted_was_zero = is_zero;
        Some(sample)
    } else {
        None
    }
}

/// Spawn the sampling loop (updater/telemetry lifecycle pattern). Called once
/// from setup after `AppState` is managed.
pub fn spawn_sampler(app: &AppHandle) {
    use tauri::Manager;
    let Some(state) = app.try_state::<crate::app_state::AppState>() else {
        tracing::warn!(target: TARGET, "AppState not managed; io throughput sampler not started");
        return;
    };
    let hub = state.iostat_hub();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let app_handle = app.clone();

    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut prev = hub.counters().snapshot();
        let mut last_emitted_was_zero = true;
        loop {
            tokio::select! {
                biased;
                res = shutdown_rx.changed() => {
                    match res {
                        Ok(()) if *shutdown_rx.borrow() => break,
                        Ok(()) => {}
                        Err(_) => break,
                    }
                }
                _ = ticker.tick() => {
                    let now_ms = SystemClock.now_ms();
                    if let Some(sample) =
                        tick(&hub, &mut prev, &mut last_emitted_was_zero, now_ms)
                    {
                        crate::events::emit_sync_io_throughput(&app_handle, sample);
                    }
                }
            }
        }
        tracing::debug!(target: TARGET, "io throughput sampler exited");
    });

    state.set_iostat_task(task, shutdown_tx);
    tracing::info!(
        target: TARGET,
        interval_ms = SAMPLE_INTERVAL.as_millis() as u64,
        "io throughput sampler started"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_diffs_records_and_suppresses_idle_runs() {
        let hub = IoStatHub::default();
        let mut prev = hub.counters().snapshot();
        let mut last_zero = true;

        // Work happened: delta sample, emitted.
        hub.counters().add_disk_read(1000);
        hub.counters().add_net_wire(400);
        let s = tick(&hub, &mut prev, &mut last_zero, 1).expect("non-zero emits");
        assert_eq!((s.disk_bytes, s.net_bytes), (1000, 400));

        // First idle tick: one trailing zero is emitted so the UI decays.
        let s = tick(&hub, &mut prev, &mut last_zero, 2).expect("trailing zero emits");
        assert_eq!((s.disk_bytes, s.net_bytes), (0, 0));

        // Further idle ticks: recorded in the ring but NOT emitted.
        assert!(tick(&hub, &mut prev, &mut last_zero, 3).is_none());
        assert!(tick(&hub, &mut prev, &mut last_zero, 4).is_none());

        // Work resumes: emitted again, with the delta only (not cumulative).
        hub.counters().add_net_wire(7);
        let s = tick(&hub, &mut prev, &mut last_zero, 5).expect("resumed work emits");
        assert_eq!((s.disk_bytes, s.net_bytes), (0, 7));

        // The ring recorded EVERY tick, quiet gaps included, oldest first.
        let series = hub.series();
        assert_eq!(series.bucket_ms, 1000);
        let deltas: Vec<(u64, u64)> = series
            .samples
            .iter()
            .map(|s| (s.disk_bytes, s.net_bytes))
            .collect();
        assert_eq!(deltas, vec![(1000, 400), (0, 0), (0, 0), (0, 0), (0, 7)]);
    }
}
