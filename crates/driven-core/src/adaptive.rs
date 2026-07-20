//! Adaptive upload parallelism (DESIGN s11.4.7, s18.2).
//!
//! Drive lets us overlap whole files, not chunks of one file (DESIGN s11.4.1), so
//! the one concurrency knob that matters is "how many files are in flight at
//! once" - the [`UploadPool`] permit count. A fixed count is a guess: too few
//! wastes the link, too many overloads Drive's edge so each upload takes longer
//! and *net* throughput falls (DESIGN s11.4.7's pathological case). This module
//! closes the loop around that knob.
//!
//! # The pieces
//!
//! - [`UploadPool`] - a RESIZABLE `tokio::sync::Semaphore` (the executor's
//!   per-file gate). Grows by adding a permit; shrinks by permanently forgetting
//!   one. Bounds `[min, max]` with `max` the hard cap 32 (DESIGN s11.4.2).
//! - [`ThroughputProbe`] - a lock-free byte accumulator the executor feeds at
//!   each completed upload; the controller drains it once per window to measure
//!   aggregate throughput.
//! - [`decide`] - the PURE control law: `(window stats, disk, pacer, size) ->
//!   {Grow, Shrink, Hold}`. No I/O, no clock, exhaustively unit-tested.
//! - [`AdaptiveController`] - the thin impure shell the orchestrator's run loop
//!   ticks on a fixed cadence; it samples the disk, drains the probe every
//!   window, calls [`decide`], and applies the result to the pool.
//!
//! # Windowing + cadence (DESIGN s11.4.7 / s18.2)
//!
//! Throughput is compared over tumbling [`WINDOW`] (30 s) windows; the disk-busy
//! gate is sampled every [`SAMPLE_INTERVAL`] (5 s). The controller is ticked at
//! `SAMPLE_INTERVAL`: each tick samples the disk, and every sixth tick (a full
//! window elapsed on the injected [`Clock`](crate::time::Clock)) runs a decision.
//! Driving the window off the injected clock - never `Instant::now()` - is what
//! makes the whole loop deterministic under a `FakeClock`.
//!
//! # Kill-switch semantics
//!
//! When `adaptive_parallelism_enabled` is true (default) the app wires a
//! controller and the pool floats within `[1, 32]` starting from
//! `default_concurrent_uploads`. When false, no controller is built: the pool is
//! FIXED at `default_concurrent_uploads` exactly as before this feature. A user
//! who wants a hard concurrency limit disables adaptation.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};

use crate::pacer::Pacer;
use crate::time::Clock;
use driven_diskstat::DiskBusyProbe;

/// The hard ceiling on in-flight files (DESIGN s11.4.2: "hard cap 32"). The pool
/// may grow up to this regardless of the (lower) default start size.
pub const MAX_POOL: usize = 32;

/// The floor on in-flight files. One keeps the pipeline alive at minimum
/// concurrency; the pool never shrinks below it.
pub const MIN_POOL: usize = 1;

/// Throughput comparison window (DESIGN s11.4.7: "30-second windows").
pub const WINDOW: Duration = Duration::from_secs(30);

/// Disk-busy sampling cadence (DESIGN s18.2: "sampled at 5-second intervals").
/// Also the cadence at which the orchestrator ticks the controller.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// The default STARTING pool size when the user has not set
/// `default_concurrent_uploads` (DESIGN s11.4.2: `min(available_parallelism * 2,
/// 16)`, clamped into `[MIN_POOL, MAX_POOL]`). The single source of truth for the
/// auto-picked concurrency, used by both the executor's default construction and
/// the app-shell's adaptive wiring.
#[must_use]
pub fn default_pool_size() -> usize {
    let par = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    par.saturating_mul(2).min(16).clamp(MIN_POOL, MAX_POOL)
}

/// Shrink trigger: a window whose throughput is below this fraction of the
/// previous window's is a "collapse" (DESIGN s11.4.7: "< 50% of the previous
/// window's").
pub const SHRINK_RATIO: f64 = 0.5;

/// Grow trigger: throughput must EXCEED the previous window by at least this
/// factor to justify adding a permit. See [`decide`] for why growth requires
/// improvement (our resolution of DESIGN's "throughput is at the pool's
/// ceiling").
pub const GROW_IMPROVE_RATIO: f64 = 1.05;

/// How long a shrink will wait for an in-flight permit to free before giving up
/// and retrying on the next window. Bounded so the orchestrator's run-loop tick
/// (which awaits this inline) is never blocked for long; a miss is harmless
/// (the pool simply stays one larger until the next decision).
const SHRINK_ACQUIRE_TIMEOUT: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// UploadPool
// ---------------------------------------------------------------------------

