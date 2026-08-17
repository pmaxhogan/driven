//! Live bottleneck classification behind the Activity dashboard's Bottleneck
//! stat tile (issue #308, 2026-08-17 follow-up).
//!
//! Every second the sampler task diffs the SAME app-global disk/net/hash byte
//! counters [`crate::iostat_hub`] diffs for the throughput graphs, polls every
//! account's orchestrator for its current state + rate-pacer backoff, runs
//! the pure [`classify`] heuristic over the combined signals, and pushes the
//! result to the webview as a live `sync:bottleneck` event. The
//! `bottleneck_status` command hydrates the initial paint from the latest
//! snapshot. Idle-suppressed the same way `iostat_hub` is: once the state
//! settles on `NotBackingUp` further identical ticks are recorded but not
//! re-emitted, so a fully idle app costs nothing on the IPC bridge.

use std::sync::Mutex;
use std::time::Duration;

use driven_core::time::{Clock, SystemClock};
use driven_core::types::OrchestratorState;
use serde::Serialize;
use tauri::AppHandle;

const TARGET: &str = "driven::app::bottleneck";

/// Sampling cadence (matches [`crate::iostat_hub::SAMPLE_INTERVAL`]).
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Below this per-second rate a pipeline stage reads as "not really moving" -
/// keeps a single stray byte from tipping the classification, and keeps a
/// genuinely idle stage out of the dominance comparison entirely.
const IDLE_FLOOR_BYTES_PER_SEC: u64 = 32 * 1024;

/// A stage's rate must trail the FASTEST active stage by at least this
/// multiple to be called a clear bottleneck; short of it there is no clear
/// limiter (`Mixed`) - the pipeline is flowing at one shared rate.
const DOMINANCE_RATIO: f64 = 1.5;

/// Which stage of the backup pipeline is presently limiting throughput (the
/// six states the mockup's Bottleneck tile shows).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BottleneckState {
    /// No account is mid-cycle.
    NotBackingUp,
    /// Local disk reads are the slowest active stage.
    Disk,
    /// Network wire acceptance is the slowest active stage.
    Network,
    /// A rate pacer is presently backing off (rate-limit / circuit-breaker
    /// trip) on an account that is mid-cycle.
    Api,
    /// Blake3 hashing is the slowest active stage.
    Cpu,
    /// Multiple stages are active with no one clearly trailing the others.
    Mixed,
}

/// One classified snapshot - the `sync:bottleneck` event and
/// `bottleneck_status` command payload (camelCase on the wire).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BottleneckSnapshot {
    /// Wall-clock ms this snapshot was classified.
    pub ts_ms: i64,
    pub state: BottleneckState,
    /// The saturated stage's rate in bytes/sec, present only when `state`
    /// names a rate-bearing stage (Disk/Network/Cpu).
    pub rate_bytes_per_sec: Option<u64>,
    /// The rate-limited destination's short display label ("Drive", "S3",
    /// ...), present only when `state == Api`.
    pub backend: Option<String>,
    /// Ms remaining in the active backoff window, present only when `state
    /// == Api`.
    pub backoff_remaining_ms: Option<i64>,
}

impl BottleneckSnapshot {
    fn not_backing_up(ts_ms: i64) -> Self {
        Self {
            ts_ms,
            state: BottleneckState::NotBackingUp,
            rate_bytes_per_sec: None,
            backend: None,
            backoff_remaining_ms: None,
        }
    }
}

/// Raw per-tick signals the pure [`classify`] function reasons over - the
/// unit-test seam, independent of AppState/orchestrator plumbing.
#[derive(Debug, Clone, PartialEq)]
pub struct BottleneckSignals {
    /// Whether ANY account is presently mid-cycle: `Scanning` / `Planning` /
    /// `Executing` / `Verifying` / `Recovering` / `PowerCheck`, OR backing off
    /// (a `Backoff` state, or a mid-cycle account whose rate pacer is
    /// presently throttled, both count as "still working, just paced").
    /// `Idle` / `Paused` / `Error` accounts do not count.
    pub active_cycle: bool,
    /// The most-throttled mid-cycle account's backoff, if any: `(backend
    /// label, remaining ms)`. Aggregated across accounts by picking the
    /// longest remaining window, so the tile never undersells how long the
    /// app will stay paced.
    pub backoff: Option<(String, i64)>,
    /// Plaintext bytes read from local files, per second.
    pub disk_bytes_per_sec: u64,
    /// Wire bytes the destination accepted, per second.
    pub net_bytes_per_sec: u64,
    /// Plaintext bytes blake3-hashed, per second.
    pub hash_bytes_per_sec: u64,
}

