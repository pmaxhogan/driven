//! Scenario registry for the app-level e2e suite.

mod basic;
mod faults;
mod s3;

use crate::scenario::Scenario;

/// Every registered scenario, in run order (cheap smoke first).
pub fn all() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(basic::WizardFirstRun),
        Box::new(basic::LocalFolderRoundTrip),
        Box::new(basic::SettingsPersistence),
        Box::new(faults::FakeDriveOutageSurfaced),
        Box::new(faults::SourceFileUnreadable),
        Box::new(faults::DestDiskFull),
        Box::new(s3::S3RoundTrip),
        Box::new(s3::S3NetworkCutMidSync),
    ]
}
