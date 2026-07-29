//! `driven-chaos` - Driven's stress / chaos test harness library
//! (STRESS_HARNESS s2).
//!
//! The harness boots the HEADLESS core (DESIGN s4.2 thick-core / thin-shell
//! split) - it depends on `driven-core` / `driven-drive` / `driven-crypto`
//! / `driven-power` / `driven-test-fixtures` and never on `src-tauri`.
//!
//! Module map:
//!
//! [`capabilities`] - the host [`capabilities::CapabilitySet`] probe (s2.5)
//! [`scenario`] - the [`scenario::Scenario`] trait + outcome types (s2.3)
//! [`handle`] - [`handle::DrivenHandle`], a booted headless instance (s2.4)
//! [`registry`] - the scenario registry the dispatch iterates (s2.2)
//! [`reporting`] - per-scenario [`reporting::Verdict`] + run report (s6)
//! [`mutator`] - the FS / Drive mutation command enums (s4)
//! [`s3_server`] - an in-process, fault-injecting S3 server (s5.1), so the S3
//! backend's wire-level quirks can be injected against the REAL
//! `driven_s3::S3Store`
//! [`localfs_fixture`] - a local-folder destination fixture + its fault-free
//! oracle (s5.2), so the removable-media and crash-mid-commit states can be
//! built on disk exactly as the failure leaves them
//! [`scenarios`] - the s3 catalogue, one submodule per category
//!
//! The Phase-1 interface fixes these types and trait signatures; the
//! Phase-2 implementer agents fill the scenario bodies and the mutation /
//! report rendering.

pub mod capabilities;
pub mod dispatch;
pub mod handle;
pub mod localfs_fixture;
pub mod mutator;
pub mod registry;
pub mod reporting;
pub mod runner;
pub mod s3_server;
pub mod scenario;
pub mod scenarios;
