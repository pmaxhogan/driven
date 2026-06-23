//! NTFS / Win32 hazard scenarios (STRESS_HARNESS s3.5).
//!
//! Hardlinks, symlinks, junctions, reparse points, OneDrive placeholders,
//! recursive junction cycles, cross-volume links, ADS, sparse / compressed
//! / EFS files, hidden+system attributes, file-id reuse after defrag. The
//! ADS row expects the new `local.ads_skipped` code (SPEC s24 /
//! STRESS_HARNESS s10).
//!
//! Phase-2 fills these and returns them from [`scenarios`].

use crate::scenario::Scenario;

/// Every NTFS / Win32 hazard scenario (STRESS_HARNESS s3.5). Empty until Phase-2.
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    Vec::new()
}
