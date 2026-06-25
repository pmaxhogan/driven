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
//! Implementation is milestoned. M1 landed the shared types, the
//! [`Clock`](time::Clock) abstraction, and the [`StateRepo`](state::StateRepo)
//! surface; M2 landed the [`scanner`], [`exclude`] rules, and [`planner`].
//! M3 phase 1 (interfaces only) adds the orchestrator/pacer/executor/
//! watcher/network *contract* surface - the [`OrchestratorState`](types::OrchestratorState)
//! machine + event/progress types, the [`pacer::Pacer`], [`executor::Executor`],
//! [`orchestrator::Orchestrator`], [`watcher::SourceWatcher`], and
//! [`network::NetworkProbe`] traits - with no behaviour; the bodies land in
//! the M3 implement phase.

pub mod crypto_provider;
pub mod exclude;
pub mod executor;
pub mod hooks;
pub mod network;
pub mod orchestrator;
pub mod pacer;
pub mod planner;
pub mod scanner;
pub mod state;
pub mod time;
pub mod types;
pub mod watcher;

pub use crypto_provider::{CryptoProvider, CryptoResolution, SingleSuiteProvider};

#[cfg(test)]
mod test_support;
