//! Storage and disk scenarios (STRESS_HARNESS s3.1).
//!
//! `disk-full-target`, `readonly-source-folder`, `readonly-file`,
//! `noaccess-file`, `noaccess-folder`. The `disk-full-target` row expects
//! the new `local.disk_full` code (SPEC s24 / STRESS_HARNESS s10).
//!
//! Phase-2 fills these and returns them from [`scenarios`].

use crate::scenario::Scenario;

/// Every storage/disk scenario (STRESS_HARNESS s3.1). Empty until Phase-2.
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    Vec::new()
}
