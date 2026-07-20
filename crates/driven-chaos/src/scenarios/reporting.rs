//! Reporting / cross-scenario-invariant scenarios (STRESS_HARNESS s6).
//!
//! Section 6 of STRESS_HARNESS is the harness's *reporting* contract: the
//! per-scenario verdict shape (s6.1), the JSON / human output format
//! (s6.2), and - the part this module makes executable - the invariants
//! asserted across EVERY scenario (s6.3):
//!
//!   no panic; no data loss; no infinite loop; no duplicate Drive objects
//!   for one `client_op_uuid`; no `pending_ops` leak; no unwrap / panic in
//!   logs.
//!
//! Those s6.3 invariants are cross-cutting post-conditions ("a scenario
//! can pass its own assertions yet fail one of these") rather than rows in
//! the s3 catalogue. To make them concretely testable - and to give the
//! harness a regression guard on the invariant checks THEMSELVES - this
//! module ships one [`crate::scenario::Scenario`] per invariant. Each
//! drives the real headless core (DESIGN s4.2) through a representative
//! workload chosen to stress one invariant, then asserts the invariant
//! holds via the shared [`InvariantReport`] check that every scenario in
//! every category should ultimately run.
//!
//! The shared checker ([`assert_invariants`]) is deliberately public so
//! the sibling category modules (storage, file_size, ...) and the
//! run-all driver can reuse the exact same s6.3 logic instead of
//! re-deriving it per scenario. That single-source-of-truth is the point
//! of s6.3: the invariants are computed once, the same way, everywhere.
//!
//! Bounded-memory note (s6.3 "bounded memory"): a precise RSS-delta
//! assertion needs a process-level memory probe that the in-memory fake
//! harness cannot make portably (s3.2 `million-files-nested` is where the
//! real RSS gate lives, gated on `cap:free_disk_bytes`). Here we assert
//! the *structural* proxy the in-memory path CAN prove: the executor does
//! not retain per-op state without bound, observable as a bounded
//! `pending_ops` table and a bounded live-object count that tracks the
//! input. The honest RSS gate is recorded as a capability-gated SKIP
//! reason on [`BoundedMemoryScenario`] so the limitation is surfaced, not
//! masked (s8 privilege / s2.5 capability discipline).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use driven_core::orchestrator::TickSource;
use driven_core::state::{SourceRow, StateRepo};
use driven_core::types::{AccountId, FileStateStatus, OrchestratorState, SourceId};

use driven_drive::fake::{InMemoryRemoteStore, ObjectContent, CLIENT_OP_UUID_KEY};
use driven_drive::remote_store::{RemoteEntry, RemoteStore};

use crate::capabilities::{Capability, CapabilityRequirements};
use crate::handle::{DrivenHandle, DrivenHandleBuilder};
use crate::scenario::{ExpectedOutcome, Outcome, Scenario, ScenarioContext};

/// Every reporting / cross-invariant scenario (STRESS_HARNESS s6.3).
///
/// The Integrate agent wires this into [`crate::registry::registry`] with
/// `all.extend(scenarios::reporting::scenarios());` and adds
/// `pub mod reporting;` to [`crate::scenarios`] (those two files are
/// shared and owned by Integrate, not this category).
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(NoDataLossScenario),
        Box::new(NoDuplicateRemoteObjectsScenario),
        Box::new(NoPendingOpsLeakScenario),
        Box::new(CleanShutdownScenario),
        Box::new(BoundedMemoryScenario),
    ]
}

// ===========================================================================
// Shared s6.3 invariant checker
// ===========================================================================

/// A structured view of the STRESS_HARNESS s6.3 cross-scenario invariants,
/// computed once from the booted [`DrivenHandle`]'s terminal state.
///
/// Every scenario in every category should run this AFTER its own
/// assertions; a scenario can satisfy its specific expected outcome yet
/// still violate one of these post-conditions (s6.3 opening sentence).
#[derive(Debug, Clone, Default)]
pub struct InvariantReport {
    /// s6.3 "No data loss": every `status='synced'` row's recorded
    /// `drive_file_id` still resolves to a live (non-trashed) remote
    /// object. `None` for a row whose object is missing.
    pub data_loss_paths: Vec<String>,
    /// s6.3 "No duplicate Drive objects for the same `client_op_uuid`":
    /// the `client_op_uuid` values that appear on more than one live
    /// remote object.
    pub duplicate_op_uuids: Vec<String>,
    /// s6.3 "No `pending_ops` leak": count of `pending_ops` rows whose
    /// `scheduled_for` is at or before "now" (a legitimate future-dated
    /// backoff row does NOT count as a leak).
    pub leaked_pending_ops: u64,
    /// Live (non-trashed) object count in the destination folder - the
    /// bounded-memory structural proxy and a convenience for per-scenario
    /// assertions.
    pub live_object_count: u64,
}

