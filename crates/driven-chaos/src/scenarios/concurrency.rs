//! Concurrency-edge scenarios (STRESS_HARNESS s3.8).
//!
//! `pause-mid-resumable-5m`, `pause-mid-resumable-7d`,
//! `kill-9-mid-pipeline`. These drive `FakeClock.advance` and
//! [`crate::handle::DrivenHandle::kill_orchestrator`] to exercise resumable
//! session expiry and the crash-recovery reconciliation pass (DESIGN s5.6).
//!
//! Phase-2 fills these and returns them from [`scenarios`].

use crate::scenario::Scenario;

/// Every concurrency-edge scenario (STRESS_HARNESS s3.8). Empty until Phase-2.
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    Vec::new()
}
