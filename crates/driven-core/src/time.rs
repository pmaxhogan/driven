//! Clock abstraction for deterministic testing.
//!
//! Production code uses [`SystemClock`]; tests use the `FakeClock`
//! provided by `driven-test-fixtures` (M1 phase 2). The split keeps the
//! sync engine exercisable from `cargo test --workspace` with no real
//! wall clock.
//!
//! Clock-change handling contract (DESIGN s18.7):
//! the wall clock can move backwards (DST, NTP correction, user-edit) so
//! anywhere a "has time passed?" decision is made the caller must consult
//! BOTH [`Clock::now_ms`] (wall) AND [`Clock::now_monotonic_ns`]
//! (monotonic), and use `max(wall_delta, monotonic_delta)`. A backwards
//! wall jump must not make the scanner think no time has passed; a
//! forwards wall jump that the monotonic clock didn't see must not make
//! the scanner think a deep-verify is overdue. The full rules
//! (re-hash window on a backwards jump greater than 60 s, etc.) live in
//! the scanner; the contract this module establishes is just that
//! both readings are available.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::types::UnixMs;

/// Time source for the sync engine.
///
/// Implementations must be cheap to call on a hot path. The orchestrator
/// reads the clock once per state transition and once per pacer permit;
/// the scanner reads it once per file.
pub trait Clock: Send + Sync {
    /// Returns the current wall-clock time as Unix epoch milliseconds.
    ///
    /// May move backwards. Always pair with [`Self::now_monotonic_ns`]
    /// per DESIGN s18.7 when reasoning about elapsed time.
    fn now_ms(&self) -> UnixMs;

    /// Returns a monotonically-non-decreasing nanosecond reading from a
    /// process-local epoch.
    ///
    /// The epoch is implementation-defined; callers must only ever take
    /// differences between readings, not interpret absolute values. The
    /// reading is unaffected by wall-clock edits, NTP correction, or DST.
    fn now_monotonic_ns(&self) -> u128;
}

/// Production [`Clock`] backed by [`SystemTime`] and [`Instant`].
///
/// Construct via `SystemClock` (it is a unit struct). The monotonic
/// reading uses a process-local `Instant` captured the first time
/// [`Clock::now_monotonic_ns`] is called, then returns nanoseconds since
/// that point.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

fn monotonic_origin() -> Instant {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    *ORIGIN.get_or_init(Instant::now)
}

impl Clock for SystemClock {
    fn now_ms(&self) -> UnixMs {
        // Pre-epoch readings are treated as the epoch (SystemTime can in
        // principle be moved before 1970 by a misconfigured machine).
        // We deliberately avoid `.expect()` per the house rule.
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_millis() as i64,
            Err(_) => 0,
        }
    }

    fn now_monotonic_ns(&self) -> u128 {
        monotonic_origin().elapsed().as_nanos()
    }
}
