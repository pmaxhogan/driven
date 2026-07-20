//! `driven-test-fixtures` - shared test helpers used across the workspace.
//!
//! Modules:
//!
//! - [`tree`]: the [`tree!`] macro for building temp directory trees
//!   declaratively.
//! - [`clock`]: a [`FakeClock`](clock::FakeClock) implementing
//!   [`driven_core::time::Clock`] with `advance()` + `now_set()`.
//! - [`diskstat`]: a [`FakeDiskBusyProbe`](diskstat::FakeDiskBusyProbe)
//!   implementing [`driven_diskstat::DiskBusyProbe`] for the adaptive
//!   upload-parallelism controller tests.
//! - [`power`]: a [`FakePowerSource`](power::FakePowerSource)
//!   implementing [`driven_power::PowerSource`] with a `set()` driver
//!   for state transitions.
//! - [`network`]: the [`FakeNetwork`](network::FakeNetwork) harness for
//!   simulating offline, captive-portal, lossy, per-service-down, and
//!   the other failure modes from DESIGN s5.8.1.
//! - [`assert`]: the [`assert_remote_eq!`] snapshot-diff macro for
//!   asserting on remote-store listings.
//!
//! All public items are documented and exercised by `#[cfg(test)]`
//! examples inside each module. The crate is `publish = false` and is
//! only used as a `[dev-dependencies]` entry from sibling crates and
//! workspace integration tests.

pub mod assert;
pub mod clock;
pub mod diskstat;
pub mod network;
pub mod power;
pub mod tree;