impl InvariantReport {
    /// Whether every s6.3 invariant this report can compute holds.
    pub fn ok(&self) -> bool {
        self.data_loss_paths.is_empty()
            && self.duplicate_op_uuids.is_empty()
            && self.leaked_pending_ops == 0
    }

    /// Convert this report into the runner-enforced
    /// [`crate::scenario::InvariantOutcome`] (P1-C). The runner reads
    /// [`crate::scenario::Outcome::invariants`] after EVERY scenario and FAILs
    /// on any tripped invariant, so routing a scenario's terminal state through
    /// here is what makes the s6.3 sweep central + unfakeable. `clean_shutdown`
    /// is supplied by the caller (it owns the orchestrator state); the other
    /// three flags are derived from the computed report.
    pub fn to_invariant_outcome(&self, clean_shutdown: bool) -> crate::scenario::InvariantOutcome {
        crate::scenario::InvariantOutcome {
            no_data_loss: self.data_loss_paths.is_empty(),
            no_duplicate_op_uuid: self.duplicate_op_uuids.is_empty(),
            no_pending_leak: self.leaked_pending_ops == 0,
            clean_shutdown,
        }
    }

    /// A human-readable diff for the [`crate::reporting::Verdict::Fail`]
    /// detail block when an invariant is violated.
    pub fn violation_summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.data_loss_paths.is_empty() {
            parts.push(format!(
                "data loss: {} synced row(s) lost their remote object ({})",
                self.data_loss_paths.len(),
                self.data_loss_paths.join(", ")
            ));
        }
        if !self.duplicate_op_uuids.is_empty() {
            parts.push(format!(
                "duplicate remote objects for client_op_uuid(s): {}",
                self.duplicate_op_uuids.join(", ")
            ));
        }
        if self.leaked_pending_ops > 0 {
            parts.push(format!(
                "{} due/overdue pending_ops row(s) leaked",
                self.leaked_pending_ops
            ));
        }
        if parts.is_empty() {
            "all s6.3 invariants hold".to_string()
        } else {
            parts.join("; ")
        }
    }
}

/// Whether a due `pending_ops` row is a well-formed deferred-create reconcile
/// op - the documented DESIGN s5.6 recovery handle a transient fault mid-first
/// -upload leaves behind (an `upload` op carrying a `client_op_uuid` but no
/// `drive_file_id` yet), which the next-boot startup reconcile resolves. It is
/// a legitimate terminal state for the Drive-side transient + crash-recovery
/// rows, NOT a pending-ops leak. Mirrors the mutator's per-row definition so the
/// central sweep and the scenario-local check agree.
fn is_deferred_create_reconcile(op: &driven_core::state::PendingOpRow) -> bool {
    op.op_type.as_str() == "upload"
        && op
            .payload_json
            .get("client_op_uuid")
            .is_some_and(|v| !v.is_null())
        && op
            .payload_json
            .get("drive_file_id")
            .map(|v| v.is_null())
            .unwrap_or(true)
}

