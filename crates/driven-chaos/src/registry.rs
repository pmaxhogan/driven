//! The scenario registry the subcommand dispatch iterates (STRESS_HARNESS s2.2).
//!
//! [`registry`] gathers every category's scenarios (STRESS_HARNESS s3) into
//! one ordered list. `scenario list` prints it, `scenario run <name>` looks
//! one up by [`crate::scenario::Scenario::name`], and `scenario run-all`
//! iterates it respecting capability gates.
//!
//! The categories return empty vectors in the Phase-1 interface; as Phase-2
//! fills each category module the registry grows automatically - no central
//! edit needed beyond the per-category `scenarios()` calls below.

use crate::scenario::Scenario;
use crate::scenarios;

/// Every registered scenario, in category order (STRESS_HARNESS s3.1..s3.8).
pub fn registry() -> Vec<Box<dyn Scenario>> {
    let mut all: Vec<Box<dyn Scenario>> = Vec::new();
    all.extend(scenarios::storage::scenarios());
    all.extend(scenarios::file_size::scenarios());
    all.extend(scenarios::permissions::scenarios());
    all.extend(scenarios::filenames::scenarios());
    all.extend(scenarios::ntfs::scenarios());
    all.extend(scenarios::mutation::scenarios());
    all.extend(scenarios::drive_side::scenarios());
    all.extend(scenarios::concurrency::scenarios());
    all
}

/// Look one scenario up by its stable name (the `scenario run <name>` and
/// `fixture create <name>` argument). Returns `None` if unknown.
pub fn find(name: &str) -> Option<Box<dyn Scenario>> {
    registry().into_iter().find(|s| s.name() == name)
}
