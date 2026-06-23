//! The [`Scenario`] trait and its outcome types (STRESS_HARNESS s2.3).
//!
//! Most rows in the STRESS_HARNESS s3 catalogue implement [`Scenario`]
//! directly. The `fuzz` and `mutator` drivers compose scenarios
//! programmatically and live in [`crate::mutator`] rather than as
//! `Scenario` impls.
//!
//! A scenario's lifecycle is `setup` -> `run_assertions` -> `teardown`,
//! with `teardown` ALWAYS called (even on assertion failure) so VSS
//! snapshots, OS handles, remote folders, and keychain entries are
//! released. The harness compares the [`Outcome`] returned by
//! `run_assertions` against [`Scenario::expected_outcome`] to compute the
//! PASS / FAIL verdict (STRESS_HARNESS s9).

use async_trait::async_trait;

use driven_core::types::ErrorCode;

use crate::capabilities::CapabilityRequirements;
use crate::handle::DrivenHandle;

/// Per-scenario mutable context threaded through `setup` and `teardown`.
///
/// Carries the scenario's hermetic fixture root (under
/// `target/chaos-fixtures/<name>/` for cacheable fixtures, or a throwaway
/// tempdir otherwise) plus the handles a teardown must release. Phase-2
/// agents extend this with the per-scenario fixture state they build in
/// `setup`; the interface fixes the fields the harness driver itself
/// reads.
#[derive(Debug, Default)]
pub struct ScenarioContext {
    /// On-disk fixture root for this scenario. The source folder(s) the
    /// scenario configures live under here.
    pub fixture_root: std::path::PathBuf,
    /// Whether the fixture under `fixture_root` is reusable across runs
    /// (the expensive `million-files-nested` / `huge-file-10gb` cases) so
    /// the driver knows not to delete it on teardown.
    pub cacheable: bool,
}

/// What `run_assertions` actually observed. Compared against
/// [`ExpectedOutcome`] to decide PASS / FAIL.
///
/// Asserting on stable [`ErrorCode`]s rather than message text is what
/// keeps the harness regression-safe (STRESS_HARNESS s9): error messages
/// can be reworded without breaking a scenario.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// Stable error codes Driven surfaced during the run (SPEC s24).
    pub error_codes_seen: Vec<ErrorCode>,
    /// Count of non-trashed objects in the final remote state.
    pub final_drive_object_count: u64,
    /// Whether every `status='synced'` row's bytes still hash to the
    /// recorded local hash (a per-scenario data-loss check separate from
    /// the cross-cutting invariant in STRESS_HARNESS s6.3).
    pub final_hash_matches_local: bool,
    /// Free-form notes a scenario wants surfaced in the human report.
    pub notes: Vec<String>,
}

/// What the harness expects to observe, driving the PASS / FAIL decision
/// after `run_assertions` returns (STRESS_HARNESS s2.3 / s9).
#[derive(Debug, Clone)]
pub enum ExpectedOutcome {
    /// Driven completes the work with no surfaced error.
    Success,
    /// Driven surfaces exactly this stable error code at least once,
    /// does not crash, and transitions to the documented post-failure
    /// state - then the scenario PASSES (STRESS_HARNESS s9).
    GracefulFailureWith {
        /// The SPEC s24 stable code the scenario expects to see.
        code: ErrorCode,
    },
    /// The scenario only documents current behaviour (e.g. a V1
    /// limitation) and asserts via a snapshot diff rather than an error
    /// code. The closure-free marker keeps the trait object simple; the
    /// scenario's own `run_assertions` carries the real check.
    DocumentedBehaviour,
}

/// One adversarial fixture + assertion bundle (STRESS_HARNESS s2.3).
#[async_trait]
pub trait Scenario: Send + Sync {
    /// Stable kebab-case name. Used as the directory name under
    /// `target/chaos-fixtures/`, as the CLI argument, and in reports.
    fn name(&self) -> &'static str;

    /// Free-form one-liner for `scenario list` and the human report.
    fn description(&self) -> &'static str;

    /// What this scenario needs from the host. Missing capabilities
    /// produce a SKIPPED result with the list of missing items.
    fn requires(&self) -> CapabilityRequirements;

    /// Build the on-disk + remote-side fixture. May be a no-op if a
    /// cached fixture from a prior `fixture create` is still valid.
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()>;

    /// Run assertions against a booted [`DrivenHandle`]. Returns a
    /// structured [`Outcome`] the harness compares against
    /// [`Self::expected_outcome`].
    async fn run_assertions(&self, handle: &DrivenHandle) -> anyhow::Result<Outcome>;

    /// Release filesystem handles, VSS snapshots, remote folders, keychain
    /// entries. Always called, even on assertion failure.
    async fn teardown(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()>;

    /// What the harness expects to observe. Drives the PASS / FAIL
    /// decision after `run_assertions` returns.
    fn expected_outcome(&self) -> ExpectedOutcome;
}