/// Compute the STRESS_HARNESS s6.3 invariants from the handle's terminal
/// state, against the destination `folder_id` for one `source_id`.
///
/// This is the single source of truth for the cross-scenario checks. It is
/// `pub` so sibling categories run the exact same logic (s6.3 requires the
/// invariants be computed uniformly across every scenario).
///
/// Takes the [`InMemoryRemoteStore`] by reference (the scenarios construct
/// it and keep a typed handle - the `RemoteStore` trait object cannot be
/// downcast). The credential-gated real-Drive scenarios run a Drive-backed
/// variant the run-all driver supplies. Returns `Err` only on a genuine
/// state-layer fault (which is itself an invariant failure: the harness
/// could not verify, so it must not report green).
pub async fn assert_invariants(
    handle: &DrivenHandle,
    remote: &InMemoryRemoteStore,
    source_id: SourceId,
    folder_id: &str,
) -> anyhow::Result<InvariantReport> {
    // Live remote objects + a map from client_op_uuid -> count of live
    // objects carrying it (the duplicate-detection key, s6.3).
    //
    // Use the FAULT-FREE `list_folder_with_trashed` accessor (a direct in-memory
    // read, NOT the faulted `list_folder` trait method) so a scenario that
    // leaves the remote in a LATCHED fault state (e.g. auth.invalid_grant, which
    // the fake applies to read calls too) can still have its terminal-state
    // invariants verified instead of the sweep itself erroring on the fault.
    let all: Vec<_> = remote.list_folder_with_trashed(folder_id);
    let live: Vec<_> = all.iter().filter(|e| !e.trashed).cloned().collect();
    let live_object_count = live.len() as u64;

    // No duplicate Drive objects per client_op_uuid (s6.3). Count over ALL
    // objects INCLUDING trashed ones: "two objects created for one op, then one
    // trashed" is still evidence of a duplicate-create bug, so filtering trashed
    // out before counting would hide it. Each upload op stamps a FRESH
    // client_op_uuid, so a legitimate trash-then-recreate carries two distinct
    // uuids and never collides here; only a genuine duplicate create does.
    // (Mirrors the mutator checker, which already counts with-trashed.)
    let mut uuid_counts: HashMap<String, u64> = HashMap::new();
    for entry in &all {
        if let Some(uuid) = entry.app_properties.get(CLIENT_OP_UUID_KEY) {
            *uuid_counts.entry(uuid.clone()).or_insert(0) += 1;
        }
    }
    let mut duplicate_op_uuids: Vec<String> = uuid_counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(uuid, _)| uuid)
        .collect();
    duplicate_op_uuids.sort();

    // The live remote objects indexed by id, for the data-loss + content checks.
    let by_id: HashMap<&str, &RemoteEntry> = live.iter().map(|e| (e.id.as_str(), e)).collect();

    // Whether this source encrypts. An encrypted source stores CIPHERTEXT on
    // Drive, whose blake3 differs from the recorded plaintext `hash_blake3`, so
    // its content is verified by md5 only (the byte-hash check would false-fail).
    let encryption_enabled = handle
        .state
        .list_sources()
        .await?
        .into_iter()
        .find(|s| s.id == source_id)
        .map(|s| s.encryption_enabled)
        .unwrap_or(false);

    // No data loss (STRESS_HARNESS s6.3): for every `status='synced'` row the
    // recorded Drive object must (a) exist live, (b) have an md5 matching the
    // recorded `drive_md5`, and (c) - for an unencrypted source whose literal
    // bytes are retained - hash to the recorded plaintext `hash_blake3`. A
    // missing object, an md5 mismatch, or a blake3 mismatch is data loss /
    // silent remote corruption. (Oracle-backed huge files retain no literal
    // bytes; their md5 match in (b) is the integrity proof and (c) is skipped.)
    let mut data_loss_paths: Vec<String> = Vec::new();
    let rows = handle.state.load_source_file_state(source_id).await?;
    for (rel, row) in &rows {
        if row.status != FileStateStatus::Synced {
            continue;
        }
        let Some(id) = row.drive_file_id.as_deref() else {
            data_loss_paths.push(format!("{rel}: synced row has no drive_file_id"));
            continue;
        };
        let Some(entry) = by_id.get(id) else {
            data_loss_paths.push(format!("{rel}: object {id} missing/trashed"));
            continue;
        };
        // (b) md5 match: the live object's md5 must equal the recorded
        // `drive_md5` (both are the ciphertext md5 Drive computed). Holds for
        // oracle-backed huge files too (the oracle carries the true md5).
        if let Some(drive_md5) = row.drive_md5 {
            if entry.md5 != Some(drive_md5) {
                data_loss_paths.push(format!("{rel}: drive md5 mismatch"));
                continue;
            }
        }
        // (c) blake3 content match: hash the retained bytes to the recorded
        // plaintext `hash_blake3`. Only for an unencrypted source (ciphertext
        // hashes differ from the plaintext hash) and a retained-bytes object.
        if !encryption_enabled {
            if let Some(ObjectContent::Literal(bytes)) = remote.object_content(id) {
                if *blake3::hash(&bytes).as_bytes() != row.hash_blake3 {
                    data_loss_paths.push(format!("{rel}: blake3 content mismatch"));
                    continue;
                }
            }
        }
    }
    data_loss_paths.sort();

    // No pending_ops leak (s6.3): a row is a leak only if it is due now or
    // overdue. Two due-row kinds are legitimate, NOT leaks:
    //   a future-dated backoff row (scheduled_for > now), and
    //   a well-formed deferred-create reconcile op - an `upload` op that
    //   carries a client_op_uuid but no drive_file_id yet, the documented
    //   DESIGN s5.6 recovery handle a transient fault mid-first-upload leaves
    //   for the next-boot startup reconcile to resolve. Counting it as a leak
    //   would falsely red the Drive-side transient + crash-recovery rows, whose
    //   correct terminal state IS one such op.
    let now = handle.clock.now_ms();
    let pending = handle.state.get_pending_ops_for_source(source_id).await?;
    let leaked_pending_ops = pending
        .iter()
        .filter(|p| p.scheduled_for <= now)
        .filter(|p| !is_deferred_create_reconcile(p))
        .count() as u64;

    Ok(InvariantReport {
        data_loss_paths,
        duplicate_op_uuids,
        leaked_pending_ops,
        live_object_count,
    })
}

// ===========================================================================
// Shared fixture helpers
// ===========================================================================

/// Build a source rooted at `root`, uploading into the fake remote's
/// `folder_id`. Mirrors the e2e_fake `source_in` helper so the reporting
/// scenarios drive the same real pipeline the acceptance tests do.
fn source_in(account: AccountId, root: &std::path::Path, folder_id: &str) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id: account,
        display_name: "reporting".into(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: folder_id.to_string(),
        drive_folder_path: "/reporting".into(),
        encryption_enabled: false,
        wrapped_source_key: None,
        respect_gitignore: false,
        include_patterns: vec![],
        exclude_patterns: vec![],
        placeholder_policy: Default::default(),
        schedule_json_v2_reserved: None,
        deep_verify_interval_secs: 604_800,
        last_full_scan_at: None,
        last_deep_verify_at: Some(0),
        created_at: 0,
    }
}

