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
//! M1 phase 1 (interfaces only): this crate currently exposes shared
//! types, the [`Clock`](time::Clock) abstraction, and the
//! [`StateRepo`](state::StateRepo) trait surface. Concrete sync-engine
//! modules (scanner, planner, orchestrator, ...) land in later milestones.

pub mod state;
pub mod time;
pub mod types;
