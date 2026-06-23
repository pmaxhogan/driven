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
    // s4 continuous-mutation soak / fuzz scenarios and s6.3 cross-scenario
    // invariant scenarios are registered alongside the s3 catalogue so
    // `scenario list` / `run` / `run-all` see them too.
    all.extend(scenarios::mutator::scenarios());
    all.extend(scenarios::reporting::scenarios());
    all
}

/// The fault-injection subset (STRESS_HARNESS s3.7 Drive-side hazards + s4.2/s5
/// drive-side mutator faults) the dedicated `chaos-fake-drive` CI job runs
/// (ROADMAP M3.7 acceptance: a distinct fake-drive gate on Linux + macOS +
/// Windows that "adds the fault-injection scenarios" against
/// `InMemoryRemoteStore`). These rows are ALSO covered by the full `run-all`
/// hermetic sweep; this focused selection gives the separately-named,
/// faster-feedback gate the acceptance requires.
pub fn fault_injection_registry() -> Vec<Box<dyn Scenario>> {
    let mut all: Vec<Box<dyn Scenario>> = Vec::new();
    all.extend(scenarios::drive_side::scenarios());
    all.extend(scenarios::mutator::scenarios());
    all
}

/// Look one scenario up by its stable name (the `scenario run <name>` and
/// `fixture create <name>` argument). Returns `None` if unknown.
pub fn find(name: &str) -> Option<Box<dyn Scenario>> {
    registry().into_iter().find(|s| s.name() == name)
}