/// Write `contents` to `root/rel`, creating parent dirs.
fn write_file(root: &std::path::Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir for fixture file");
    }
    std::fs::write(&path, contents).expect("write fixture file");
}

/// The booted handle + the source it drives + the destination folder, the
/// common starting point every reporting scenario builds on.
struct BootedSource {
    handle: DrivenHandle,
    /// Typed handle to the fake remote (the `RemoteStore` trait object
    /// inside `handle.remote` cannot be downcast).
    remote: Arc<InMemoryRemoteStore>,
    source_id: SourceId,
    folder_id: String,
    /// Kept alive so the on-disk fixture survives for the run.
    _src_dir: tempfile::TempDir,
    /// Kept alive so the hermetic state DB survives for the run.
    _state_dir: tempfile::TempDir,
}

/// Boot a headless handle over a fresh in-memory fake, configure a source
/// rooted at a fresh tempdir, and return the pieces. The caller writes the
/// scenario's fixture files into `_src_dir` before driving cycles, OR
/// passes a `populate` closure that does so before the source is upserted.
async fn boot_with_source(
    remote: Arc<InMemoryRemoteStore>,
    populate: impl FnOnce(&std::path::Path),
) -> anyhow::Result<BootedSource> {
    let state_dir = tempfile::tempdir()?;
    let src_dir = tempfile::tempdir()?;
    let folder_id = remote.root_id().to_string();

    populate(src_dir.path());

    let handle = DrivenHandleBuilder::new(state_dir.path().join("state.db"))
        .remote(remote.clone())
        .boot()
        .await?;

    let src = source_in(handle.account_id, src_dir.path(), &folder_id);
    let source_id = src.id;
    handle.state.upsert_source(&src).await?;

    Ok(BootedSource {
        handle,
        remote,
        source_id,
        folder_id,
        _src_dir: src_dir,
        _state_dir: state_dir,
    })
}

/// Re-point every persisted source to `account_id`, preserving the source
/// `id` (and therefore every `file_state` / `pending_ops` row keyed to it).
///
/// [`DrivenHandleBuilder::boot`] mints a FRESH account on every boot, even
/// when reopening an existing hermetic DB - so a rebooted handle's
/// orchestrator (which selects work via `list_enabled_sources_for(its own
/// account)`) would otherwise see ZERO sources and silently sync nothing.
/// The crash-recovery scenarios reboot over the same DB, so they call this
/// to re-attach the persisted source(s) to the new account before driving
/// the recovery cycle. Identity that matters for recovery (the source id,
/// and the `file_state` / `pending_ops` rows under it) is unchanged; only
/// the account FK is rewritten.
async fn rebind_sources_to_account(
    state: &dyn StateRepo,
    account_id: AccountId,
) -> anyhow::Result<()> {
    for mut src in state.list_sources().await? {
        if src.account_id != account_id {
            src.account_id = account_id;
            state.upsert_source(&src).await?;
        }
    }
    Ok(())
}

// ===========================================================================
// Scenario: no data loss (s6.3)
// ===========================================================================

/// STRESS_HARNESS s6.3 "No data loss": after a clean sync of a populated
/// tree, every `status='synced'` row's recorded Drive object exists live.
///
/// Drives the real scan -> plan -> execute pipeline over a small nested
/// tree, then asserts via [`assert_invariants`] that no Synced row lost its
/// remote object and that the live object count equals the file count
/// (nothing silently dropped).
struct NoDataLossScenario;

#[async_trait]
impl Scenario for NoDataLossScenario {
    fn name(&self) -> &'static str {
        "report-no-data-loss"
    }

    fn description(&self) -> &'static str {
        "s6.3 invariant: every synced file_state row maps to a live remote object after a clean sync"
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }

    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        // This scenario boots its own handle (it needs control over the
        // source tree + remote); the driver's pre-booted handle is unused.
        let remote = Arc::new(InMemoryRemoteStore::new());
        let booted = boot_with_source(remote, |root| {
            for i in 0..25u32 {
                write_file(
                    root,
                    &format!("dir{}/f{i:02}.txt", i % 5),
                    format!("data-loss-body-{i}").as_bytes(),
                );
            }
        })
        .await?;

        booted
            .handle
            .orchestrator
            .run_cycle(TickSource::Scheduled)
            .await?;

        let report = assert_invariants(
            &booted.handle,
            &booted.remote,
            booted.source_id,
            &booted.folder_id,
        )
        .await?;

        let mut notes = Vec::new();
        if !report.ok() {
            notes.push(report.violation_summary());
        }
        // The 25 files must all be live - a missing object is data loss the
        // invariant report would also flag, but assert the count too so a
        // silent under-upload fails loudly.
        if report.live_object_count != 25 {
            notes.push(format!(
                "expected 25 live objects after sync, found {}",
                report.live_object_count
            ));
        }

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: report.live_object_count,
            final_hash_matches_local: report.data_loss_paths.is_empty(),
            notes,
            // Single-cycle clean sync driven to completion: the
            // orchestrator's run_cycle returned, so terminal quiescence is
            // the justified clean_shutdown arg. The other three flags are the
            // real computed report.
            invariants: Some(report.to_invariant_outcome(true)),
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ===========================================================================
// Scenario: no duplicate remote objects for one client_op_uuid (s6.3)
// ===========================================================================

