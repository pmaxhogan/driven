//! `driven-core` - the I/O-free heart of Driven.
//!
//! Owns the sync state machine, scanner, planner, orchestrator, pacer,
//! scheduler, activity-log writer, exclusion rules, pending-ops queue,
//! deep-verify cycle, filesystem watcher, and the SQLite state layer.
//!
//! All real I/O (filesystem reads, network calls, OS clock, OS keychain,
//! power-source signals) flows through injected traits so the whole crate
//! is exercisable from plain `cargo test --workspace` with no Tauri shell,
//! no real Google Drive, and no real wall clock.
//!
//! The crate is layered as a set of *contract* traits plus their concrete
//! implementations, so each subsystem is swappable in tests: the shared types
//! and the [`OrchestratorState`](types::OrchestratorState) machine, the
//! [`Clock`](time::Clock) and [`StateRepo`](state::StateRepo) seams, the
//! [`scanner`] / [`exclude`] / [`planner`] read side, and the
//! [`pacer::Pacer`], [`executor::Executor`], [`orchestrator::Orchestrator`],
//! [`watcher::SourceWatcher`], and [`network::NetworkProbe`] traits that drive
//! the write side. All of them are implemented; the traits exist for
//! substitution, not because anything is still a stub.

pub mod adaptive;
pub mod bundle;
pub mod crypto_provider;
pub mod exclude;
pub mod executor;
pub mod hooks;
pub mod network;
pub mod orchestrator;
pub mod pacer;
pub mod planner;
pub mod priority;
pub mod scanner;
pub mod state;
pub mod telemetry;
pub mod time;
pub mod types;
pub mod watcher;

pub use crypto_provider::{CryptoProvider, CryptoResolution, SingleSuiteProvider};

#[cfg(test)]
mod test_support;
