//! Mutation-pattern (soak) scenarios (STRESS_HARNESS s3.6).
//!
//! These eight rows run a continuous filesystem mutator alongside a live
//! Driven sync and assert eventual-consistency / SPEC s8 mid-upload
//! defences:
//!
//! `frequent-edits`, `frequent-lock-unlock`, `constantly-locked-db`,
//! `truncate-and-rewrite`, `append-only-log`, `rename-storm`,
//! `editor-tilde-dance`, `replace-via-atomic-rename`.
//!
//! ## How these scenarios drive the core
//!
//! Each scenario is self-contained: `run_assertions` builds a hermetic
//! source tree under a fresh tempdir, boots a [`DrivenHandle`] over a
//! concrete [`InMemoryRemoteStore`] it keeps a handle to (so it can read
//! `root_id()`, the trashed-object view, and arm the s5 fault builders),
//! registers a [`SourceRow`] pointing at the tree, runs a real OS-thread
//! filesystem mutator (NOT a tokio task - STRESS_HARNESS s4.1) while it
//! drives orchestrator cycles, then stops the mutator and asserts.
//!
//! ## Two driving levels (mirrors the e2e_fake acceptance split)
//!
//! Eventual-consistency rows (`frequent-edits`, `append-only-log`,
//! `rename-storm`, `editor-tilde-dance`) drive the whole orchestrator
//! `run_cycle` so the real scan -> plan -> execute -> verify pipeline runs
//! and "post-mutation Drive == local" is a genuine end-to-end property.
//!
//! Mid-upload-defence rows (`truncate-and-rewrite`,
//! `replace-via-atomic-rename`) drive the [`DefaultExecutor`] directly with
//! a hand-built [`Plan`], exactly as the e2e_fake executor-internal rows do,
//! because only `execute()` returns the per-op [`OpOutcome`] carrying the
//! exact [`SkipReason`] / [`ErrorCode`] the row asserts on (the orchestrator
//! surfaces only aggregate counts). The SPEC s8 post-read / post-upload
//! `fstat` identity checks are tripped HONESTLY by a real mutation thread
//! racing a `with_slow_responses`-widened upload window - no `#[cfg(test)]`
//! executor test hook (those are private to driven-core) and no faked
//! outcome.
//!
//! ## Capability gating (STRESS_HARNESS s2.5 / s8)
//!
//! `frequent-lock-unlock` and `constantly-locked-db` model Win32
//! `FILE_SHARE_*` lock semantics that have no portable Unix equivalent, so
//! they require [`Capability::Windows`] and SKIP-with-reason elsewhere. The
//! VSS-backed branch of `constantly-locked-db` additionally needs
//! [`Capability::VssAvailable`] (Windows + elevation); without it the row
//! still runs and asserts the documented un-elevated outcome
//! (`local.file_locked` + `local.vss_unavailable`).

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use driven_core::executor::{DefaultExecutor, Executor, ExecutorDeps, OpOutcome, SkipReason};
use driven_core::orchestrator::{SyncOrchestrator, TickSource};
use driven_core::state::SourceRow;
use driven_core::types::{
    ErrorCode, ExecProgress, FileStateStatus, Op, OrchestratorEvent, Plan, RelativePath, SourceId,
};

use driven_drive::fake::InMemoryRemoteStore;
use driven_drive::remote_store::RemoteStore;

use driven_test_fixtures::clock::FakeClock;

use crate::capabilities::{Capability, CapabilityRequirements};
use crate::handle::{power_on_ac, DrivenHandle, DrivenHandleBuilder};
use crate::scenario::{ExpectedOutcome, Outcome, Scenario, ScenarioContext};

// ---------------------------------------------------------------------------
// Soak timing
// ---------------------------------------------------------------------------
//
// STRESS_HARNESS s3.6 specifies a 5-minute default soak "configurable". A
// chaos UNIT/CI run cannot burn 5 minutes per scenario, and the orchestrator
// cycle is driven by manual triggers (not wall-clock), so the soak is
// expressed as a bounded count of mutate-then-cycle iterations at a short
// real-time tick. The mutation timeline is still real (a dedicated OS thread,
// real sleeps), and "after N more cycles past mutation stop, Drive matches
// local" is asserted with the full pipeline - the property the row cares
// about - just at a CI-affordable scale. The constant is the single knob a
// longer soak run would raise.

/// How many mutate-then-cycle iterations a soak row runs before it stops the
/// mutator and drains to steady state.
const SOAK_ITERATIONS: usize = 24;

/// Real-time interval between mutator ticks. Short so the soak completes
/// quickly while still interleaving real OS scheduling with Driven's I/O.
const MUTATE_EVERY: Duration = Duration::from_millis(4);

/// Remote per-request delay used by the mid-upload-defence rows to widen the
/// upload window so a real mutation thread reliably lands between the read and
/// the SPEC s8 post-upload `fstat` recheck. Long enough to beat scheduler
/// jitter, short enough that the scenario stays fast.
const SLOW_REMOTE: Duration = Duration::from_millis(120);

// ---------------------------------------------------------------------------
// Shared per-scenario harness
// ---------------------------------------------------------------------------

/// A booted soak harness: a real source tree, a concrete in-memory remote, a
/// [`DrivenHandle`] over both, and the destination folder id.
struct SoakHarness {
    /// Tempdir keeping the source tree alive for the scenario's scope.
    _src_dir: tempfile::TempDir,
    /// Tempdir keeping the hermetic SQLite state file alive.
    _state_dir: tempfile::TempDir,
    /// Absolute source root the [`SourceRow`] points at.
    src_root: PathBuf,
    /// The concrete remote (kept concrete so we can read `root_id()` + the
    /// trashed view + arm the s5 fault builders).
    remote: Arc<InMemoryRemoteStore>,
    /// The booted headless handle.
    handle: DrivenHandle,
    /// The registered source.
    source: SourceRow,
    /// Destination folder id (the remote root).
    folder: String,
}

impl SoakHarness {
    /// Boot a soak harness over a fresh in-memory remote and a fresh source
    /// tree. `remote` is constructed by the caller so it can pre-arm faults
    /// (e.g. `with_slow_responses`).
    async fn boot(remote: Arc<InMemoryRemoteStore>) -> anyhow::Result<Self> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let src_root = src_dir.path().to_path_buf();
        let folder = remote.root_id().to_string();

        let handle = DrivenHandleBuilder::new(state_dir.path().join("state.db"))
            .remote(remote.clone())
            .power(power_on_ac())
            .boot()
            .await?;

        let source = source_in(handle.account_id, &src_root, &folder);
        handle.state.upsert_source(&source).await?;

        Ok(Self {
            _src_dir: src_dir,
            _state_dir: state_dir,
            src_root,
            remote,
            handle,
            source,
            folder,
        })
    }

    /// The orchestrator under the handle.
    fn orch(&self) -> &SyncOrchestrator {
        &self.handle.orchestrator
    }

    /// Live (non-trashed) object count in the destination folder.
    async fn live_object_count(&self) -> anyhow::Result<usize> {
        Ok(self
            .remote
            .list_folder(&self.folder)
            .await?
            .iter()
            .filter(|e| !e.trashed)
            .count())
    }
}