/// A resizable in-flight-file gate (DESIGN s11.4.2 / s11.4.7).
///
/// Wraps a `tokio::sync::Semaphore` whose permit count is the current pool size.
/// The executor acquires one permit per file via [`acquire_owned`](Self::acquire_owned)
/// and releases it by dropping the permit. The controller resizes via
/// [`grow`](Self::grow) / [`shrink`](Self::shrink).
///
/// # Resize mechanism
///
/// - **Grow**: `Semaphore::add_permits(1)` - the new permit is immediately
///   available to the next waiting file.
/// - **Shrink**: acquire one permit and `forget()` it, which permanently removes
///   it from circulation. Done inline under a short [`SHRINK_ACQUIRE_TIMEOUT`]
///   (never a detached task - the repo forbids orphanable spawns). On timeout the
///   size is left unchanged and the next window retries.
///
/// [`target`](Self::target) - not `available_permits()` - is the authoritative
/// size: `available_permits()` momentarily reads low while permits are checked
/// out and can lag a just-landed forget. `target` is only mutated by the single
/// controller task, so its loads/stores need no CAS loop.
#[derive(Debug)]
pub struct UploadPool {
    sem: Arc<Semaphore>,
    /// Authoritative logical size (permits that belong to the pool).
    target: AtomicUsize,
    min: usize,
    max: usize,
    /// Count of [`acquire_owned`](Self::acquire_owned) calls that had to WAIT for
    /// a permit since the last [`take_contended`](Self::take_contended). A
    /// non-zero count over a window means the pool was the active bottleneck
    /// (files queued for permits) - the concrete signal for "the pool is the
    /// ceiling" used by both the grow and shrink decisions.
    contended: AtomicU64,
}

impl UploadPool {
    /// Build a pool starting at `start` permits (clamped into `[MIN_POOL,
    /// MAX_POOL]`), resizable within those bounds. Returned as an `Arc` because
    /// the SAME handle is shared into the executor (which acquires) and the
    /// controller (which resizes).
    #[must_use]
    pub fn new(start: usize) -> Arc<Self> {
        Self::with_bounds(start, MIN_POOL, MAX_POOL)
    }

    /// Build a pool with explicit bounds (test seam). `start` and the bounds are
    /// clamped so `min <= start <= max` always holds.
    #[must_use]
    pub fn with_bounds(start: usize, min: usize, max: usize) -> Arc<Self> {
        let min = min.max(1);
        let max = max.max(min);
        let start = start.clamp(min, max);
        Arc::new(Self {
            sem: Arc::new(Semaphore::new(start)),
            target: AtomicUsize::new(start),
            min,
            max,
            contended: AtomicU64::new(0),
        })
    }

    /// The current authoritative pool size.
    #[must_use]
    pub fn target(&self) -> usize {
        self.target.load(Ordering::Acquire)
    }

    /// The configured lower bound.
    #[must_use]
    pub fn min(&self) -> usize {
        self.min
    }

    /// The configured upper bound (the DESIGN s11.4.2 hard cap by default).
    #[must_use]
    pub fn max(&self) -> usize {
        self.max
    }

