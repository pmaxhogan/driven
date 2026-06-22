//! Deterministic [`Clock`](driven_core::time::Clock) for tests.
//!
//! [`FakeClock`] holds an independent wall reading and monotonic reading,
//! mirroring the two-clock contract in
//! [`driven_core::time`](driven_core::time): wall (`now_ms`) is the Unix
//! epoch reading that can be jumped backwards or forwards by NTP / DST /
//! user edits; monotonic (`now_monotonic_ns`) is process-local and
//! non-decreasing. Scanner / deep-verify tests rely on being able to
//! drive these independently (DESIGN s18.7 - a backwards wall jump must
//! not make the scanner think no time has passed).
//!
//! Three test affordances:
//!
//! - [`FakeClock::new`] starts both readings at zero.
//! - [`FakeClock::advance`] advances **both** readings by the given
//!   [`Duration`](std::time::Duration). Use this for normal "time
//!   passes" tests.
//! - [`FakeClock::now_set`] sets the wall reading to a specific Unix
//!   epoch ms **without touching the monotonic reading**. Use this to
//!   simulate a wall-clock edit / DST jump that the monotonic clock did
//!   not observe.
//!
//! Interior mutability uses [`parking_lot::Mutex`]; the [`Clock`] impl
//! reads through the mutex on every call. The lock is held for a few
//! atomic loads, so contention is a non-issue at test scale.

use std::sync::Arc;
use std::time::Duration;

use driven_core::time::Clock;
use driven_core::types::UnixMs;
use parking_lot::Mutex;

#[derive(Debug, Default)]
struct ClockInner {
    /// Wall-clock reading in Unix epoch ms; tests drive this freely.
    wall_ms: UnixMs,
    /// Monotonic reading in nanoseconds from a test-local origin (zero).
    mono_ns: u128,
}

/// Test clock implementing [`Clock`].
///
/// Cheap to [`Clone`]: shares state through an [`Arc`] so the same logical
/// clock can be handed to an orchestrator under test and still driven from
/// the test body.
///
/// ```ignore
/// use std::time::Duration;
/// use driven_core::time::Clock;
/// use driven_test_fixtures::clock::FakeClock;
///
/// let clock = FakeClock::new();
/// assert_eq!(clock.now_ms(), 0);
/// clock.advance(Duration::from_secs(5));
/// assert_eq!(clock.now_ms(), 5_000);
/// // Simulate a wall-clock edit that the monotonic clock did not see:
/// let mono_before = clock.now_monotonic_ns();
/// clock.now_set(1_000_000);
/// assert_eq!(clock.now_ms(), 1_000_000);
/// assert_eq!(clock.now_monotonic_ns(), mono_before);
/// ```
#[derive(Debug, Clone, Default)]
pub struct FakeClock {
    inner: Arc<Mutex<ClockInner>>,
}

impl FakeClock {
    /// Constructs a fresh clock with `wall_ms = 0` and `mono_ns = 0`.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ClockInner {
                wall_ms: 0,
                mono_ns: 0,
            })),
        }
    }

    /// Advances both the wall and the monotonic reading by `d`.
    ///
    /// This is the "real time passes" knob - use it whenever a test
    /// would otherwise sleep. Wall ms gain `d.as_millis()` and monotonic
    /// ns gain `d.as_nanos()`.
    pub fn advance(&self, d: Duration) {
        let mut g = self.inner.lock();
        // `as i64` saturating to i64::MAX is fine for any realistic test
        // budget (i64::MAX ms is ~292 million years).
        let ms_delta = d.as_millis() as i64;
        g.wall_ms = g.wall_ms.saturating_add(ms_delta);
        g.mono_ns = g.mono_ns.saturating_add(d.as_nanos());
    }

    /// Sets the wall reading to `wall_ms` without touching the monotonic
    /// reading.
    ///
    /// Models a wall-clock edit (NTP correction, DST flip, user edit) -
    /// the kind of jump DESIGN s18.7 explicitly requires the scanner to
    /// tolerate.
    pub fn now_set(&self, wall_ms: UnixMs) {
        let mut g = self.inner.lock();
        g.wall_ms = wall_ms;
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> UnixMs {
        self.inner.lock().wall_ms
    }

    fn now_monotonic_ns(&self) -> u128 {
        self.inner.lock().mono_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_at_zero() {
        let c = FakeClock::new();
        assert_eq!(c.now_ms(), 0);
        assert_eq!(c.now_monotonic_ns(), 0);
    }

    #[test]
    fn advance_bumps_both_readings() {
        let c = FakeClock::new();
        c.advance(Duration::from_millis(1_500));
        assert_eq!(c.now_ms(), 1_500);
        assert_eq!(c.now_monotonic_ns(), 1_500_000_000);
    }

    #[test]
    fn now_set_only_touches_wall() {
        let c = FakeClock::new();
        c.advance(Duration::from_secs(10));
        let mono_before = c.now_monotonic_ns();
        c.now_set(42);
        assert_eq!(c.now_ms(), 42);
        assert_eq!(c.now_monotonic_ns(), mono_before);
    }

    #[test]
    fn clone_shares_state() {
        let a = FakeClock::new();
        let b = a.clone();
        a.advance(Duration::from_secs(1));
        assert_eq!(b.now_ms(), 1_000);
    }
}
