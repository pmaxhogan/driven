//! File-size-extreme scenarios (STRESS_HARNESS s3.2).
//!
//! `huge-file-10gb`, `huge-file-50gb-mid-run-crash`,
//! `tiny-files-100k-in-one-dir`, `million-files-nested`. The big fixtures
//! are cacheable under `target/chaos-fixtures/` (STRESS_HARNESS s2.2).
//!
//! Phase-2 fills these and returns them from [`scenarios`].

use crate::scenario::Scenario;

/// Every file-size-extreme scenario (STRESS_HARNESS s3.2). Empty until Phase-2.
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    Vec::new()
}
