//! Permissions and ACL scenarios (STRESS_HARNESS s3.3).
//!
//! `windows-acl-deny-read-file`, `posix-mode-000`,
//! `windows-acl-deny-enumerate`, `setuid-files`. The deny-enumerate row
//! exercises the DESIGN s5.2 delete-suppression path.
//!
//! Phase-2 fills these and returns them from [`scenarios`].

use crate::scenario::Scenario;

/// Every permissions/ACL scenario (STRESS_HARNESS s3.3). Empty until Phase-2.
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    Vec::new()
}