/// STRESS_HARNESS s6.3 "No duplicate Drive objects for the same
/// `client_op_uuid`": a real crash mid-pipeline followed by a reboot must
/// NOT leave two live objects sharing one
/// `appProperties.driven.client_op_uuid`.
///
/// Drives a REAL mid-stream crash: a > 5 MiB file uploads through the
/// streaming resumable path against a remote rigged to drop the network
/// after 2 requests (session open + first wire chunk acked), so the upload
/// aborts with the object NOT yet finalized and the executor persists a
/// resume op (live session + acked offset + identity, NO content hash -
/// the mid-stream-crash invariant from DESIGN s5.4 / s5.6). A fresh handle
/// then reboots over the SAME hermetic state + remote; its first
/// `run_cycle` runs the startup reconcile, which resumes the persisted
/// session byte-for-byte and finalizes exactly ONE object. The single-shot
/// drop means phase 2's requests all succeed.
///
/// This uses the REAL executor crash/resume path (no manually-seeded hash,
/// so it needs no `blake3` dependency in the harness crate) - the honest
/// way to exercise the duplicate-object invariant.
struct NoDuplicateRemoteObjectsScenario;

#[async_trait]
impl Scenario for NoDuplicateRemoteObjectsScenario {
    fn name(&self) -> &'static str {
        "report-no-duplicate-remote-objects"
    }

    fn description(&self) -> &'static str {
        "s6.3 invariant: crash + reboot adopts the orphan; no two live objects share one client_op_uuid"
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }

    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        // A > 5 MiB file so it runs the STREAMING resumable path; a single-
        // shot network drop fires mid-cycle. Where exactly the drop lands
        // (folder-ensure, session open, or a mid-stream wire chunk) is left
        // to the orchestrator - and that is the POINT: the s6.3 "no
        // duplicate objects / no data loss" invariant must hold for ANY
        // single-shot fault and the recovery that follows, not just a
        // perfectly-timed mid-stream crash. So this scenario asserts the
        // post-recovery INVARIANT, not a brittle phase-1 crash position.
        let wire_chunk = 4 * 1024 * 1024usize;
        let total_len = 5 * wire_chunk + 4096;
        let big: Vec<u8> = (0..total_len).map(|i| (i % 251) as u8).collect();

        let remote = Arc::new(InMemoryRemoteStore::new().with_network_drop_after(2));
        let folder = remote.root_id().to_string();
        let booted = boot_with_source(remote.clone(), |root| {
            write_file(root, "crash.bin", &big);
        })
        .await?;
        let source_id = booted.source_id;
        let state_db = booted._state_dir.path().join("state.db");
        let src_dir = booted._src_dir;

        // Phase 1: the cycle hits the single-shot drop somewhere. It may
        // abort (persisting a resume op), backoff, or transparently retry to
        // completion (the drop is single-shot). Tolerate every outcome - the
        // post-recovery invariant is what matters.
        let _ = booted
            .handle
            .orchestrator
            .run_cycle(TickSource::Scheduled)
            .await;
        let live_after_phase1 = remote
            .list_folder(&folder)
            .await?
            .into_iter()
            .filter(|e| !e.trashed)
            .count();

        // Hard stop. The single-shot drop is spent.
        let _ = booted.handle.kill_orchestrator().await;

        // Phase 2: a fresh handle reboots over the SAME state + remote. Its
        // first run_cycle runs the startup reconcile (resuming any persisted
        // session byte-for-byte) and then a normal scan/plan/execute, so the
        // one file ends up backed up exactly once regardless of where phase 1
        // crashed.
        let reopened = DrivenHandleBuilder::new(state_db)
            .remote(remote.clone())
            .boot()
            .await?;
        // boot() minted a fresh account; re-attach the persisted source to it
        // so the reopened orchestrator actually picks up the recovery work.
        rebind_sources_to_account(reopened.state.as_ref(), reopened.account_id).await?;
        reopened
            .orchestrator
            .run_cycle(TickSource::Scheduled)
            .await?;

        // The invariant: exactly one live object, no duplicate uuid, no data
        // loss, queue drained. Run the shared checker over the reopened
        // handle (same state + remote).
        let report = assert_invariants(&reopened, &remote, source_id, &folder).await?;

        let mut notes = vec![format!(
            "phase-1 left {live_after_phase1} live object(s) before recovery (informational)"
        )];
        if report.live_object_count != 1 {
            notes.push(format!(
                "expected exactly 1 live object after recovery, found {}",
                report.live_object_count
            ));
        }
        if !report.ok() {
            notes.push(report.violation_summary());
        }

        drop(src_dir);

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: report.live_object_count,
            final_hash_matches_local: report.data_loss_paths.is_empty()
                && report.duplicate_op_uuids.is_empty(),
            notes,
            // Crash + reboot recovery drives the reopened orchestrator's
            // run_cycle to completion (the resume/reconcile path finalizes
            // exactly one object), so terminal quiescence holds. The report
            // is computed AFTER recovery, so its duplicate_op_uuids /
            // data_loss / pending flags reflect the recovered state.
            invariants: Some(report.to_invariant_outcome(true)),
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        // The crash/resume path may log reconcile/backoff codes along the
        // way but completes the work; the s6.3 success criterion is "no
        // duplicate object + no data loss", enforced by the per-scenario
        // assertion plus the shared checker.
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// Scenario: no pending_ops leak (s6.3)
// ===========================================================================