    /// Acquire one in-flight-file permit, counting contention. Tries without
    /// waiting first; only if no permit is free does it record a contention event
    /// (the "pool is the bottleneck" signal) and then wait. Functionally
    /// identical to a bare `acquire_owned` from the caller's view.
    pub async fn acquire_owned(&self) -> Result<OwnedSemaphorePermit, AcquireError> {
        match self.sem.clone().try_acquire_owned() {
            Ok(permit) => Ok(permit),
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                self.contended.fetch_add(1, Ordering::Relaxed);
                self.sem.clone().acquire_owned().await
            }
            // Closed: fall through to the awaiting form, which surfaces the
            // `AcquireError` the caller already handles.
            Err(tokio::sync::TryAcquireError::Closed) => self.sem.clone().acquire_owned().await,
        }
    }

    /// Try to acquire a permit WITHOUT waiting and WITHOUT counting contention.
    /// Test seam for constructing a pinned-pool scenario deterministically.
    #[must_use]
    pub fn try_acquire_owned(&self) -> Option<OwnedSemaphorePermit> {
        self.sem.clone().try_acquire_owned().ok()
    }

    /// Read-and-reset the contention count accrued since the last call. Called
    /// once per decision window by the controller.
    #[must_use]
    pub fn take_contended(&self) -> u64 {
        self.contended.swap(0, Ordering::Relaxed)
    }

    /// Peek the contention count without resetting it (used for the cheap
    /// "is this account doing anything?" idle check).
    #[must_use]
    pub fn peek_contended(&self) -> u64 {
        self.contended.load(Ordering::Relaxed)
    }

    /// Grow the pool by one permit, up to [`max`](Self::max). Returns the new
    /// size (unchanged if already at the cap).
    pub fn grow(&self) -> usize {
        let cur = self.target.load(Ordering::Acquire);
        if cur >= self.max {
            return cur;
        }
        self.sem.add_permits(1);
        self.target.store(cur + 1, Ordering::Release);
        cur + 1
    }

    /// Shrink the pool by one permit, down to [`min`](Self::min). Acquires a
    /// permit (waiting at most [`SHRINK_ACQUIRE_TIMEOUT`]) and permanently
    /// forgets it. `target` is decremented ONLY on a successful forget, so a
    /// timed-out shrink leaves the size honest and simply retries next window.
    /// Returns the new (or unchanged) size.
    pub async fn shrink(&self) -> usize {
        let cur = self.target.load(Ordering::Acquire);
        if cur <= self.min {
            return cur;
        }
        match tokio::time::timeout(SHRINK_ACQUIRE_TIMEOUT, self.sem.clone().acquire_owned()).await {
            Ok(Ok(permit)) => {
                // Permanently remove this permit from circulation.
                permit.forget();
                self.target.store(cur - 1, Ordering::Release);
                cur - 1
            }
            // Timed out (all permits busy) or the semaphore is closed: no change.
            _ => cur,
        }
    }
}

// ---------------------------------------------------------------------------
// ThroughputProbe
// ---------------------------------------------------------------------------

/// A per-account aggregate-upload-throughput accumulator (DESIGN s11.4.7).
///
/// The executor calls [`record_bytes`](Self::record_bytes) at every completed
/// upload (the same site that feeds the telemetry latency reservoir); the
/// controller drains it once per window with [`take_bytes`](Self::take_bytes)
/// and divides by the injected-clock window duration to get bytes/sec. The
/// accumulator is a single lock-free atomic, so the hot upload path pays only an
/// atomic add and carries NO clock (window boundaries are the controller's job,
/// driven by its injected [`Clock`](crate::time::Clock) - keeping the whole loop
/// deterministic without an `Instant::now()` anywhere on the hot path).
#[derive(Debug, Default)]
pub struct ThroughputProbe {
    bytes: AtomicU64,
}

impl ThroughputProbe {
    /// A fresh probe with an empty window.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Add `n` uploaded bytes to the current window. Lock-free; safe to call from
    /// every concurrent upload task.
    pub fn record_bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
    }

    /// Read-and-reset the bytes accumulated this window.
    #[must_use]
    pub fn take_bytes(&self) -> u64 {
        self.bytes.swap(0, Ordering::Relaxed)
    }

    /// Peek the accumulated bytes without resetting (idle check).
    #[must_use]
    pub fn peek_bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Pure control law
// ---------------------------------------------------------------------------

/// The controller's decision for one window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Add one in-flight-file permit (up to the cap).
    Grow,
    /// Remove one in-flight-file permit (down to the floor).
    Shrink,
    /// Leave the pool size unchanged.
    Hold,
}

/// The pure inputs to one [`decide`] call (DESIGN s11.4.7).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ControllerInput {
    /// Aggregate upload throughput this window, bytes/sec.
    pub current_bps: f64,
    /// The previous window's throughput, or `None` before the first full window.
    pub previous_bps: Option<f64>,
    /// Was the pool the active bottleneck this window (files queued for permits)?
    /// The concrete "throughput is at the pool's ceiling" signal.
    pub pool_saturated: bool,
    /// Was the local disk saturated (DESIGN s18.2) for the window?
    pub disk_saturated: bool,
    /// Did the pacer throttle (rate-limit / daily-quota) at any point this
    /// window? A throttle explains a throughput drop, so it suppresses a shrink.
    pub pacer_throttled: bool,
    /// The current pool size.
    pub current_size: usize,
    /// The pool's lower bound.
    pub min_size: usize,
    /// The pool's upper bound (hard cap).
    pub max_size: usize,
}

