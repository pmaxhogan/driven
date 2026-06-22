//! Crate-internal test doubles for `driven-core`'s own `#[cfg(test)]` unit
//! tests.
//!
//! `driven-test-fixtures` provides the canonical [`FakeClock`] used across the
//! workspace, but it depends on `driven-core`, so when `driven-core` pulls it
//! in as a dev-dependency the build graph contains a dev-dep cycle: compiling
//! `driven-core`'s own unit tests yields *two* instances of the `driven-core`
//! crate. The fixtures' `FakeClock` then implements the [`Clock`] trait from
//! the dependency instance, not the instance the unit tests link against, so
//! the `Arc<FakeClock> -> Arc<dyn Clock>` coercion fails to resolve
//! ("multiple different versions of crate `driven_core`").
//!
//! Integration tests under `tests/` link the normal (non-cyclic) build and so
//! keep using `driven_test_fixtures::clock::FakeClock`. Only the in-crate unit
//! tests need this same-instance double. It mirrors the fixtures' `FakeClock`
//! semantics exactly (both readings start at zero; `advance` bumps both wall
//! and monotonic by the same `Duration`; `now_set` moves only the wall
//! reading) so the timing behaviour the pacer/network tests rely on is
//! identical.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::time::Clock;
use crate::types::UnixMs;

#[derive(Debug, Default)]
struct ClockInner {
    /// Wall-clock reading in Unix epoch ms; tests drive this freely.
    wall_ms: UnixMs,
    /// Monotonic reading in nanoseconds from a test-local origin (zero).
    mono_ns: u128,
}

/// Same-crate-instance deterministic [`Clock`] for `driven-core` unit tests.
///
/// Cheap to [`Clone`]: shares state through an [`Arc`] so the same logical
/// clock can be handed to a subject under test and still driven from the test
/// body. Mirrors `driven_test_fixtures::clock::FakeClock`.
#[derive(Debug, Clone, Default)]
pub(crate) struct FakeClock {
    inner: Arc<Mutex<ClockInner>>,
}

impl FakeClock {
    /// Constructs a fresh clock with `wall_ms = 0` and `mono_ns = 0`.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Advances both the wall and the monotonic reading by `d`.
    ///
    /// The "real time passes" knob: wall ms gain `d.as_millis()` and monotonic
    /// ns gain `d.as_nanos()`, saturating.
    pub(crate) fn advance(&self, d: Duration) {
        let mut g = self.lock();
        let ms_delta = d.as_millis() as i64;
        g.wall_ms = g.wall_ms.saturating_add(ms_delta);
        g.mono_ns = g.mono_ns.saturating_add(d.as_nanos());
    }

    /// Sets the wall reading to `wall_ms` without touching the monotonic
    /// reading (models an NTP / DST / user-edit wall-clock jump).
    #[allow(dead_code)]
    pub(crate) fn now_set(&self, wall_ms: UnixMs) {
        self.lock().wall_ms = wall_ms;
    }

    /// Locks the inner state, recovering the guard on poison (a panicking
    /// test thread is already a failure; we just want the reading, not a
    /// double-panic).
    fn lock(&self) -> std::sync::MutexGuard<'_, ClockInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> UnixMs {
        self.lock().wall_ms
    }

    fn now_monotonic_ns(&self) -> u128 {
        self.lock().mono_ns
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