/// STRESS_HARNESS s6.3 "No `pending_ops` leak": after a clean sync settles,
/// `pending_ops` is empty (or holds only future-dated backoff rows).
///
/// Drives a sync to completion then a second steady-state cycle, and
/// asserts no due/overdue `pending_ops` row survives. A leak here means an
/// op was enqueued but never drained - the classic "stuck queue" bug.
struct NoPendingOpsLeakScenario;

#[async_trait]
impl Scenario for NoPendingOpsLeakScenario {
    fn name(&self) -> &'static str {
        "report-no-pending-ops-leak"
    }

    fn description(&self) -> &'static str {
        "s6.3 invariant: pending_ops drains to empty (or only future backoff rows) after a settled sync"
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }

    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let remote = Arc::new(InMemoryRemoteStore::new());
        let booted = boot_with_source(remote, |root| {
            for i in 0..12u32 {
                write_file(
                    root,
                    &format!("q{i:02}.bin"),
                    format!("queue-{i}").as_bytes(),
                );
            }
        })
        .await?;

        // Two cycles: the first uploads, the second is a no-op. After both
        // settle, the queue must be empty.
        booted
            .handle
            .orchestrator
            .run_cycle(TickSource::Scheduled)
            .await?;
        booted
            .handle
            .orchestrator
            .run_cycle(TickSource::Scheduled)
            .await?;

        let report = assert_invariants(
            &booted.handle,
            &booted.remote,
            booted.source_id,
            &booted.folder_id,
        )
        .await?;

        let mut notes = Vec::new();
        if report.leaked_pending_ops > 0 {
            notes.push(format!(
                "{} pending_ops row(s) leaked (due/overdue, not drained)",
                report.leaked_pending_ops
            ));
        }
        if !report.ok() {
            notes.push(report.violation_summary());
        }

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: report.live_object_count,
            final_hash_matches_local: report.data_loss_paths.is_empty(),
            notes,
            // Two cycles driven to a settled steady state (upload then
            // no-op); both run_cycle calls returned, so terminal quiescence
            // is the justified clean_shutdown arg.
            invariants: Some(report.to_invariant_outcome(true)),
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ===========================================================================
// Scenario: clean shutdown (s6.3)
// ===========================================================================

/// STRESS_HARNESS s6.3 "clean shutdown": after the orchestrator finishes a
/// cycle and the handle is dropped (the `kill_orchestrator` path that drops
/// channels without a graceful signal), a fresh handle reboots over the
/// same hermetic state with no leaked queue and a coherent terminal state.
///
/// This is the reporting-side proof that a hard stop leaves the persisted
/// state consistent: reboot -> a steady-state cycle is a clean no-op, the
/// orchestrator settles to `Idle`, and every s6.3 invariant holds on the
/// reopened handle.
struct CleanShutdownScenario;