/// The adaptive-parallelism control law (DESIGN s11.4.7), as a PURE function so
/// every branch is exhaustively unit-testable.
///
/// # Rules
///
/// 1. **Representative-window guard.** Act only when the pool was the active
///    bottleneck (`pool_saturated`) with real throughput. A draining/idle window
///    shows a throughput drop that is NOT congestion; acting on it would shrink a
///    pool that is simply out of work. (This is the guard that keeps the loop off
///    non-representative windows - it never weakens the DESIGN rules below, it
///    only refuses to apply them to noise.)
/// 2. **Shrink** (`current < 50% of previous`, and neither the pacer nor the disk
///    explains the drop, and above the floor): too many files against a congested
///    Drive edge - fewer-in-flight each finish faster (DESIGN s11.4.7).
/// 3. **Grow** (throughput still IMPROVING, disk has headroom, not rate-limited,
///    below the cap): the pool is the ceiling and lifting it is paying off.
///
/// # "At the pool's ceiling" (resolved ambiguity)
///
/// DESIGN s11.4.7 says grow when "sustained throughput is at the pool's ceiling
/// AND the disk + CPU have headroom." Taken literally as "pool pinned", a pool
/// stuck at a suboptimal-high size with steady (not falling) throughput would
/// grow every window straight to the cap, since the shrink rule only fires on an
/// active >50% drop. We operationalize "at the pool's ceiling" as *the pool is
/// pinned AND throughput is still improving window-over-window* (by
/// [`GROW_IMPROVE_RATIO`]): growth continues only while it pays off and stops at
/// a plateau, giving a stable additive-increase / multiplicative-decrease loop.
/// The first window (no previous) grows once as a bootstrap probe.
///
/// # CPU headroom (resolved ambiguity)
///
/// DESIGN s11.4.7 names "disk + CPU" headroom, but s18.2 defines a concrete
/// signal only for the DISK. Uploads are network-bound and their CPU work
/// (hash + encrypt) runs on a separate rayon pool sized to leave a core for the
/// reactor (DESIGN s11.4.5), so the disk-saturation gate is the operative
/// hardware-headroom signal; no separate dynamic CPU probe is introduced.
#[must_use]
pub fn decide(i: ControllerInput) -> Decision {
    // Rule 1: only act on a representative window.
    if !i.pool_saturated || i.current_bps <= 0.0 {
        return Decision::Hold;
    }

    // Rule 2: shrink on an unexplained throughput collapse.
    if let Some(prev) = i.previous_bps {
        if prev > 0.0
            && i.current_bps < SHRINK_RATIO * prev
            && !i.pacer_throttled
            && !i.disk_saturated
            && i.current_size > i.min_size
        {
            return Decision::Shrink;
        }
    }

    // Rule 3: grow while lifting the ceiling is still paying off.
    let improving = match i.previous_bps {
        None => true, // bootstrap probe: try one step up
        Some(prev) => i.current_bps > GROW_IMPROVE_RATIO * prev,
    };
    if improving && !i.disk_saturated && !i.pacer_throttled && i.current_size < i.max_size {
        return Decision::Grow;
    }

    Decision::Hold
}

// ---------------------------------------------------------------------------
// AdaptiveController (impure shell)
// ---------------------------------------------------------------------------

/// Per-window accumulators + the previous-window baseline. Only ever touched
/// from the single orchestrator run-loop task, behind a `Mutex` so the shell can
/// be `&self` and never holds the lock across an `.await`.
#[derive(Debug)]
struct WindowState {
    /// Injected-clock ms at which the current window opened.
    window_start_ms: i64,
    /// The previous completed window's throughput (bytes/sec), or `None`.
    previous_bps: Option<f64>,
    /// Disk-busy samples taken this window, and how many read "saturated".
    disk_samples: u32,
    disk_saturated_samples: u32,
}

/// The thin impure shell around [`decide`] (DESIGN s11.4.7). The orchestrator
/// calls [`tick`](Self::tick) every [`SAMPLE_INTERVAL`]; the shell samples the
/// disk, and once per [`WINDOW`] drains the throughput probe + contention count,
/// builds a [`ControllerInput`], calls [`decide`], and applies the result to the
/// [`UploadPool`].
pub struct AdaptiveController {
    pool: Arc<UploadPool>,
    probe: Arc<ThroughputProbe>,
    disk: Arc<dyn DiskBusyProbe>,
    pacer: Arc<dyn Pacer>,
    clock: Arc<dyn Clock>,
    window_ms: i64,
    state: Mutex<WindowState>,
}