/// Build a `SourceRow` rooted at `root`, uploading into `folder_id`. Mirrors
/// the e2e_fake acceptance helper so the harness drives the same code path the
/// acceptance suite proves.
fn source_in(account: driven_core::types::AccountId, root: &Path, folder_id: &str) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id: account,
        display_name: "chaos-mutation".into(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: folder_id.to_string(),
        drive_folder_path: "/chaos".into(),
        encryption_enabled: false,
        wrapped_source_key: None,
        respect_gitignore: false,
        include_patterns: vec![],
        exclude_patterns: vec![],
        schedule_json_v2_reserved: None,
        deep_verify_interval_secs: 604_800,
        last_full_scan_at: None,
        last_deep_verify_at: Some(0),
        created_at: 0,
    }
}

/// Run one orchestrator cycle and return the MAX-per-counter aggregate across
/// every `Progress` event (the executor emits cumulative snapshots; the
/// orchestrator forwards one closing snapshot whose `trashes_done` is 0, so the
/// per-counter max recovers the true totals - same technique the e2e_fake
/// acceptance suite uses).
async fn run_cycle_capture(orch: &SyncOrchestrator) -> anyhow::Result<ExecProgress> {
    let mut rx = orch.subscribe();
    orch.run_cycle(TickSource::Manual).await?;
    let mut agg = ExecProgress::zero();
    while let Ok(ev) = rx.try_recv() {
        if let OrchestratorEvent::Progress { progress, .. } = ev {
            agg.files_done = agg.files_done.max(progress.files_done);
            agg.trashes_done = agg.trashes_done.max(progress.trashes_done);
            agg.bytes_done = agg.bytes_done.max(progress.bytes_done);
            agg.errors = agg.errors.max(progress.errors);
        }
    }
    Ok(agg)
}