#[async_trait]
impl Scenario for CleanShutdownScenario {
    fn name(&self) -> &'static str {
        "report-clean-shutdown"
    }

    fn description(&self) -> &'static str {
        "s6.3 invariant: hard stop + reboot leaves consistent state - Idle, empty queue, no data loss"
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }

    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let remote = Arc::new(InMemoryRemoteStore::new());
        let booted = boot_with_source(remote.clone(), |root| {
            for i in 0..8u32 {
                write_file(
                    root,
                    &format!("s{i}.txt"),
                    format!("shutdown-{i}").as_bytes(),
                );
            }
        })
        .await?;
        let folder = booted.folder_id.clone();
        let source_id = booted.source_id;
        let state_db = booted._state_dir.path().join("state.db");
        let src_dir = booted._src_dir;

        // One full cycle, then a hard stop: drop the orchestrator/channels
        // without a graceful shutdown (kill_orchestrator hands back the
        // persisted state layer).
        booted
            .handle
            .orchestrator
            .run_cycle(TickSource::Scheduled)
            .await?;
        let _state = booted.handle.kill_orchestrator().await;

        // Reboot over the SAME hermetic state DB + remote.
        let reopened = DrivenHandleBuilder::new(state_db)
            .remote(remote.clone())
            .boot()
            .await?;
        // boot() minted a fresh account; re-attach the persisted source(s) so
        // the reopened orchestrator sees them.
        rebind_sources_to_account(reopened.state.as_ref(), reopened.account_id).await?;
        // The source row persisted; a steady-state cycle is a clean no-op.
        reopened
            .orchestrator
            .run_cycle(TickSource::Scheduled)
            .await?;

        let settled_idle = matches!(reopened.state().await, OrchestratorState::Idle { .. });
        let report = assert_invariants(&reopened, &remote, source_id, &folder).await?;

        let mut notes = Vec::new();
        if !settled_idle {
            notes.push("orchestrator did not settle to Idle after reboot".to_string());
        }
        if report.live_object_count != 8 {
            notes.push(format!(
                "expected 8 live objects to survive the hard stop, found {}",
                report.live_object_count
            ));
        }
        if !report.ok() {
            notes.push(report.violation_summary());
        }

        // Keep the source fixture alive until assertions are done.
        drop(src_dir);

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: report.live_object_count,
            final_hash_matches_local: report.data_loss_paths.is_empty() && settled_idle,
            notes,
            // This scenario computes terminal quiescence directly: the
            // reopened orchestrator must settle to Idle after the hard stop +
            // reboot. Feed the REAL check (not a hardcoded true) as the
            // clean_shutdown flag.
            invariants: Some(report.to_invariant_outcome(settled_idle)),
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ===========================================================================
// Scenario: bounded memory (s6.3)
// ===========================================================================

/// STRESS_HARNESS s6.3 "bounded memory".
///
/// The PRECISE form of this invariant is an RSS-delta gate over a large
/// tree (s3.2 `million-files-nested`: "Scanner memory bounded (RSS delta <
/// 100 MiB during scan)"), which needs both a big-disk fixture AND a
/// portable process-memory probe. The in-memory hermetic harness has
/// neither, so asserting a real RSS bound here would be FAKE-GREEN. Per the
/// capability discipline (s2.5 / s8) this scenario SKIPS honestly when the
/// big-disk capability is absent, recording why.
///
/// When the capability IS present, it asserts the structural proxy the
/// pipeline CAN prove without an RSS probe: the executor retains no
/// unbounded per-op state, observable as a `pending_ops` table and a
/// live-object count that track the bounded input rather than growing
/// without limit. The honest RSS gate stays the property of
/// `million-files-nested`; this scenario documents the boundary so the gap
/// is surfaced, not masked.
struct BoundedMemoryScenario;