impl AdaptiveController {
    /// Build a controller over the SAME [`UploadPool`] + [`ThroughputProbe`] the
    /// executor holds, the disk-busy probe, the account's pacer, and the clock.
    #[must_use]
    pub fn new(
        pool: Arc<UploadPool>,
        probe: Arc<ThroughputProbe>,
        disk: Arc<dyn DiskBusyProbe>,
        pacer: Arc<dyn Pacer>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let now = clock.now_ms();
        Self {
            pool,
            probe,
            disk,
            pacer,
            clock,
            window_ms: WINDOW.as_millis() as i64,
            state: Mutex::new(WindowState {
                window_start_ms: now,
                previous_bps: None,
                disk_samples: 0,
                disk_saturated_samples: 0,
            }),
        }
    }

    /// The upload pool this controller resizes (so the caller can share the same
    /// handle into the executor).
    #[must_use]
    pub fn pool(&self) -> &Arc<UploadPool> {
        &self.pool
    }

    /// One controller tick, called by the orchestrator every [`SAMPLE_INTERVAL`].
    ///
    /// Samples the disk (unless the account is idle - no bytes, no contention -
    /// in which case the disk syscall is skipped) and, once a full [`WINDOW`] has
    /// elapsed on the injected clock, runs a decision. `async` because a shrink
    /// awaits a freeing permit; the window-state lock is always released before
    /// that await.
    pub async fn tick(&self) {
        let now = self.clock.now_ms();
        // Cheap idle check: don't syscall the disk on an account doing nothing.
        let active = self.probe.peek_bytes() > 0 || self.pool.peek_contended() > 0;

        let decision_input = {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());

            if active {
                let busy = self.disk.sample();
                st.disk_samples = st.disk_samples.saturating_add(1);
                if busy.is_saturated() {
                    st.disk_saturated_samples = st.disk_saturated_samples.saturating_add(1);
                }
            }

            if now.saturating_sub(st.window_start_ms) < self.window_ms {
                // Not a decision tick yet - just accumulated a disk sample.
                return;
            }

            // --- Decision boundary: drain the window and build the input. ---
            let elapsed_ms = now.saturating_sub(st.window_start_ms).max(1);
            let bytes = self.probe.take_bytes();
            let contended = self.pool.take_contended();
            let current_bps = (bytes as f64) * 1000.0 / (elapsed_ms as f64);
            // Disk is "saturated for the window" if the majority of samples read
            // saturated (a single transient spike does not gate the pool).
            let disk_saturated =
                st.disk_samples > 0 && st.disk_saturated_samples * 2 >= st.disk_samples;
            // Throttle is window-scoped: any throttle AT/AFTER the window start,
            // even one that has since cleared, counts.
            let pacer_throttled = self.pacer.last_throttle_ms() >= st.window_start_ms;

            let input = ControllerInput {
                current_bps,
                previous_bps: st.previous_bps,
                pool_saturated: contended > 0,
                disk_saturated,
                pacer_throttled,
                current_size: self.pool.target(),
                min_size: self.pool.min(),
                max_size: self.pool.max(),
            };

            // Roll the window forward. Only a REPRESENTATIVE window updates the
            // baseline (F3): the same guard `decide` rule 1 acts on
            // (`pool_saturated && current_bps > 0`). A draining / idle /
            // non-saturated window is not a valid throughput reference - letting
            // it become `previous_bps` would skew the NEXT window's shrink/grow
            // ratio (a genuinely-congested window measured against a low
            // non-representative baseline can miss a real shrink, or over-grow
            // against an anomalously low one). Non-representative windows leave
            // the last representative baseline intact.
            if input.pool_saturated && input.current_bps > 0.0 {
                st.previous_bps = Some(current_bps);
            }
            st.window_start_ms = now;
            st.disk_samples = 0;
            st.disk_saturated_samples = 0;

            input
        }; // lock dropped here, before any await

