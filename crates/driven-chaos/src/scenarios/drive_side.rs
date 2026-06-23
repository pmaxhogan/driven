//! Drive-side fault scenarios (STRESS_HARNESS s3.7).
//!
//! `dest-folder-deleted`, `access-revoked`, `dest-folder-readonly`,
//! `dest-folder-moved`, `trash-emptied-with-our-file`,
//! `storage-quota-mid-upload`, `daily-quota-exhausted`,
//! `concurrent-driven-instance-on-other-machine`, `drive-fileid-recycled`,
//! `concurrent-rename-on-drive`. These bind the fake's fault-injection
//! builders (STRESS_HARNESS s5); the dest-folder rows expect the new
//! `drive.dest_folder_missing` / `drive.dest_folder_permission_denied`
//! codes (SPEC s24 / STRESS_HARNESS s10).
//!
//! Phase-2 fills these and returns them from [`scenarios`].

use crate::scenario::Scenario;

/// Every Drive-side fault scenario (STRESS_HARNESS s3.7). Empty until Phase-2.
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    Vec::new()
}