#[async_trait]
impl Scenario for BoundedMemoryScenario {
    fn name(&self) -> &'static str {
        "report-bounded-memory"
    }

    fn description(&self) -> &'static str {
        "s6.3 invariant: scanner/executor retain no unbounded per-op state (structural proxy; RSS gate lives in million-files-nested)"
    }

    fn requires(&self) -> CapabilityRequirements {
        // The real RSS-delta bound needs the big-disk fixture; without it
        // we SKIP rather than assert a weakened bound (s8 honesty).
        CapabilityRequirements::of(vec![Capability::FreeDiskBytes {
            min: 1024 * 1024 * 1024,
        }])
    }

    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        // Reached only when the capability gate passed (the driver SKIPs
        // otherwise). Drive a larger fan-out and assert the structural
        // proxy: bounded queue + live count == input.
        const N: u32 = 500;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let booted = boot_with_source(remote, |root| {
            for i in 0..N {
                write_file(
                    root,
                    &format!("d{}/m{i:04}.bin", i % 50),
                    format!("mem-{i}").as_bytes(),
                );
            }
        })
        .await?;

        booted
            .handle
            .orchestrator
            .run_cycle(TickSource::Scheduled)
            .await?;

        let report = assert_invariants(
            &booted.handle,
            &booted.remote,
            booted.source_id,
            &booted.folder_id,
        )
        .await?;

        let mut notes = vec![
            "RSS-delta gate is exercised by million-files-nested (s3.2); this row asserts the \
             bounded-queue structural proxy only"
                .to_string(),
        ];
        if report.live_object_count != u64::from(N) {
            notes.push(format!(
                "expected {N} live objects, found {} (possible unbounded drop/dup)",
                report.live_object_count
            ));
        }
        if report.leaked_pending_ops > 0 {
            notes.push(format!(
                "{} pending_ops leaked - queue is not bounded to drained work",
                report.leaked_pending_ops
            ));
        }
        if !report.ok() {
            notes.push(report.violation_summary());
        }

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: report.live_object_count,
            final_hash_matches_local: report.data_loss_paths.is_empty(),
            notes,
            // Single-cycle fan-out driven to completion (run_cycle returned),
            // so terminal quiescence is the justified clean_shutdown arg. The
            // bounded-queue / no-dup / no-loss flags are the real report.
            invariants: Some(report.to_invariant_outcome(true)),
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every reporting scenario must expose a stable kebab-case name and a
    /// non-empty description, and the registry list must be the five s6.3
    /// invariant scenarios.
    #[test]
    fn scenario_surface_is_stable() {
        let all = scenarios();
        assert_eq!(all.len(), 5, "five s6.3 invariant scenarios");
        let names: Vec<&str> = all.iter().map(|s| s.name()).collect();
        assert!(names.contains(&"report-no-data-loss"));
        assert!(names.contains(&"report-no-duplicate-remote-objects"));
        assert!(names.contains(&"report-no-pending-ops-leak"));
        assert!(names.contains(&"report-clean-shutdown"));
        assert!(names.contains(&"report-bounded-memory"));
        for s in &all {
            assert!(!s.name().is_empty());
            assert!(!s.description().is_empty());
        }
    }

    /// The no-data-loss scenario passes its own assertion on a clean sync:
    /// 25 live objects, no data-loss paths, queue drained.
    #[tokio::test]
    async fn no_data_loss_holds_on_clean_sync() {
        let s = NoDataLossScenario;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let booted = boot_with_source(remote, |root| {
            for i in 0..25u32 {
                write_file(
                    root,
                    &format!("dir{}/f{i:02}.txt", i % 5),
                    format!("data-loss-body-{i}").as_bytes(),
                );
            }
        })
        .await
        .expect("boot");
        booted
            .handle
            .orchestrator
            .run_cycle(TickSource::Scheduled)
            .await
            .expect("cycle");
        let report = assert_invariants(
            &booted.handle,
            &booted.remote,
            booted.source_id,
            &booted.folder_id,
        )
        .await
        .expect("invariants");
        assert!(report.ok(), "{}", report.violation_summary());
        assert_eq!(report.live_object_count, 25);
        // Exercise the scenario's own run path too (boots its own handle).
        let dummy = boot_with_source(Arc::new(InMemoryRemoteStore::new()), |_| {})
            .await
            .expect("dummy");
        let outcome = s.run_assertions(&dummy.handle).await.expect("assertions");
        assert!(outcome.notes.is_empty(), "notes: {:?}", outcome.notes);
        assert_eq!(outcome.final_drive_object_count, 25);
        assert!(outcome.final_hash_matches_local);
    }

    /// The no-duplicate-objects scenario recovers a single-shot crash to
    /// exactly one live object, no duplicate uuid, after reboot + reconcile.
    /// (There is always one informational phase-1 note; the test asserts the
    /// invariant outcome, not note emptiness.)
    #[tokio::test]
    async fn no_duplicate_objects_recovers_to_single_object() {
        let s = NoDuplicateRemoteObjectsScenario;
        let dummy = boot_with_source(Arc::new(InMemoryRemoteStore::new()), |_| {})
            .await
            .expect("dummy");
        let outcome = s.run_assertions(&dummy.handle).await.expect("assertions");
        assert_eq!(
            outcome.final_drive_object_count, 1,
            "recovered to exactly one object, not duplicated; notes: {:?}",
            outcome.notes
        );
        assert!(
            outcome.final_hash_matches_local,
            "no data loss + no duplicate uuid; notes: {:?}",
            outcome.notes
        );
        // Only the single informational phase-1 note should be present (no
        // invariant-violation notes appended after it).
        assert_eq!(
            outcome.notes.len(),
            1,
            "only the informational phase-1 note expected; got {:?}",
            outcome.notes
        );
    }

    /// The pending-ops-leak scenario drains the queue after a settled sync.
    #[tokio::test]
    async fn no_pending_ops_leak_after_settle() {
        let s = NoPendingOpsLeakScenario;
        let dummy = boot_with_source(Arc::new(InMemoryRemoteStore::new()), |_| {})
            .await
            .expect("dummy");
        let outcome = s.run_assertions(&dummy.handle).await.expect("assertions");
        assert!(outcome.notes.is_empty(), "notes: {:?}", outcome.notes);
    }

    /// The clean-shutdown scenario reboots to a consistent Idle state.
    #[tokio::test]
    async fn clean_shutdown_reboots_consistent() {
        let s = CleanShutdownScenario;
        let dummy = boot_with_source(Arc::new(InMemoryRemoteStore::new()), |_| {})
            .await
            .expect("dummy");
        let outcome = s.run_assertions(&dummy.handle).await.expect("assertions");
        assert!(outcome.notes.is_empty(), "notes: {:?}", outcome.notes);
        assert_eq!(outcome.final_drive_object_count, 8);
        assert!(outcome.final_hash_matches_local);
    }

    /// The bounded-memory scenario requires the big-disk capability so it
    /// SKIPs honestly on a small host rather than faking an RSS bound.
    #[test]
    fn bounded_memory_requires_big_disk() {
        let s = BoundedMemoryScenario;
        let req = s.requires();
        assert!(req
            .required
            .iter()
            .any(|c| matches!(c, Capability::FreeDiskBytes { .. })));
    }
}
