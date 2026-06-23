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
    /// The cross-cutting STRESS_HARNESS s6.3 invariant snapshot, when the
    /// scenario captured it. The RUNNER enforces this centrally after EVERY
    /// scenario (P1-C): any tripped invariant flips the verdict to FAIL
    /// regardless of the scenario's own [`ExpectedOutcome`], so a scenario can
    /// never pass while silently losing data, duplicating objects, leaking
    /// pending ops, or failing to quiesce. `None` only for the pure-fuzz /
    /// metadata rows that have no single source+remote to sweep; those carry
    /// their invariant checks inline (see the mutator driver).
    pub invariants: Option<InvariantOutcome>,
    /// Free-form notes a scenario wants surfaced in the human report.
    pub notes: Vec<String>,
}

/// The cross-cutting STRESS_HARNESS s6.3 invariant snapshot the runner
/// enforces after EVERY scenario (P1-C). Every flag must hold; a `false`
/// anywhere is a hard FAIL no matter what the scenario's own
/// [`ExpectedOutcome`] expected.
#[derive(Debug, Clone, Copy)]
pub struct InvariantOutcome {
    /// Every `status='synced'` row still resolves to a live remote object of
    /// the recorded size AND its local file still exists at that size.
    pub no_data_loss: bool,
    /// No two non-trashed remote objects share one `client_op_uuid`.
    pub no_duplicate_op_uuid: bool,
    /// No `pending_ops` row is due-or-past at the end of the run (only
    /// future-scheduled backoff is allowed).
    pub no_pending_leak: bool,
    /// The orchestrator quiesced to a non-running terminal state (no work left
    /// mid-flight, no panic) - the s6.3 clean-shutdown check.
    pub clean_shutdown: bool,
}

impl InvariantOutcome {
    /// Whether every cross-cutting invariant held.
    pub fn all_held(&self) -> bool {
        self.no_data_loss
            && self.no_duplicate_op_uuid
            && self.no_pending_leak
            && self.clean_shutdown
    }

    /// The labels of the invariants that were VIOLATED (for the FAIL diff).
    pub fn violations(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if !self.no_data_loss {
            v.push("no-data-loss");
        }
        if !self.no_duplicate_op_uuid {
            v.push("no-duplicate-remote-object-per-op-uuid");
        }
        if !self.no_pending_leak {
            v.push("no-pending_ops-leak");
        }
        if !self.clean_shutdown {
            v.push("clean-shutdown");
        }
        v
    }
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

    /// The wall-clock cap the runner applies to `run_assertions` (the s6.3
    /// "no infinite loop" guard). Defaults to [`DEFAULT_WALL_CAP`]. The
    /// massive-input rows (`million-files-nested`, `tiny-files-100k-in-one-dir`)
    /// override it: scanning hundreds of thousands to a million real files is a
    /// deterministic, steadily-progressing workload that legitimately takes
    /// longer than the default in a debug build - it is not a hang, so it gets
    /// a larger finite cap rather than a false `harness.timeout`. The guarantee
    /// stays intact: every scenario still has a HARD finite cap.
    fn wall_cap(&self) -> std::time::Duration {
        DEFAULT_WALL_CAP
    }
}

/// Default per-scenario `run_assertions` wall-clock cap (STRESS_HARNESS s6.3
/// "no infinite loop"). Generous because deterministic `FakeClock`-driven
/// cycles are fast, so a scenario that hits this is a genuine hang. The
/// massive-input rows raise it via [`Scenario::wall_cap`].
pub const DEFAULT_WALL_CAP: std::time::Duration = std::time::Duration::from_secs(300);