/// The pure classification heuristic (issue #308), unit-tested over all six
/// states plus the boundary cases around [`IDLE_FLOOR_BYTES_PER_SEC`] and
/// [`DOMINANCE_RATIO`]. A pure function of one snapshot: calling it twice
/// with the same signals always yields the same state, so any flap-smoothing
/// (debounce, hysteresis) is entirely the caller's problem - here the
/// frontend Pinia store, which is where the spec puts it.
///
/// 1. No account mid-cycle => [`BottleneckState::NotBackingUp`].
/// 2. A mid-cycle account's rate pacer is presently backing off (or the
///    account itself is in the orchestrator-level `Backoff` state, a
///    circuit-breaker/rate-limit trip) => [`BottleneckState::Api`]. Checked
///    before the rate comparison because a paced account can otherwise show
///    misleadingly "healthy" disk/net/hash rates between backoff-gated
///    requests.
/// 3. Otherwise compare the three per-second rates (disk read, net wire
///    accepted, blake3-hashed): stages at or below the idle floor are
///    dropped from consideration entirely (a scan phase with no upload
///    traffic yet should not make Network read as "the bottleneck at
///    0 B/s"). Among the remaining ACTIVE stages, the slowest is the
///    reported bottleneck, provided the fastest active stage clears it by at
///    least [`DOMINANCE_RATIO`] - a pipeline flowing at one shared rate (the
///    common case: every stage paced by the same backpressure) has no clear
///    winner and reports [`BottleneckState::Mixed`] instead. Zero active
///    stages during a mid-cycle account (e.g. the `Planning` phase, which
///    moves no bytes) is also `Mixed` - work is happening, but nothing
///    currently measurable is the limiter.
pub fn classify(signals: &BottleneckSignals) -> (BottleneckState, Option<u64>) {
    if !signals.active_cycle {
        return (BottleneckState::NotBackingUp, None);
    }
    if signals.backoff.is_some() {
        return (BottleneckState::Api, None);
    }

    let stages = [
        (BottleneckState::Disk, signals.disk_bytes_per_sec),
        (BottleneckState::Network, signals.net_bytes_per_sec),
        (BottleneckState::Cpu, signals.hash_bytes_per_sec),
    ];
    let active: Vec<(BottleneckState, u64)> = stages
        .into_iter()
        .filter(|&(_, rate)| rate > IDLE_FLOOR_BYTES_PER_SEC)
        .collect();

    match active.len() {
        0 => (BottleneckState::Mixed, None),
        1 => (active[0].0, Some(active[0].1)),
        _ => {
            // `active` has >= 2 distinct stages (Disk/Network/Cpu can never
            // repeat), so both `min_by_key`/`max` below always find a value.
            let (slow_state, slow_rate) = *active.iter().min_by_key(|&&(_, r)| r).unwrap();
            let fastest = active.iter().map(|&(_, r)| r).max().unwrap();
            if (fastest as f64) >= (slow_rate as f64) * DOMINANCE_RATIO {
                (slow_state, Some(slow_rate))
            } else {
                (BottleneckState::Mixed, None)
            }
        }
    }
}

/// Whether an [`OrchestratorState`] counts as "this account is mid-cycle"
/// for [`BottleneckSignals::active_cycle`] - everything except the three
/// at-rest states.
fn is_active(state: &OrchestratorState) -> bool {
    !matches!(
        state,
        OrchestratorState::Idle { .. }
            | OrchestratorState::Paused { .. }
            | OrchestratorState::Error { .. }
    )
}