        match decide(decision_input) {
            Decision::Grow => {
                let size = self.pool.grow();
                tracing::debug!(
                    target: "driven::adaptive",
                    new_size = size,
                    throughput_bps = decision_input.current_bps,
                    "adaptive parallelism: grew upload pool"
                );
            }
            Decision::Shrink => {
                let size = self.pool.shrink().await;
                tracing::debug!(
                    target: "driven::adaptive",
                    new_size = size,
                    throughput_bps = decision_input.current_bps,
                    "adaptive parallelism: shrank upload pool"
                );
            }
            Decision::Hold => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The in-crate `FakeClock` (same `driven_core` instance) - NOT the fixtures'
    // one: driven-core's unit tests link a second `driven_core` via the
    // `driven-test-fixtures` dev-dependency cycle, so `driven_test_fixtures`'s
    // `FakeClock` implements a DIFFERENT `Clock` and won't coerce here. See
    // `crate::test_support`. `FakeDiskBusyProbe` is fine (its `DiskBusyProbe`
    // trait lives in the non-cyclic `driven-diskstat`, a single instance).
    use crate::test_support::FakeClock;
    use driven_test_fixtures::diskstat::FakeDiskBusyProbe;

    // --- pure `decide` ----------------------------------------------------

    /// A baseline input: pinned pool, real throughput, disk + pacer clear, room
    /// to move in both directions. Individual tests override one field.
    fn base() -> ControllerInput {
        ControllerInput {
            current_bps: 1_000_000.0,
            previous_bps: Some(1_000_000.0),
            pool_saturated: true,
            disk_saturated: false,
            pacer_throttled: false,
            current_size: 8,
            min_size: MIN_POOL,
            max_size: MAX_POOL,
        }
    }

    #[test]
    fn holds_when_pool_not_saturated() {
        // A draining window (throughput fell) must NOT shrink if the pool was not
        // the bottleneck.
        let i = ControllerInput {
            pool_saturated: false,
            current_bps: 100.0,
            previous_bps: Some(1_000_000.0),
            ..base()
        };
        assert_eq!(decide(i), Decision::Hold);
    }

    #[test]
    fn holds_when_zero_throughput() {
        let i = ControllerInput {
            current_bps: 0.0,
            ..base()
        };
        assert_eq!(decide(i), Decision::Hold);
    }

    #[test]
    fn shrinks_on_unexplained_collapse() {
        let i = ControllerInput {
            current_bps: 400_000.0, // < 50% of 1_000_000
            previous_bps: Some(1_000_000.0),
            ..base()
        };
        assert_eq!(decide(i), Decision::Shrink);
    }

    #[test]
    fn collapse_but_throttled_holds() {
        // The pacer explains the drop -> not our concurrency's fault.
        let i = ControllerInput {
            current_bps: 400_000.0,
            pacer_throttled: true,
            ..base()
        };
        assert_eq!(decide(i), Decision::Hold);
    }

    #[test]
    fn collapse_but_disk_saturated_holds() {
        let i = ControllerInput {
            current_bps: 400_000.0,
            disk_saturated: true,
            ..base()
        };
        assert_eq!(decide(i), Decision::Hold);
    }

    #[test]
    fn collapse_at_floor_holds() {
        let i = ControllerInput {
            current_bps: 400_000.0,
            current_size: MIN_POOL,
            ..base()
        };
        assert_eq!(decide(i), Decision::Hold);
    }

    #[test]
    fn grows_when_improving_with_headroom() {
        let i = ControllerInput {
            current_bps: 2_000_000.0, // > 1.05x previous
            previous_bps: Some(1_000_000.0),
            ..base()
        };
        assert_eq!(decide(i), Decision::Grow);
    }

    #[test]
    fn bootstrap_grows_on_first_window() {
        let i = ControllerInput {
            previous_bps: None,
            ..base()
        };
        assert_eq!(decide(i), Decision::Grow);
    }

    #[test]
    fn flat_throughput_holds() {
        // Neither a collapse nor an improvement -> a plateau -> settle.
        let i = ControllerInput {
            current_bps: 1_000_000.0,
            previous_bps: Some(1_000_000.0),
            ..base()
        };
        assert_eq!(decide(i), Decision::Hold);
    }

    #[test]
    fn grow_blocked_by_disk_saturation() {
        let i = ControllerInput {
            current_bps: 2_000_000.0,
            disk_saturated: true,
            ..base()
        };
        assert_eq!(decide(i), Decision::Hold);
    }

    #[test]
    fn grow_blocked_by_throttle() {
        let i = ControllerInput {
            current_bps: 2_000_000.0,
            pacer_throttled: true,
            ..base()
        };
        assert_eq!(decide(i), Decision::Hold);
    }

    #[test]
    fn grow_blocked_at_cap() {
        let i = ControllerInput {
            current_bps: 2_000_000.0,
            current_size: MAX_POOL,
            ..base()
        };
        assert_eq!(decide(i), Decision::Hold);
    }

    // --- UploadPool -------------------------------------------------------

    #[test]
    fn pool_new_clamps_start_into_bounds() {
        assert_eq!(UploadPool::with_bounds(0, 1, 32).target(), 1);
        assert_eq!(UploadPool::with_bounds(999, 1, 32).target(), 32);
        assert_eq!(UploadPool::new(50).target(), MAX_POOL); // start clamped to cap
    }

    #[tokio::test]
    async fn pool_grows_to_cap_and_stops() {
        let pool = UploadPool::with_bounds(1, 1, 3);
        assert_eq!(pool.grow(), 2);
        assert_eq!(pool.grow(), 3);
        assert_eq!(pool.grow(), 3, "must not grow past the cap");
        // The extra permits are really available for acquisition.
        let _a = pool.try_acquire_owned().unwrap();
        let _b = pool.try_acquire_owned().unwrap();
        let _c = pool.try_acquire_owned().unwrap();
        assert!(pool.try_acquire_owned().is_none(), "only 3 permits exist");
    }

    #[tokio::test]
    async fn pool_shrinks_to_floor_and_stops() {
        let pool = UploadPool::with_bounds(3, 1, 3);
        assert_eq!(pool.shrink().await, 2);
        assert_eq!(pool.shrink().await, 1);
        assert_eq!(pool.shrink().await, 1, "must not shrink past the floor");
        // Only one permit remains after shrinking to the floor.
        let _a = pool.try_acquire_owned().unwrap();
        assert!(pool.try_acquire_owned().is_none());
    }

    #[tokio::test]
    async fn pool_counts_contention() {
        let pool = UploadPool::with_bounds(1, 1, 4);
        let _held = pool.acquire_owned().await.unwrap(); // uncontended
        assert_eq!(pool.peek_contended(), 0);
        // No permit free -> this contends (times out, but the count already rose).
        let _ = tokio::time::timeout(Duration::from_millis(5), pool.acquire_owned()).await;
        assert_eq!(pool.take_contended(), 1);
        assert_eq!(pool.take_contended(), 0, "take resets");
    }

    // --- ThroughputProbe --------------------------------------------------

    #[test]
    fn probe_accumulates_and_drains() {
        let p = ThroughputProbe::new();
        p.record_bytes(100);
        p.record_bytes(50);
        assert_eq!(p.peek_bytes(), 150);
        assert_eq!(p.take_bytes(), 150);
        assert_eq!(p.take_bytes(), 0);
    }

    // --- AdaptiveController end-to-end (deterministic) --------------------

    /// Drive a full window: record `bytes` of upload, force the pool to register
    /// contention (so it reads as the bottleneck), advance the clock one window,
    /// and tick the controller to a decision.
    async fn run_window(ctrl: &AdaptiveController, clock: &FakeClock, bytes: u64) {
        ctrl.probe.record_bytes(bytes);
        // Pin every permit, then contend once so `pool_saturated` is true.
        let held: Vec<_> = (0..ctrl.pool.target())
            .filter_map(|_| ctrl.pool.try_acquire_owned())
            .collect();
        let _ = tokio::time::timeout(Duration::from_millis(5), ctrl.pool.acquire_owned()).await;
        drop(held);
        clock.advance(WINDOW);
        ctrl.tick().await;
    }

    /// Drive a NON-representative window: record `bytes` but never contend the
    /// pool, so `pool_saturated` is false. `decide` Holds on it (rule 1) and,
    /// with the F3 fix, it must not become the baseline.
    async fn run_window_unsaturated(ctrl: &AdaptiveController, clock: &FakeClock, bytes: u64) {
        ctrl.probe.record_bytes(bytes);
        clock.advance(WINDOW);
        ctrl.tick().await;
    }

    #[tokio::test]
    async fn controller_shrinks_on_latency_then_recovers() {
        let clock = Arc::new(FakeClock::new());
        let pool = UploadPool::new(8);
        let probe = ThroughputProbe::new();
        let disk: Arc<dyn DiskBusyProbe> = Arc::new(FakeDiskBusyProbe::not_saturated());
        let pacer: Arc<dyn Pacer> = Arc::new(crate::pacer::AimdPacer::new(clock.clone(), None));
        let ctrl = AdaptiveController::new(pool.clone(), probe.clone(), disk, pacer, clock.clone());

        // Warm up to a steady high-throughput baseline (bootstrap grow + plateau).
        run_window(&ctrl, &clock, 60_000_000).await; // establishes previous
        run_window(&ctrl, &clock, 60_000_000).await; // flat -> settle
        let before_latency = pool.target();

        // Induce latency: throughput collapses well below 50% of the baseline.
        run_window(&ctrl, &clock, 3_000_000).await;
        let after_latency = pool.target();
        assert!(
            after_latency < before_latency,
            "induced latency must shrink the pool: {before_latency} -> {after_latency}"
        );

        // Clear the latency: throughput jumps back up (improving) -> pool recovers.
        run_window(&ctrl, &clock, 60_000_000).await;
        run_window(&ctrl, &clock, 90_000_000).await;
        let recovered = pool.target();
        assert!(
            recovered > after_latency,
            "restoring throughput must regrow the pool: {after_latency} -> {recovered}"
        );
    }

    #[tokio::test]
    async fn controller_holds_below_full_window() {
        // A tick before a full 30s window must not change the pool.
        let clock = Arc::new(FakeClock::new());
        let pool = UploadPool::new(8);
        let probe = ThroughputProbe::new();
        let disk: Arc<dyn DiskBusyProbe> = Arc::new(FakeDiskBusyProbe::not_saturated());
        let pacer: Arc<dyn Pacer> = Arc::new(crate::pacer::AimdPacer::new(clock.clone(), None));
        let ctrl = AdaptiveController::new(pool.clone(), probe.clone(), disk, pacer, clock.clone());

        probe.record_bytes(60_000_000);
        clock.advance(SAMPLE_INTERVAL); // only 5s, not a window
        ctrl.tick().await;
        assert_eq!(pool.target(), 8, "no decision before a full window");
    }

    #[tokio::test]
    async fn controller_does_not_shrink_while_throttled() {
        let clock = Arc::new(FakeClock::new());
        let pool = UploadPool::new(8);
        let probe = ThroughputProbe::new();
        let disk: Arc<dyn DiskBusyProbe> = Arc::new(FakeDiskBusyProbe::not_saturated());
        let pacer: Arc<dyn Pacer> = Arc::new(crate::pacer::AimdPacer::new(clock.clone(), None));
        let ctrl = AdaptiveController::new(
            pool.clone(),
            probe.clone(),
            disk,
            pacer.clone(),
            clock.clone(),
        );

        run_window(&ctrl, &clock, 60_000_000).await;
        run_window(&ctrl, &clock, 60_000_000).await;
        let before = pool.target();
        // Throttle DURING the collapse window: the drop is explained by the pacer.
        pacer.note_response(crate::pacer::ResponseClass::RateLimited {
            retry_after: Duration::from_secs(1),
        });
        run_window(&ctrl, &clock, 3_000_000).await;
        assert_eq!(
            pool.target(),
            before,
            "a throttle-explained drop must not shrink"
        );
    }

    #[tokio::test]
    async fn baseline_not_contaminated_by_nonrepresentative_window() {
        // F3: a non-representative window must not become the throughput baseline.
        let clock = Arc::new(FakeClock::new());
        let pool = UploadPool::new(8);
        let probe = ThroughputProbe::new();
        let disk: Arc<dyn DiskBusyProbe> = Arc::new(FakeDiskBusyProbe::not_saturated());
        let pacer: Arc<dyn Pacer> = Arc::new(crate::pacer::AimdPacer::new(clock.clone(), None));
        let ctrl = AdaptiveController::new(pool.clone(), probe.clone(), disk, pacer, clock.clone());

        // Two representative HIGH windows establish + settle a high baseline
        // (60 MB / 30 s = 2 MB/s).
        run_window(&ctrl, &clock, 60_000_000).await;
        run_window(&ctrl, &clock, 60_000_000).await;
        let size_before = pool.target();

        // A NON-representative low window (pool not the bottleneck): `decide`
        // Holds (rule 1), and the baseline must stay at the HIGH value - not roll
        // down to this window's low throughput.
        run_window_unsaturated(&ctrl, &clock, 1_000_000).await;
        assert_eq!(
            pool.target(),
            size_before,
            "a non-representative window must not resize the pool"
        );

        // A representative window at ~40% of the HIGH baseline (24 MB / 30 s =
        // 0.8 MB/s < 50% of 2 MB/s) must SHRINK - which only happens if the
        // baseline is still the intact HIGH value. Had the low non-representative
        // window contaminated `previous_bps` (to ~0.033 MB/s), 0.8 MB/s would read
        // as a >1.05x improvement and GROW instead, so this assertion is the
        // discriminating check between the fixed and unfixed behaviour.
        run_window(&ctrl, &clock, 24_000_000).await;
        assert!(
            pool.target() < size_before,
            "a collapse measured against the intact high baseline must shrink: \
             {size_before} -> {}",
            pool.target()
        );
    }
}
