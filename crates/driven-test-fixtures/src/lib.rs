//! `driven-test-fixtures` — shared test helpers used across the workspace.
//!
//! Provides the `tree!()` macro for building temp directory trees, a
//! `FakeClock` for deterministic time control, a `FakePowerSource` for
//! simulating battery / AC / metered / offline transitions, a fake
//! network harness for simulating offline / captive-portal /
//! per-service-down conditions, and `assert_remote_eq!()` snapshot
//! helpers for diffing remote state.