/// Bytes/sec from a cumulative-counter delta over `elapsed_ms`. Guards
/// against a zero/negative elapsed (clock oddities) by flooring at 1ms so
/// this can never divide by zero or return a spurious infinite rate.
fn rate_bytes_per_sec(cur: u64, prev: u64, elapsed_ms: i64) -> u64 {
    let delta = cur.saturating_sub(prev);
    let elapsed = elapsed_ms.max(1) as u128;
    ((delta as u128 * 1000) / elapsed) as u64
}

/// The latest classified snapshot, held for the `bottleneck_status` command's
/// hydration read. No ring/history - the frontend store owns any windowing
/// it wants for hysteresis.
#[derive(Debug)]
pub struct BottleneckHub {
    latest: Mutex<BottleneckSnapshot>,
}

impl Default for BottleneckHub {
    fn default() -> Self {
        Self {
            latest: Mutex::new(BottleneckSnapshot::not_backing_up(0)),
        }
    }
}

impl BottleneckHub {
    /// The latest classification, for the hydration command. Always
    /// available (defaults to `NotBackingUp` before the first tick).
    pub fn latest(&self) -> BottleneckSnapshot {
        self.latest
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    fn push(&self, snapshot: BottleneckSnapshot) {
        if let Ok(mut g) = self.latest.lock() {
            *g = snapshot;
        }
    }
}

/// One sampler tick: reads the app-global IO counters + every account's
/// orchestrator, classifies, records the snapshot on `hub`, and - unless
/// idle-suppressed - emits `sync:bottleneck`. Returns the delta rates purely
/// so the caller can carry `prev_io`/`prev_ms` forward; the emit decision
/// lives here so the idle-suppression state (`last_emitted`) stays local to
/// the sampler loop, mirroring `iostat_hub::tick`.
async fn tick_once(
    app: &AppHandle,
    hub: &BottleneckHub,
    prev_io: &mut driven_core::iostat::IoSnapshot,
    prev_ms: &mut i64,
    last_emitted: &mut Option<BottleneckState>,
) {
    use tauri::Manager;
    let Some(state) = app.try_state::<crate::app_state::AppState>() else {
        return;
    };

    let now_ms = SystemClock.now_ms();
    let elapsed_ms = now_ms - *prev_ms;
    *prev_ms = now_ms;

    let cur_io = state.iostat_hub().counters().snapshot();
    let disk_bytes_per_sec =
        rate_bytes_per_sec(cur_io.disk_read_bytes, prev_io.disk_read_bytes, elapsed_ms);
    let net_bytes_per_sec =
        rate_bytes_per_sec(cur_io.net_wire_bytes, prev_io.net_wire_bytes, elapsed_ms);
    let hash_bytes_per_sec =
        rate_bytes_per_sec(cur_io.hashed_bytes, prev_io.hashed_bytes, elapsed_ms);
    *prev_io = cur_io;

    let mut active_cycle = false;
    let mut chosen_backoff: Option<(String, i64)> = None;
    for (_, handle) in state.accounts() {
        let account_state = handle.orchestrator.state().await;
        let account_active = is_active(&account_state);
        // The account-level circuit-breaker/rate-limit trip carries its own
        // deadline; a mid-cycle account otherwise defers to its rate
        // pacer's live backoff window (issue #308's primary signal).
        let remaining_ms = match &account_state {
            OrchestratorState::Backoff { until } => Some((*until - now_ms).max(0)),
            _ if account_active => handle.orchestrator.pacer_backoff_remaining_ms(),
            _ => None,
        };
        if account_active {
            active_cycle = true;
        }
        if let Some(remaining_ms) = remaining_ms {
            let better = chosen_backoff
                .as_ref()
                .is_none_or(|(_, r)| remaining_ms > *r);
            if better {
                chosen_backoff = Some((
                    handle.orchestrator.backend_label().to_string(),
                    remaining_ms,
                ));
            }
        }
    }

    let signals = BottleneckSignals {
        active_cycle,
        backoff: chosen_backoff.clone(),
        disk_bytes_per_sec,
        net_bytes_per_sec,
        hash_bytes_per_sec,
    };
    let (bottleneck_state, rate_bytes_per_sec) = classify(&signals);
    let is_api = bottleneck_state == BottleneckState::Api;
    let snapshot = BottleneckSnapshot {
        ts_ms: now_ms,
        state: bottleneck_state,
        rate_bytes_per_sec,
        backend: if is_api {
            chosen_backoff.as_ref().map(|(b, _)| b.clone())
        } else {
            None
        },
        backoff_remaining_ms: if is_api {
            chosen_backoff.as_ref().map(|(_, r)| *r)
        } else {
            None
        },
    };
    hub.push(snapshot.clone());

    // Idle suppression (mirrors `iostat_hub::tick`): once a `NotBackingUp`
    // has been emitted, further identical ticks are recorded (so a late
    // `bottleneck_status` hydration is still correct) but not re-broadcast -
    // an idle app costs nothing on the IPC bridge.
    let suppress = bottleneck_state == BottleneckState::NotBackingUp
        && *last_emitted == Some(bottleneck_state);
    if !suppress {
        *last_emitted = Some(bottleneck_state);
        crate::events::emit_sync_bottleneck(app, snapshot);
    }
}

/// Spawn the sampling loop (updater/telemetry lifecycle pattern; see
/// [`crate::iostat_hub::spawn_sampler`]). Called once from setup after
/// `AppState` is managed.
pub fn spawn_sampler(app: &AppHandle) {
    use tauri::Manager;
    let Some(state) = app.try_state::<crate::app_state::AppState>() else {
        tracing::warn!(target: TARGET, "AppState not managed; bottleneck sampler not started");
        return;
    };
    let hub = state.bottleneck_hub();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let app_handle = app.clone();

    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut prev_io = driven_core::iostat::IoSnapshot {
            disk_read_bytes: 0,
            net_wire_bytes: 0,
            hashed_bytes: 0,
        };
        let mut prev_ms = SystemClock.now_ms();
        let mut last_emitted: Option<BottleneckState> = None;
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
                    tick_once(&app_handle, &hub, &mut prev_io, &mut prev_ms, &mut last_emitted).await;
                }
            }
        }
        tracing::debug!(target: TARGET, "bottleneck sampler exited");
    });

    state.set_bottleneck_task(task, shutdown_tx);
    tracing::info!(
        target: TARGET,
        interval_ms = SAMPLE_INTERVAL.as_millis() as u64,
        "bottleneck sampler started"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(
        active_cycle: bool,
        backoff: Option<(&str, i64)>,
        disk: u64,
        net: u64,
        hash: u64,
    ) -> BottleneckSignals {
        BottleneckSignals {
            active_cycle,
            backoff: backoff.map(|(b, r)| (b.to_string(), r)),
            disk_bytes_per_sec: disk,
            net_bytes_per_sec: net,
            hash_bytes_per_sec: hash,
        }
    }

    #[test]
    fn not_backing_up_when_no_account_is_mid_cycle() {
        let (state, rate) = classify(&signals(false, None, 999_999, 999_999, 999_999));
        assert_eq!(state, BottleneckState::NotBackingUp);
        assert_eq!(rate, None);
    }

    #[test]
    fn api_wins_over_any_rate_reading_while_a_pacer_backs_off() {
        let (state, rate) = classify(&signals(
            true,
            Some(("Drive", 8_000)),
            500_000,
            500_000,
            500_000,
        ));
        assert_eq!(state, BottleneckState::Api);
        // Api carries no rate in the classifier's own output - the backend +
        // remaining ms come from `signals.backoff`, surfaced by the caller.
        assert_eq!(rate, None);
    }

    #[test]
    fn disk_is_the_clear_bottleneck_when_it_trails_the_others() {
        let (state, rate) = classify(&signals(true, None, 100_000, 400_000, 400_000));
        assert_eq!(state, BottleneckState::Disk);
        assert_eq!(rate, Some(100_000));
    }

    #[test]
    fn network_is_the_clear_bottleneck_when_it_trails_the_others() {
        let (state, rate) = classify(&signals(true, None, 400_000, 100_000, 400_000));
        assert_eq!(state, BottleneckState::Network);
        assert_eq!(rate, Some(100_000));
    }

    #[test]
    fn cpu_is_the_clear_bottleneck_when_it_trails_the_others() {
        let (state, rate) = classify(&signals(true, None, 400_000, 400_000, 100_000));
        assert_eq!(state, BottleneckState::Cpu);
        assert_eq!(rate, Some(100_000));
    }

    #[test]
    fn cpu_alone_active_reads_as_cpu_even_with_no_rival_stage() {
        // A deep-verify scan hashing files with nothing queued to upload yet:
        // only the hash counter is moving.
        let (state, rate) = classify(&signals(true, None, 0, 0, 900_000));
        assert_eq!(state, BottleneckState::Cpu);
        assert_eq!(rate, Some(900_000));
    }

    #[test]
    fn mixed_when_all_active_stages_run_at_one_shared_rate() {
        let (state, rate) = classify(&signals(true, None, 300_000, 310_000, 305_000));
        assert_eq!(state, BottleneckState::Mixed);
        assert_eq!(rate, None);
    }

    #[test]
    fn mixed_when_mid_cycle_but_nothing_measurable_is_moving() {
        // e.g. the `Planning` phase: an active cycle, but no bytes moved yet.
        let (state, rate) = classify(&signals(true, None, 0, 0, 0));
        assert_eq!(state, BottleneckState::Mixed);
        assert_eq!(rate, None);
    }

    #[test]
    fn a_stage_at_or_under_the_idle_floor_is_dropped_from_consideration() {
        // Net sits right at the idle floor - not "active" - so with only
        // disk+hash left active and equal, this is Mixed, not Network.
        let (state, _) = classify(&signals(
            true,
            None,
            300_000,
            IDLE_FLOOR_BYTES_PER_SEC,
            300_000,
        ));
        assert_eq!(state, BottleneckState::Mixed);
    }

    #[test]
    fn dominance_ratio_boundary_is_inclusive() {
        // Fastest exactly DOMINANCE_RATIO x the slowest: still a clear call.
        let slow = 100_000u64;
        let fast = (slow as f64 * DOMINANCE_RATIO) as u64;
        let (state, _) = classify(&signals(true, None, slow, fast, fast));
        assert_eq!(state, BottleneckState::Disk);

        // One byte short of the ratio: no longer a clear call.
        let (state, _) = classify(&signals(true, None, slow, fast - 1, fast - 1));
        assert_eq!(state, BottleneckState::Mixed);
    }

    #[test]
    fn classify_is_pure_and_hysteresis_friendly() {
        // Calling it twice with the same signals is idempotent - any
        // flap-smoothing is entirely the caller's (the frontend store's) job.
        let s = signals(true, None, 100_000, 400_000, 400_000);
        assert_eq!(classify(&s), classify(&s));
    }

    #[test]
    fn rate_bytes_per_sec_floors_a_degenerate_elapsed_at_1ms() {
        // A zero/negative elapsed (clock oddity) must never divide by zero
        // or panic; it floors at 1ms, producing a large-but-finite rate.
        assert_eq!(rate_bytes_per_sec(1_000, 0, 0), 1_000_000);
        assert_eq!(rate_bytes_per_sec(1_000, 0, -5), 1_000_000);
        assert_eq!(rate_bytes_per_sec(0, 0, 1000), 0);
    }

    #[test]
    fn is_active_excludes_only_the_three_at_rest_states() {
        assert!(!is_active(&OrchestratorState::Idle { last_run_at: None }));
        assert!(!is_active(&OrchestratorState::Paused {
            reason: driven_core::types::PauseReason::Manual
        }));
        assert!(is_active(&OrchestratorState::PowerCheck));
        assert!(is_active(&OrchestratorState::Backoff { until: 0 }));
    }
}