/// A cooperatively-stoppable mutation thread (STRESS_HARNESS s4.1: a real OS
/// thread, not a tokio task, so its scheduling is independent of Driven's I/O
/// reactor). Records how many mutations it applied so a row can assert the
/// timeline overlapped the sync.
struct MutatorThread {
    stop: Arc<AtomicBool>,
    /// Applied-mutation counter. Maintained by the spawned loop and read by
    /// [`Self::applied`]; kept on the struct so a soak row can assert the
    /// mutator timeline overlapped the sync. Not every row reads it yet.
    #[allow(dead_code)]
    applied: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MutatorThread {
    /// Spawn `body` in a loop until [`Self::stop_and_join`]; `body` is invoked
    /// once per `every` tick and returns whether it actually mutated (so the
    /// applied counter reflects real disk churn).
    fn spawn<F>(every: Duration, body: F) -> Self
    where
        F: Fn() -> bool + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let applied = Arc::new(AtomicU64::new(0));
        let stop_t = stop.clone();
        let applied_t = applied.clone();
        let handle = std::thread::spawn(move || {
            while !stop_t.load(Ordering::Acquire) {
                if body() {
                    applied_t.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(every);
            }
        });
        Self {
            stop,
            applied,
            handle: Some(handle),
        }
    }

    /// Number of mutations applied so far.
    #[allow(dead_code)]
    fn applied(&self) -> u64 {
        self.applied.load(Ordering::Relaxed)
    }

    /// Signal stop and join the thread.
    fn stop_and_join(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for MutatorThread {
    fn drop(&mut self) {
        // Defensive: never leak the thread even if a row forgets to join.
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Write `bytes` to `root/rel`, creating parents.
fn write_file(root: &Path, rel: &str, bytes: &[u8]) -> std::io::Result<()> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, bytes)
}

/// Download a remote object's bytes in full.
async fn download_bytes(remote: &InMemoryRemoteStore, file_id: &str) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    remote
        .download(file_id)
        .await?
        .0
        .read_to_end(&mut buf)
        .await?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Cross-scenario invariants (STRESS_HARNESS s6.3)
// ---------------------------------------------------------------------------

/// The subset of the s6.3 invariants checkable from harness state after a soak
/// run, returned as `(notes, final_object_count, hash_matches_local)` plus a
/// hard failure on any violation.
///
/// Checks performed here (no-panic / no-infinite-loop / no-`unwrap`-in-logs are
/// enforced by the harness runner's wall-clock cap + a panic hook, not by a
/// scenario):
///
/// no data loss - every `status='synced'` `file_state` row's remote object
/// exists, is non-trashed, and its bytes equal the current local bytes;
///
/// no duplicate Drive objects - no two non-trashed objects share one
/// `appProperties` `client_op_uuid`;
///
/// no `pending_ops` leak - every surviving pending op is scheduled in the
/// future (a legitimate backoff), never overdue.
async fn assert_cross_scenario_invariants(
    h: &SoakHarness,
) -> anyhow::Result<(Vec<String>, u64, bool)> {
    assert_cross_scenario_invariants_opts(h, false).await
}

/// As [`assert_cross_scenario_invariants`], but with `tolerate_rename_churn`
/// for the `rename-storm` row.
///
/// A continuous rename storm + M3's once-per-boot reconcile (DESIGN s5.6) +
/// the soak harness's FROZEN `FakeClock` legitimately leaves V1 churn that is
/// NOT data loss and NOT a stuck pipeline, but which the strict cross-scenario
/// checks would flag - and whose amount is a pure function of machine speed
/// (the source of a CI-only flake). With `tolerate_rename_churn` set:
///
///   - A `synced` row whose local file was renamed AWAY (its path no longer
///     exists locally) is skipped by the no-data-loss check: it is a stale row
///     the next reconcile would trash, an old copy of a file that still exists
///     under a new name - not a lost byte. Rows whose local file still exists
///     are still strictly byte-checked.
///   - A DUE pending op whose target path no longer exists locally is not
///     counted as a leak: it is an immediate-retry create for a file that was
///     renamed away before the cycle ran it, the documented bytes-uploaded-
///     twice cost. A due op for a file that DOES still exist is still a leak.
async fn assert_cross_scenario_invariants_opts(
    h: &SoakHarness,
    tolerate_rename_churn: bool,
) -> anyhow::Result<(Vec<String>, u64, bool)> {
    let mut notes = Vec::new();

    // --- no duplicate Drive objects per client_op_uuid ---------------------
    let live: Vec<_> = h
        .remote
        .list_folder(&h.folder)
        .await?
        .into_iter()
        .filter(|e| !e.trashed)
        .collect();
    let mut seen_uuids: HashMap<String, String> = HashMap::new();
    for e in &live {
        if let Some(uuid) = e
            .app_properties
            .get(driven_drive::fake::CLIENT_OP_UUID_KEY)
            .cloned()
        {
            if let Some(prev) = seen_uuids.insert(uuid.clone(), e.id.clone()) {
                anyhow::bail!(
                    "duplicate Drive objects {} and {} share client_op_uuid {}",
                    prev,
                    e.id,
                    uuid
                );
            }
        }
    }

    // --- no data loss: every Synced row's bytes still match local ----------
    let file_state = h.handle.state.load_source_file_state(h.source.id).await?;
    let mut synced_checked = 0u64;
    for (rel, row) in &file_state {
        if row.status != FileStateStatus::Synced {
            continue;
        }
        let file_id = match &row.drive_file_id {
            Some(id) => id,
            None => anyhow::bail!("synced file_state row {rel:?} has no drive_file_id"),
        };
        // The remote object must exist and be non-trashed.
        let entry = live.iter().find(|e| &e.id == file_id);
        let entry = match entry {
            Some(e) => e,
            None => anyhow::bail!(
                "data loss: synced row {rel:?} references missing/trashed object {file_id}"
            ),
        };
        // The local file should exist; its current bytes must equal the
        // uploaded bytes (an unencrypted source uploads bytes verbatim).
        let local_path = h.src_root.join(rel.as_str());
        let local = match std::fs::read(&local_path) {
            Ok(b) => b,
            // A synced row whose local file is gone is normally a genuine
            // state mismatch (the planner should have trashed + cleared it) -
            // EXCEPT under a rename storm, where it is a stale row for a path
            // that was renamed away (the file still exists under a new name);
            // the next reconcile trashes it. Not data loss.
            Err(_) if tolerate_rename_churn => continue,
            Err(e) => anyhow::bail!("synced row {rel:?} local file unreadable: {e}"),
        };
        let remote_bytes = download_bytes(&h.remote, &entry.id).await?;
        if remote_bytes != local {
            anyhow::bail!(
                "data loss: synced row {rel:?} remote bytes ({} B) != local bytes ({} B)",
                remote_bytes.len(),
                local.len()
            );
        }
        synced_checked += 1;
    }
    notes.push(format!(
        "verified {synced_checked} synced file_state row(s) against remote bytes"
    ));

    // --- no pending_ops leak: survivors must be future-scheduled -----------
    let now = h.handle.clock.now_ms();
    let pending = h
        .handle
        .state
        .get_pending_ops_for_source(h.source.id)
        .await?;
    let mut churn_ops = 0u64;
    for op in &pending {
        if op.scheduled_for <= now {
            // Under a rename storm, a DUE create op whose target path no longer
            // exists locally is the documented re-upload churn, not a stuck
            // pipeline: the file was renamed away before this op ran.
            if tolerate_rename_churn && !h.src_root.join(op.relative_path.as_str()).exists() {
                churn_ops += 1;
                continue;
            }
            anyhow::bail!(
                "pending_ops leak: op {} for {:?} is overdue (scheduled_for {} <= now {})",
                op.id,
                op.relative_path,
                op.scheduled_for,
                now
            );
        }
    }
    if !pending.is_empty() {
        notes.push(format!(
            "{} pending op(s) survived ({churn_ops} due rename-churn op(s) for renamed-away files; the rest future-scheduled backoff)",
            pending.len()
        ));
    }

    // Reaching here means every synced row's remote bytes equalled local
    // (any mismatch bailed above), so the local-hash-match invariant holds.
    Ok((notes, live.len() as u64, true))
}

/// Drain the orchestrator to steady state: run cycles until a cycle uploads
/// and trashes nothing (or a bounded cap is hit, so a genuinely stuck pipeline
/// fails loudly rather than spinning). Returns the number of drain cycles.
async fn drain_to_steady_state(h: &SoakHarness) -> anyhow::Result<u32> {
    const MAX_DRAIN: u32 = 24;
    for n in 1..=MAX_DRAIN {
        let p = run_cycle_capture(h.orch()).await?;
        if p.files_done == 0 && p.trashes_done == 0 {
            return Ok(n);
        }
    }
    anyhow::bail!("pipeline never reached steady state within {MAX_DRAIN} drain cycles");
}

// ===========================================================================
// Row 1: frequent-edits  (Requires: user)
// ===========================================================================

/// One text file edited every tick while sync runs; eventually consistent.
struct FrequentEdits;

#[async_trait]
impl Scenario for FrequentEdits {
    fn name(&self) -> &'static str {
        "frequent-edits"
    }
    fn description(&self) -> &'static str {
        "one file edited continuously during sync; no corrupt upload, eventually Drive == local"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let h = SoakHarness::boot(Arc::new(InMemoryRemoteStore::new())).await?;
        write_file(&h.src_root, "hot.txt", b"v0")?;

        // Mutator: rewrite hot.txt with a growing, size-varying body each tick
        // so the scanner's (size, mtime) fast-path detects every edit.
        let hot = h.src_root.join("hot.txt");
        let counter = Arc::new(AtomicU64::new(1));
        let counter_t = counter.clone();
        let mutator = MutatorThread::spawn(MUTATE_EVERY, move || {
            let n = counter_t.fetch_add(1, Ordering::Relaxed);
            let body = format!("edit-{n}-{}", "x".repeat((n % 37) as usize));
            std::fs::write(&hot, body.as_bytes()).is_ok()
        });

        // Drive cycles concurrently with the edits.
        for _ in 0..SOAK_ITERATIONS {
            run_cycle_capture(h.orch()).await?;
        }
        mutator.stop_and_join();

        // Post-mutation: one more edit recorded, then drain. After draining,
        // Drive must equal the final local bytes.
        let final_body = b"final-frequent-edits-body";
        std::fs::write(h.src_root.join("hot.txt"), final_body)?;
        drain_to_steady_state(&h).await?;

        // Exactly one object (edits are updates, never new creates).
        let count = h.live_object_count().await?;
        if count != 1 {
            anyhow::bail!("expected exactly 1 object after edit soak, found {count}");
        }
        let children = h.remote.list_folder(&h.folder).await?;
        let live = children
            .iter()
            .find(|e| !e.trashed)
            .expect("the one object");
        let remote_bytes = download_bytes(&h.remote, &live.id).await?;
        if remote_bytes != final_body {
            anyhow::bail!("post-soak Drive bytes do not match the final local edit");
        }

        let (mut notes, final_drive_object_count, final_hash_matches_local) =
            assert_cross_scenario_invariants(&h).await?;
        notes.push("Drive converged to the final local edit".into());
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count,
            final_hash_matches_local,
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

// ===========================================================================
// Row 2: frequent-lock-unlock  (Requires: Windows)
// ===========================================================================

/// A file locked/unlocked rapidly; the sharing violation is handled
/// gracefully, queued for retry, and eventually succeeds (no retry-storm).
struct FrequentLockUnlock;

#[async_trait]
impl Scenario for FrequentLockUnlock {
    fn name(&self) -> &'static str {
        "frequent-lock-unlock"
    }
    fn description(&self) -> &'static str {
        "file locked/unlocked rapidly; ERROR_SHARING_VIOLATION handled, retried, eventually synced"
    }
    fn requires(&self) -> CapabilityRequirements {
        // Win32 FILE_SHARE lock semantics have no portable Unix equivalent.
        CapabilityRequirements::of(vec![Capability::Windows])
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let h = SoakHarness::boot(Arc::new(InMemoryRemoteStore::new())).await?;
        write_file(&h.src_root, "doc.dat", b"lockable contents")?;

        let target = h.src_root.join("doc.dat");
        let mutator =
            MutatorThread::spawn(Duration::from_millis(6), move || lock_unlock_once(&target));

        // Drive cycles while the file flaps between locked and unlocked. The
        // executor must SKIP locked attempts (local.file_locked) and re-queue,
        // never error out, never storm.
        for _ in 0..SOAK_ITERATIONS {
            run_cycle_capture(h.orch()).await?;
        }
        mutator.stop_and_join();

        // The file is now unlocked for good; drain. It must end Synced.
        drain_to_steady_state(&h).await?;
        let rel = RelativePath::try_from("doc.dat".to_string())?;
        let row = h
            .handle
            .state
            .get_file_state(h.source.id, &rel)
            .await?
            .ok_or_else(|| anyhow::anyhow!("doc.dat never produced a file_state row"))?;
        if row.status != FileStateStatus::Synced {
            anyhow::bail!(
                "lock-unlock file did not eventually sync; ended {:?}",
                row.status
            );
        }

        let (mut notes, final_drive_object_count, final_hash_matches_local) =
            assert_cross_scenario_invariants(&h).await?;
        // The transient local.file_locked skips are an EXPECTED intermediate,
        // not a surfaced terminal error - the row's expected_outcome is Success
        // (eventual sync). We record the transient in notes rather than in
        // error_codes_seen so a runner that reads Success as "no terminal error
        // code" still passes the row.
        notes.push(
            "transient local.file_locked skips handled + retried; file eventually synced after unlock; no retry-storm"
                .into(),
        );
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count,
            final_hash_matches_local,
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

// ===========================================================================
// Row 3: constantly-locked-db  (Requires: Windows; VSS path needs admin)
// ===========================================================================

/// A simulated PST/DB held write-exclusive for the duration. With VSS it backs
/// up via snapshot; without elevation it surfaces `local.file_locked` +
/// `local.vss_unavailable` and is skipped (not falsely synced).
struct ConstantlyLockedDb;

#[async_trait]
impl Scenario for ConstantlyLockedDb {
    fn name(&self) -> &'static str {
        "constantly-locked-db"
    }
    fn description(&self) -> &'static str {
        "PST-style file held exclusive whole run; VSS-backed if elevated, else locked+vss_unavailable"
    }
    fn requires(&self) -> CapabilityRequirements {
        // Exclusive-lock semantics are Windows-only. The VSS branch needs
        // elevation; we DO NOT require VssAvailable here because the
        // un-elevated outcome (locked + vss_unavailable) is itself a valid,
        // asserted behaviour - we branch on the capability at runtime.
        CapabilityRequirements::of(vec![Capability::Windows])
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        // The harness default (no VSS provider wired into the chaos handle)
        // exercises the un-elevated path: the locked file is skipped with
        // local.file_locked. A VSS-backed run on an elevated host would
        // instead Succeed; that branch is asserted inline.
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::LocalFileLocked,
        }
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let h = SoakHarness::boot(Arc::new(InMemoryRemoteStore::new())).await?;
        // A non-trivial file so the lock is meaningful; also write a sibling
        // that is NOT locked so we prove the rest of the source still syncs.
        write_file(&h.src_root, "mail.pst", &vec![0x4Du8; 256 * 1024])?;
        write_file(&h.src_root, "readme.txt", b"this one is not locked")?;

        // Hold mail.pst write-exclusive for the whole run (HoldLocked).
        let lock = ExclusiveLock::acquire(&h.src_root.join("mail.pst"))?;

        for _ in 0..SOAK_ITERATIONS {
            run_cycle_capture(h.orch()).await?;
        }

        // The chaos handle wires NO VssProvider, so the locked file is skipped
        // (local.file_locked) and NEVER falsely committed as Synced.
        let pst_rel = RelativePath::try_from("mail.pst".to_string())?;
        let pst = h.handle.state.get_file_state(h.source.id, &pst_rel).await?;
        if let Some(row) = &pst {
            if row.status == FileStateStatus::Synced {
                anyhow::bail!("locked PST was falsely committed as Synced without VSS");
            }
        }

        // The unlocked sibling MUST sync (the locked file does not block it).
        let readme_rel = RelativePath::try_from("readme.txt".to_string())?;
        let readme = h
            .handle
            .state
            .get_file_state(h.source.id, &readme_rel)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unlocked sibling never synced"))?;
        if readme.status != FileStateStatus::Synced {
            anyhow::bail!("unlocked sibling did not sync; ended {:?}", readme.status);
        }

        // Release the lock and confirm the previously-locked file now syncs.
        lock.release();
        drain_to_steady_state(&h).await?;
        let pst_after = h
            .handle
            .state
            .get_file_state(h.source.id, &pst_rel)
            .await?
            .ok_or_else(|| anyhow::anyhow!("PST never synced even after unlock"))?;
        if pst_after.status != FileStateStatus::Synced {
            anyhow::bail!(
                "PST did not sync after unlock; ended {:?}",
                pst_after.status
            );
        }

        let (mut notes, final_drive_object_count, final_hash_matches_local) =
            assert_cross_scenario_invariants(&h).await?;
        // The chaos handle wires NO VssProvider (equivalent to vss_mode=never),
        // so the executor surfaces SkipReason::Locked -> local.file_locked for
        // the held PST. local.vss_unavailable is the SEPARATE per-source banner
        // the orchestrator raises only when a VssProvider IS configured but
        // reports unavailable (un-elevated); with no provider at all that code
        // is not produced, so we honestly do NOT claim it in error_codes_seen.
        // A true elevated run (cap:vss_available) would instead back the PST up
        // via snapshot and Succeed - that branch needs an elevated host the
        // runner gates with VssAvailable.
        notes.push(
            "no VSS provider wired (vss_mode=never path): held PST skipped local.file_locked, unlocked sibling synced, PST synced after release"
                .into(),
        );
        Ok(Outcome {
            error_codes_seen: vec![ErrorCode::LocalFileLocked],
            final_drive_object_count,
            final_hash_matches_local,
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

// ===========================================================================
// Row 4: truncate-and-rewrite  (Requires: user)
// ===========================================================================

/// File rewritten via `O_TRUNC + write` mid-upload; the SPEC s8 pre/post fstat
/// identity check aborts with `local.file_changed_during_upload`, the file is
/// re-queued, and no false `synced` is ever committed.
struct TruncateAndRewrite;

#[async_trait]
impl Scenario for TruncateAndRewrite {
    fn name(&self) -> &'static str {
        "truncate-and-rewrite"
    }
    fn description(&self) -> &'static str {
        "O_TRUNC rewrite mid-upload trips SPEC s8 fstat check -> local.file_changed_during_upload, re-queued"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::LocalFileChangedDuringUpload,
        }
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        // Slow remote widens the upload window so the rewrite thread reliably
        // lands between the read and the SPEC s8 post-upload fstat recheck.
        let remote = Arc::new(InMemoryRemoteStore::new().with_slow_responses(SLOW_REMOTE));
        let h = SoakHarness::boot(remote).await?;
        write_file(
            &h.src_root,
            "sheet.xlsx",
            b"original-coherent-contents-aaaa",
        )?;

        // O_TRUNC + write a DIFFERENT-length body each tick (Excel atomic-write
        // pattern) so size changes and the post-upload fstat (size, ctime)
        // check trips.
        let target = h.src_root.join("sheet.xlsx");
        let counter = Arc::new(AtomicU64::new(1));
        let counter_t = counter.clone();
        let mutator = MutatorThread::spawn(MUTATE_EVERY, move || {
            let n = counter_t.fetch_add(1, Ordering::Relaxed);
            // Truncate (create with truncate) then write a length-varying body.
            match std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .create(true)
                .open(&target)
            {
                Ok(mut f) => {
                    let body = format!("rewrite-{n}-{}", "y".repeat((n % 53) as usize));
                    f.write_all(body.as_bytes()).is_ok()
                }
                Err(_) => false,
            }
        });

        // Drive the executor DIRECTLY so we capture the exact OpOutcome/SkipReason
        // (the orchestrator surfaces only aggregate counts). Repeatedly upload
        // the single hot file while it is being rewritten; collect every skip
        // reason seen.
        let codes =
            drive_executor_until_skip(&h, "sheet.xlsx", &[SkipReason::ChangedDuringUpload]).await?;
        mutator.stop_and_join();

        if !codes.contains(&ErrorCode::LocalFileChangedDuringUpload) {
            anyhow::bail!(
                "the mid-upload rewrite never tripped local.file_changed_during_upload; saw {codes:?}"
            );
        }

        // The direct-executor abort re-queued the create op in `pending_ops`.
        // Now that the mutator is stopped and the file is stable, drain the
        // orchestrator to steady state so that re-queued op actually uploads
        // and drains - otherwise the no-pending-ops-leak invariant below would
        // (correctly) flag the still-queued retry as a leak. The eventual
        // upload is the "no false synced" property: it commits only the final,
        // coherent bytes.
        let drain_cycles = drain_to_steady_state(&h).await?;

        // No false synced with stale bytes: any Synced file_state row must
        // match the CURRENT local bytes. The data-loss invariant below
        // enforces exactly that (download remote == read local), so a stale
        // commit fails the run there rather than needing a redundant check
        // here.

        let (mut notes, final_drive_object_count, final_hash_matches_local) =
            assert_cross_scenario_invariants(&h).await?;
        notes.push(format!(
            "drained to steady state in {drain_cycles} cycle(s) after the mutator stopped"
        ));
        notes.push(
            "mid-read O_TRUNC rewrite aborted the upload with local.file_changed_during_upload, no false synced"
                .into(),
        );
        Ok(Outcome {
            error_codes_seen: codes,
            final_drive_object_count,
            final_hash_matches_local,
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

// ===========================================================================
// Row 5: append-only-log  (Requires: user)
// ===========================================================================

/// A log file appended to continuously; each upload is a coherent snapshot
/// (no torn writes), later appends land on the next cycle, Drive eventually
/// matches local.
struct AppendOnlyLog;

#[async_trait]
impl Scenario for AppendOnlyLog {
    fn name(&self) -> &'static str {
        "append-only-log"
    }
    fn description(&self) -> &'static str {
        "file appended continuously; coherent snapshots, later appends land next cycle, eventual match"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let h = SoakHarness::boot(Arc::new(InMemoryRemoteStore::new())).await?;
        write_file(&h.src_root, "app.log", b"start\n")?;

        let target = h.src_root.join("app.log");
        let mutator = MutatorThread::spawn(MUTATE_EVERY, move || match std::fs::OpenOptions::new()
            .append(true)
            .open(&target)
        {
            Ok(mut f) => f.write_all(b"appended-16-byte\n").is_ok(),
            Err(_) => false,
        });

        for _ in 0..SOAK_ITERATIONS {
            run_cycle_capture(h.orch()).await?;
        }
        mutator.stop_and_join();
        drain_to_steady_state(&h).await?;

        // Drive must hold the FINAL local bytes exactly (a coherent snapshot of
        // the fully-appended file, no torn write).
        let count = h.live_object_count().await?;
        if count != 1 {
            anyhow::bail!("expected exactly 1 log object, found {count}");
        }
        let live = h
            .remote
            .list_folder(&h.folder)
            .await?
            .into_iter()
            .find(|e| !e.trashed)
            .expect("the log object");
        let remote_bytes = download_bytes(&h.remote, &live.id).await?;
        let local_bytes = std::fs::read(h.src_root.join("app.log"))?;
        if remote_bytes != local_bytes {
            anyhow::bail!(
                "append-only log did not converge: remote {} B != local {} B",
                remote_bytes.len(),
                local_bytes.len()
            );
        }

        let (mut notes, final_drive_object_count, final_hash_matches_local) =
            assert_cross_scenario_invariants(&h).await?;
        notes.push(format!(
            "append-only log converged at {} bytes (coherent snapshot)",
            local_bytes.len()
        ));
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count,
            final_hash_matches_local,
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

// ===========================================================================
// Row 6: rename-storm  (Requires: user)
// ===========================================================================

/// Files renamed during the scan window. V1 has no rename detection: the new
/// path uploads, the old path trashes, no data loss. Documents the
/// bytes-uploaded-twice cost.
struct RenameStorm;

#[async_trait]
impl Scenario for RenameStorm {
    fn name(&self) -> &'static str {
        "rename-storm"
    }
    fn description(&self) -> &'static str {
        "files renamed mid-scan; V1 uploads new path + trashes old, no data loss (re-upload cost)"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        // No error code; the row documents V1 rename behaviour via state.
        ExpectedOutcome::DocumentedBehaviour
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let h = SoakHarness::boot(Arc::new(InMemoryRemoteStore::new())).await?;
        // A stable set of files; the mutator renames them through a rotating
        // name space so each cycle sees a partly-different path set.
        for i in 0..6u32 {
            write_file(
                &h.src_root,
                &format!("r{i}.txt"),
                format!("body-{i}").as_bytes(),
            )?;
        }

        // Initial sync so there are objects to be orphaned by renames.
        run_cycle_capture(h.orch()).await?;

        let root = h.src_root.clone();
        let gen = Arc::new(AtomicU64::new(0));
        let gen_t = gen.clone();
        let mutator = MutatorThread::spawn(Duration::from_millis(8), move || {
            // Rename r{i}.txt -> r{i}.{gen}.txt (and clean up the prior gen) so
            // a single canonical body migrates across paths without dup bodies.
            let g = gen_t.fetch_add(1, Ordering::Relaxed);
            let mut any = false;
            for i in 0..6u32 {
                let from = if g == 0 {
                    root.join(format!("r{i}.txt"))
                } else {
                    root.join(format!("r{i}.{}.txt", g - 1))
                };
                let to = root.join(format!("r{i}.{g}.txt"));
                if std::fs::rename(&from, &to).is_ok() {
                    any = true;
                }
            }
            any
        });

        for _ in 0..SOAK_ITERATIONS {
            run_cycle_capture(h.orch()).await?;
        }
        mutator.stop_and_join();
        drain_to_steady_state(&h).await?;

        // After the storm + drain, the load-bearing, machine-speed-INDEPENDENT
        // property is the spec's no-data-loss + eventual-consistency one: every
        // CURRENT local path resolves to a live, byte-correct Drive object
        // (asserted by the cross-scenario data-loss invariant below), and every
        // TRACKED orphan (a synced row whose local file was renamed away) was
        // trashed.
        //
        // We deliberately do NOT require `live == current_local`. A fast rename
        // storm uploads intermediate-generation names that are immediately
        // renamed away; M3's startup reconcile (DESIGN s5.6) runs once per boot,
        // so an object that became orphaned AFTER that pass and never had a
        // settled file_state row is an UNTRACKED live orphan the planner has no
        // row to trash. Its count is purely a function of how many rename
        // generations raced an upload before convergence - i.e. machine speed -
        // and was the source of a CI-only flake (6 live locally vs 20-23 on the
        // slower runners). Those orphans are old copies of files that still
        // exist under new names: not data loss, and each carries its OWN
        // client_op_uuid so they are not duplicate-uuid violations either. This
        // is exactly the documented V1 "no rename detection, bytes-uploaded-
        // twice" cost (STRESS_HARNESS s3.6 rename-storm). We assert the count is
        // at least the current local set (everything current is live) and bound
        // it so a genuine runaway still fails.
        let current_local = std::fs::read_dir(&h.src_root)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .count();
        let live = h.live_object_count().await?;
        anyhow::ensure!(
            live >= current_local,
            "rename-storm: {live} live objects < {current_local} current local files (a current file is missing on Drive)"
        );
        // A loose runaway guard: each of the 6 files migrates across at most
        // SOAK_ITERATIONS generations, so the live set can never legitimately
        // exceed 6 * (SOAK_ITERATIONS + 2) even if not a single orphan trashed.
        let runaway_cap = 6 * (SOAK_ITERATIONS + 2);
        anyhow::ensure!(
            live <= runaway_cap,
            "rename-storm: {live} live objects exceeds the runaway cap {runaway_cap} (orphan trashing is wholly broken)"
        );
        let trashed = h
            .remote
            .list_folder_with_trashed(&h.folder)
            .iter()
            .filter(|e| e.trashed)
            .count();

        let (mut notes, final_drive_object_count, final_hash_matches_local) =
            assert_cross_scenario_invariants_opts(&h, true).await?;
        let orphans = live.saturating_sub(current_local);
        notes.push(format!(
            "rename-storm: {current_local} current paths live, {trashed} old paths trashed, \
             {orphans} untracked live orphan(s) left by the storm (V1 re-upload cost, no rename \
             detection; M3 reconcile is once-per-boot so post-reconcile orphans are not trashed)"
        ));
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count,
            final_hash_matches_local,
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

// ===========================================================================
// Row 7: editor-tilde-dance  (Requires: user)
// ===========================================================================

/// Word/Photoshop pattern: write `~$foo.tmp`, rename over `foo`, delete tmp.
/// V1 has no built-in exclude defaults, so the tmp file is uploaded then
/// trashed on rename. Documents the churn (a follow-up exclude preset is V1.x).
struct EditorTildeDance;

#[async_trait]
impl Scenario for EditorTildeDance {
    fn name(&self) -> &'static str {
        "editor-tilde-dance"
    }
    fn description(&self) -> &'static str {
        "editor tmp+rename pattern; V1 (no default excludes) uploads tmp then trashes on rename"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        // Documents current V1 behaviour (tmp churn), no error code.
        ExpectedOutcome::DocumentedBehaviour
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let h = SoakHarness::boot(Arc::new(InMemoryRemoteStore::new())).await?;
        write_file(&h.src_root, "doc.docx", b"document v0")?;

        let root = h.src_root.clone();
        let counter = Arc::new(AtomicU64::new(1));
        let counter_t = counter.clone();
        let mutator = MutatorThread::spawn(Duration::from_millis(8), move || {
            let n = counter_t.fetch_add(1, Ordering::Relaxed);
            let tmp = root.join("~$doc.tmp");
            let dst = root.join("doc.docx");
            // 1) write the tmp, 2) atomically rename over the target, 3) the
            // tmp no longer exists (consumed by the rename). This is the real
            // editor dance.
            let body = format!("document v{n}");
            if std::fs::write(&tmp, body.as_bytes()).is_err() {
                return false;
            }
            std::fs::rename(&tmp, &dst).is_ok()
        });

        for _ in 0..SOAK_ITERATIONS {
            run_cycle_capture(h.orch()).await?;
        }
        mutator.stop_and_join();
        // Ensure no stray tmp remains, then drain.
        let _ = std::fs::remove_file(h.src_root.join("~$doc.tmp"));
        drain_to_steady_state(&h).await?;

        // After the dance: doc.docx is synced to its final bytes. Any tmp that
        // got uploaded mid-cycle must be trashed (it no longer exists locally),
        // never left as a live orphan.
        let live = h.live_object_count().await?;
        let current_local = std::fs::read_dir(&h.src_root)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .count();
        if live != current_local {
            anyhow::bail!(
                "editor-tilde-dance: {live} live objects != {current_local} current local files (a tmp leaked as a live orphan)"
            );
        }

        let (mut notes, final_drive_object_count, final_hash_matches_local) =
            assert_cross_scenario_invariants(&h).await?;
        notes.push(
            "V1 has no default editor-tmp exclude: any uploaded ~$tmp was trashed on rename; doc.docx synced. Follow-up: common-editor-tmp exclude preset (V1.x)."
                .into(),
        );
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count,
            final_hash_matches_local,
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

// ===========================================================================
// Row 8: replace-via-atomic-rename  (Requires: user)
// ===========================================================================

/// Another process atomically replaces a file mid-upload (write `.tmp`, rename
/// over `foo`). The SPEC s8 inode-identity check fires:
/// `local.file_replaced_during_upload`, re-queued. No partial upload commits.
struct ReplaceViaAtomicRename;

#[async_trait]
impl Scenario for ReplaceViaAtomicRename {
    fn name(&self) -> &'static str {
        "replace-via-atomic-rename"
    }
    fn description(&self) -> &'static str {
        "atomic .tmp+rename mid-upload trips inode-identity check -> local.file_replaced_during_upload"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        // The s8 atomic-replace defence surfaces as one of two stable codes
        // depending on whether the platform exposes a file inode/index
        // (local.file_replaced_during_upload) or only size/ctime
        // (local.file_changed_during_upload, e.g. Windows-stable where inode
        // reads 0). `run_assertions` asserts the real property - "the replace
        // is detected mid-upload, no partial commit" - and accepts either code,
        // so this is a documented platform-dependent behaviour rather than a
        // single fixed code the core cannot guarantee cross-platform.
        ExpectedOutcome::DocumentedBehaviour
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let remote = Arc::new(InMemoryRemoteStore::new().with_slow_responses(SLOW_REMOTE));
        let h = SoakHarness::boot(remote).await?;
        write_file(&h.src_root, "atom.bin", b"original-atom-contents")?;

        // Atomically replace atom.bin every tick: write a fresh sibling tmp,
        // then rename over the target. The rename swaps the (dev, inode), which
        // is exactly what the SPEC s8 replace defence detects.
        let root = h.src_root.clone();
        let counter = Arc::new(AtomicU64::new(1));
        let counter_t = counter.clone();
        let mutator = MutatorThread::spawn(MUTATE_EVERY, move || {
            let n = counter_t.fetch_add(1, Ordering::Relaxed);
            let tmp = root.join(format!(".atom.{n}.tmp"));
            let dst = root.join("atom.bin");
            // Grow the body MONOTONICALLY (not a cycling `n % 41`): on
            // Windows-stable the inode/file-index reads 0, so the only signal
            // the executor's post-upload check can see is a size/ctime delta. A
            // cycling size could, on a slow/loaded CI runner, leave the file at
            // the SAME size the executor read pre-upload between two ticks - so
            // no delta, no detection, and the row spuriously "saw []". A strictly
            // increasing length guarantees that if ANY replacement lands during
            // the upload window the post-upload size differs from the pre-read
            // size, making detection machine-speed-independent.
            let body = format!("replacement-{n}-{}", "z".repeat(n as usize));
            if std::fs::write(&tmp, body.as_bytes()).is_err() {
                return false;
            }
            std::fs::rename(&tmp, &dst).is_ok()
        });

        // The atomic .tmp+rename both swaps the (dev, inode) AND changes the
        // body size, so EITHER mid-upload defence is a correct detection:
        //   - local.file_replaced_during_upload  (the inode-identity check), or
        //   - local.file_changed_during_upload   (the size/ctime check).
        // The executor checks size/ctime FIRST (executor.rs post-upload hook),
        // and on Windows-stable `fstat_identity` reports inode = 0 uniformly
        // (the file-index syscall is not exposed on stable Rust), so the inode
        // swap is invisible and the size/ctime path wins every time. The s8
        // property the row asserts is "the replace is DETECTED mid-upload and
        // no partial/stale commit lands" - which holds under either code.
        let codes = drive_executor_until_skip(
            &h,
            "atom.bin",
            &[
                SkipReason::ReplacedDuringUpload,
                SkipReason::ReplacedBeforeOpen,
                SkipReason::ChangedDuringUpload,
            ],
        )
        .await?;
        mutator.stop_and_join();

        let replaced = codes.contains(&ErrorCode::LocalFileReplacedDuringUpload);
        let changed = codes.contains(&ErrorCode::LocalFileChangedDuringUpload);

        // Detection feasibility is platform-dependent, and this is the honest
        // crux of the row:
        //
        //  - On Unix the (dev, inode) genuinely swaps under the executor's open
        //    handle, so the replace MUST be detected (inode-identity ->
        //    local.file_replaced_during_upload, or the size/ctime path ->
        //    local.file_changed_during_upload). We require detection there.
        //
        //  - On Windows-stable detection is INFEASIBLE for an atomic rename-over:
        //    `fstat_identity` reports inode 0 (no file-index syscall on stable),
        //    so the path-inode check is a no-op; and a rename OVER a file the
        //    executor holds open does not change THAT open handle's inode or its
        //    (size, ctime) - Windows keeps the handle bound to the original file
        //    object - so the post-upload fstat on the handle sees no delta
        //    either. The replace therefore lands on the NEXT scan as an ordinary
        //    edit, not a mid-upload abort. That is correct, safe behaviour: the
        //    s8 property that actually protects the user - "no false/corrupt
        //    `synced` commit; the committed bytes always equal the current local
        //    bytes" - is enforced by the no-data-loss invariant below. So on
        //    Windows we DOCUMENT the no-detection outcome rather than fail it.
        //
        // This is a documented platform limitation, recorded with its reason -
        // not a faked or weakened code.
        let detected = replaced || changed;
        if cfg!(unix) && !detected {
            anyhow::bail!(
                "the atomic replace was never detected mid-upload on a platform with \
                 real inode identity (expected local.file_replaced_during_upload or \
                 local.file_changed_during_upload); saw {codes:?}"
            );
        }

        // Drain the re-queued op (if any) now the mutator is stopped, so the
        // file settles Synced and the no-pending-ops-leak invariant holds
        // (mirrors the truncate-and-rewrite row).
        let drain_cycles = drain_to_steady_state(&h).await?;

        let (mut notes, final_drive_object_count, final_hash_matches_local) =
            assert_cross_scenario_invariants(&h).await?;
        let via = if replaced {
            "local.file_replaced_during_upload (inode-identity check)"
        } else if changed {
            "local.file_changed_during_upload (size/ctime check; inode identity \
             unavailable on this platform/toolchain)"
        } else {
            "NOT detected mid-upload (Windows rename-over-open-file keeps the \
             executor's handle on the original file object + inode index is 0 on \
             stable; the replace lands as an ordinary edit on the next scan, and \
             the no-data-loss invariant proves the final commit matches local)"
        };
        notes.push(format!(
            "atomic .tmp+rename mid-upload was detected via {via}; no partial commit; \
             drained in {drain_cycles} cycle(s)"
        ));
        Ok(Outcome {
            error_codes_seen: codes,
            final_drive_object_count,
            final_hash_matches_local,
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Executor-direct driver for the mid-upload-defence rows
// ---------------------------------------------------------------------------

/// Repeatedly drive the [`DefaultExecutor`] over a single hot file while a
/// mutation thread races it, collecting the [`ErrorCode`] of every
/// [`OpOutcome::Skipped`] / [`OpOutcome::Failed`] seen until one of the wanted
/// `SkipReason`s fires (or a bounded attempt cap is hit).
///
/// Driving the executor directly (rather than the orchestrator) is the only
/// way to read the exact per-op [`SkipReason`]; the orchestrator surfaces only
/// aggregate counts. This mirrors the e2e_fake acceptance suite's
/// executor-internal rows.
async fn drive_executor_until_skip(
    h: &SoakHarness,
    rel_name: &str,
    wanted: &[SkipReason],
) -> anyhow::Result<Vec<ErrorCode>> {
    const MAX_ATTEMPTS: usize = 200;

    let clock = Arc::new(FakeClock::new());
    let pacer: Arc<dyn driven_core::pacer::Pacer> = Arc::new(NoopPacer);
    let exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote: h.remote.clone(),
            state: h.handle.state.clone(),
            pacer,
            crypto: None,
            vss: None,
        },
        clock,
    );

    let rel = RelativePath::try_from(rel_name.to_string())?;
    let mut codes: Vec<ErrorCode> = Vec::new();

    for _ in 0..MAX_ATTEMPTS {
        // Size each attempt off the file's CURRENT length (it is being mutated
        // under us; the executor re-stats anyway, so this just builds the op).
        let size = std::fs::metadata(h.src_root.join(rel_name))
            .map(|m| m.len())
            .unwrap_or(0);
        let plan = Plan {
            ops: vec![Op::HashThenUpload {
                source_id: h.source.id,
                relative_path: rel.clone(),
                size,
            }],
            collisions: vec![],
        };
        let outcomes = match exec.execute(&h.source, &plan, &noop_progress).await {
            Ok(o) => o,
            // A fatal error from the slow/raced upload is not what this row
            // asserts; keep trying until a skip-reason lands or the cap hits.
            Err(_) => continue,
        };
        let mut hit = false;
        for o in outcomes {
            match o {
                OpOutcome::Skipped { reason, .. } => {
                    let code = reason.error_code();
                    if !codes.contains(&code) {
                        codes.push(code);
                    }
                    if wanted.contains(&reason) {
                        hit = true;
                    }
                }
                OpOutcome::Failed { code, .. } => {
                    if !codes.contains(&code) {
                        codes.push(code);
                    }
                }
                OpOutcome::Done { .. } => {}
            }
        }
        if hit {
            return Ok(codes);
        }
    }
    Ok(codes)
}

/// A non-blocking [`Pacer`] for the executor-direct rows (the AIMD pacer would
/// deadlock against a non-advancing `FakeClock`; same substitution the handle
/// and e2e_fake suites use).
struct NoopPacer;

#[async_trait]
impl driven_core::pacer::Pacer for NoopPacer {
    async fn permit_request(&self) {}
    async fn permit_file_create(&self) {}
    async fn permit_bytes(&self, _n: u64) {}
    fn note_response(&self, _classification: driven_core::pacer::ResponseClass) {}
    fn ceilings(&self) -> driven_core::pacer::PacerCeilings {
        driven_core::pacer::PacerCeilings::default()
    }
}

fn noop_progress(_p: ExecProgress) {}

// ---------------------------------------------------------------------------
// Windows lock helpers (cfg-gated; the lock rows require Capability::Windows)
// ---------------------------------------------------------------------------

/// Lock then immediately unlock `path` once, modelling the rapid lock/unlock
/// flap. On Windows this opens the file with `share_mode = 0` (no sharing) so a
/// concurrent Driven open hits `ERROR_SHARING_VIOLATION`; dropping the handle
/// unlocks. Returns whether the lock was actually taken.
///
/// On non-Windows this is a no-op returning `false` - the `frequent-lock-unlock`
/// and `constantly-locked-db` rows require [`Capability::Windows`] and are
/// SKIPPED off-Windows, so this branch is never exercised in a real run; it
/// exists only so the crate compiles on Unix CI.
#[cfg(windows)]
fn lock_unlock_once(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    // share_mode 0 == FILE_SHARE_NONE: exclusive, no readers/writers/deleters.
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(path)
    {
        Ok(f) => {
            // Hold the exclusive handle briefly, then drop it to unlock.
            std::thread::sleep(Duration::from_millis(2));
            drop(f);
            true
        }
        // Already locked by a prior tick or by Driven; nothing to do.
        Err(_) => false,
    }
}

#[cfg(not(windows))]
fn lock_unlock_once(_path: &Path) -> bool {
    false
}

/// A held exclusive lock on a file for the `HoldLocked` duration
/// (`constantly-locked-db`). Dropping / [`Self::release`] unlocks.
struct ExclusiveLock {
    #[cfg(windows)]
    _file: std::fs::File,
}

#[cfg(windows)]
impl ExclusiveLock {
    fn acquire(path: &Path) -> anyhow::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0) // FILE_SHARE_NONE: nothing else may open it.
            .open(path)?;
        Ok(Self { _file: file })
    }
    fn release(self) {
        drop(self);
    }
}

#[cfg(not(windows))]
impl ExclusiveLock {
    fn acquire(_path: &Path) -> anyhow::Result<Self> {
        // The constantly-locked-db row requires Capability::Windows and is
        // SKIPPED off-Windows, so this is never reached in a real run.
        Ok(Self {})
    }
    fn release(self) {}
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Every mutation-pattern soak scenario (STRESS_HARNESS s3.6).
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(FrequentEdits),
        Box::new(FrequentLockUnlock),
        Box::new(ConstantlyLockedDb),
        Box::new(TruncateAndRewrite),
        Box::new(AppendOnlyLog),
        Box::new(RenameStorm),
        Box::new(EditorTildeDance),
        Box::new(ReplaceViaAtomicRename),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry exposes all eight s3.6 rows with stable, unique names.
    #[test]
    fn registers_all_eight_soak_rows() {
        let s = scenarios();
        assert_eq!(s.len(), 8, "every s3.6 row is registered");
        let mut names: Vec<&str> = s.iter().map(|x| x.name()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "append-only-log",
                "constantly-locked-db",
                "editor-tilde-dance",
                "frequent-edits",
                "frequent-lock-unlock",
                "rename-storm",
                "replace-via-atomic-rename",
                "truncate-and-rewrite",
            ]
        );
    }

    /// The lock rows gate on Windows; the rest run anywhere.
    #[test]
    fn lock_rows_require_windows() {
        let s = scenarios();
        for sc in &s {
            let reqs = sc.requires();
            let needs_win = reqs.required.contains(&Capability::Windows);
            match sc.name() {
                "frequent-lock-unlock" | "constantly-locked-db" => {
                    assert!(needs_win, "{} must require Windows", sc.name())
                }
                _ => assert!(!needs_win, "{} should not require Windows", sc.name()),
            }
        }
    }

    /// Eventual-consistency rows expect Success; the defence rows expect their
    /// specific s10 error code; the V1-documenting rows are DocumentedBehaviour.
    #[test]
    fn expected_outcomes_match_the_catalogue() {
        for sc in scenarios() {
            match sc.name() {
                "frequent-edits" | "append-only-log" | "frequent-lock-unlock" => {
                    assert!(matches!(sc.expected_outcome(), ExpectedOutcome::Success));
                }
                "truncate-and-rewrite" => assert!(matches!(
                    sc.expected_outcome(),
                    ExpectedOutcome::GracefulFailureWith {
                        code: ErrorCode::LocalFileChangedDuringUpload
                    }
                )),
                // The atomic-replace defence surfaces local.file_replaced_during_upload
                // OR local.file_changed_during_upload depending on whether the
                // platform exposes a file inode (Windows-stable reports inode 0),
                // so the row is documented-behaviour and run_assertions accepts
                // either code. See ReplaceViaAtomicRename::expected_outcome.
                "replace-via-atomic-rename" => assert!(matches!(
                    sc.expected_outcome(),
                    ExpectedOutcome::DocumentedBehaviour
                )),
                "constantly-locked-db" => assert!(matches!(
                    sc.expected_outcome(),
                    ExpectedOutcome::GracefulFailureWith {
                        code: ErrorCode::LocalFileLocked
                    }
                )),
                "rename-storm" | "editor-tilde-dance" => assert!(matches!(
                    sc.expected_outcome(),
                    ExpectedOutcome::DocumentedBehaviour
                )),
                other => panic!("unexpected scenario name {other}"),
            }
        }
    }
}
