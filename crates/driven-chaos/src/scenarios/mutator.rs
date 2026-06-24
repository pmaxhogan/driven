//! The continuous-mutation harness as registered scenarios (STRESS_HARNESS s4).
//!
//! STRESS_HARNESS s2.3 sketches `fuzz` / `mutator` as harness DRIVERS that
//! compose scenarios programmatically "rather than as `Scenario` impls". The
//! M3.7 implement task overrides that: it asks for EVERY s4 item to be a
//! [`Scenario`] impl registered in the registry, in this file. We honour both
//! intents - the s4 mechanism is exposed BOTH as registered [`Scenario`]s
//! (so `scenario run`/`run-all`/`fixture` see them) AND as the
//! [`run_fuzz`] / [`run_fs_mutator`] / [`run_drive_mutator`] public driver
//! functions the `fuzz` / `mutator` CLI subcommands (STRESS_HARNESS s2.2)
//! need, since a bare registered scenario cannot carry `--seed` / `--duration`
//! / `--flavour`.
//!
//! How these differ from the s3.6 / s3.7 catalogue rows (in
//! [`crate::scenarios::mutation`] / [`crate::scenarios::drive_side`]): those
//! rows assert a SINGLE mutation pattern's point behaviour once. The s4
//! scenarios here run the CONTINUOUS soak / fault loop - the s4.1 filesystem
//! mutator races a dedicated OS thread against repeated sync cycles, the s4.2
//! Drive-side mutator drives the fault-injection surface mid-sync, and s4.3
//! fuzz applies a seeded weighted MIX - and then assert the s6.3
//! cross-scenario invariants (no data loss, no duplicate remote objects,
//! bounded memory, clean shutdown, no `pending_ops` leak) ON TOP of the
//! per-scenario outcome. To keep the two registries collision-free every name
//! here is prefixed `mutator-` / `fuzz-`.
//!
//! Every scenario boots the headless core via [`DrivenHandle`] against an
//! [`InMemoryRemoteStore`] (plus the s5 fault-injection builders) and a real
//! temp dir, exactly as the M3 `e2e_fake` acceptance suite does.
//!
//! ## Honest capability gates (STRESS_HARNESS s2.5 / s8)
//!
//! The lock-based filesystem mutators (`mutator-fs-frequent-lock-unlock`,
//! `mutator-fs-constantly-locked-db`) need Windows exclusive-share-mode
//! semantics to make a file genuinely un-openable by the scanner; on Unix an
//! advisory lock does not block Driven's read, so those scenarios would assert
//! a behaviour the platform cannot produce. They therefore declare
//! [`Capability::Windows`] and SKIP-with-reason off Windows rather than fake a
//! weakened outcome.
//!
//! ## What the M3 core actually does on a Drive-side fault (verified)
//!
//! The M3 orchestrator does NOT pause the account on a quota / dest-folder
//! failure: the executor returns a per-op `OpOutcome::Failed { code }`
//! (`crates/driven-core/src/executor.rs` `RetryDecision::Fail`), the
//! orchestrator writes a durable `activity_log` ERROR row whose `event_type`
//! IS the stable SPEC s24 code, leaves the source scan-due so it retries, and
//! the cycle still ends `Idle`. The STRESS_HARNESS s9 "transition to
//! `Paused { AccountQuotaExhausted }` within 5s" post-failure state is an
//! M4-and-later behaviour; asserting it here would be asserting a capability
//! the core does not yet have, so the drive-side scenarios assert the exact
//! error code + the s6.3 invariants + a non-crashed terminal state and record
//! a note. This is the honest direction of the no-fake rule.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use driven_core::state::{ActivityFilter, PageRequest, SourceRow, StateRepo};
use driven_core::types::{AccountId, ErrorCode, FileStateStatus, OrchestratorState, SourceId};

use driven_drive::fake::{InMemoryRemoteStore, CLIENT_OP_UUID_KEY};
use driven_drive::remote_store::RemoteStore;

use crate::capabilities::{Capability, CapabilityRequirements};
use crate::handle::{power_on_ac, DrivenHandle, DrivenHandleBuilder};
use crate::scenario::{ExpectedOutcome, Outcome, Scenario, ScenarioContext};

/// Every s4 continuous-mutation scenario, registered into the catalogue.
///
/// Order: the filesystem mutators (s4.1), then the Drive-side mutators
/// (s4.2), then the seeded fuzz smoke run (s4.3).
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        // --- s4.1 filesystem mutators ---
        Box::new(FsMutatorScenario::frequent_edits()),
        Box::new(FsMutatorScenario::truncate_and_rewrite()),
        Box::new(FsMutatorScenario::append_only_log()),
        Box::new(FsMutatorScenario::rename_storm()),
        Box::new(FsMutatorScenario::editor_tilde_dance()),
        Box::new(FsMutatorScenario::atomic_replace()),
        Box::new(FsMutatorScenario::frequent_lock_unlock()),
        Box::new(FsMutatorScenario::constantly_locked_db()),
        // --- s4.2 Drive-side mutators ---
        Box::new(DriveMutatorScenario::quota_mid_upload()),
        Box::new(DriveMutatorScenario::daily_quota_exhausted()),
        Box::new(DriveMutatorScenario::rate_limit_storm()),
        Box::new(DriveMutatorScenario::five_hundred_storm()),
        Box::new(DriveMutatorScenario::md5_mismatch()),
        Box::new(DriveMutatorScenario::invalid_grant()),
        Box::new(DriveMutatorScenario::dest_folder_deleted()),
        Box::new(DriveMutatorScenario::network_drop()),
        // --- s4.3 fuzz (registered smoke variant; CLI uses run_fuzz) ---
        Box::new(FuzzSmokeScenario::default()),
    ]
}

// ===========================================================================
// Shared fixture + invariant helpers (STRESS_HARNESS s6.3)
// ===========================================================================

/// Per-scenario wall-clock cap (STRESS_HARNESS s6.3 "no infinite loop"). The
/// registered scenarios run in the ~5-min `chaos-hermetic` job, so they use a
/// bounded ITERATION count + tiny intervals rather than the 5-minute soak
/// default (which is reserved for the `fuzz --duration` CLI path). This cap is
/// a backstop the driver can enforce; exceeding it is `harness.timeout`.
pub const SCENARIO_WALL_CAP: Duration = Duration::from_secs(60);

/// Cycles a registered soak scenario interleaves with mutation (small, fast,
/// deterministic - not the 5-min soak).
const SOAK_CYCLES: u32 = 16;

/// Settling cycles run AFTER the mutator stops, to drive eventual consistency
/// (STRESS_HARNESS s3.6 "after N more cycles past mutation stop, Drive matches
/// local"). Two is enough: one to pick up the final mutation, one to confirm a
/// no-op steady state.
const SETTLE_CYCLES: u32 = 3;

/// Write a file under `root`, creating parents (mirrors the e2e helper).
fn write_file(root: &Path, rel: &str, contents: &[u8]) -> std::io::Result<()> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, contents)
}

/// Build a flat, unencrypted source rooted at `root` whose destination is the
/// fake's root folder. Unencrypted so the s6.3 data-loss check can byte-compare
/// the remote object against the local file directly (an encrypted source
/// stores ciphertext remotely, which a naive byte-equality check would reject).
fn flat_source(account: AccountId, root: &Path, folder_id: &str) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id: account,
        display_name: "mutator".into(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: folder_id.to_string(),
        drive_folder_path: "/mutator".into(),
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

/// Boot a [`DrivenHandle`] over the given fault-injected (or plain) remote, in a
/// hermetic state DB under `state_dir`.
async fn boot_handle(
    state_dir: &Path,
    remote: Arc<InMemoryRemoteStore>,
) -> anyhow::Result<DrivenHandle> {
    DrivenHandleBuilder::new(state_dir.join("state.db"))
        .remote(remote)
        .power(power_on_ac())
        .boot()
        .await
}

/// Collect the distinct stable [`ErrorCode`]s the orchestrator surfaced this
/// run, by reading the `activity_log` rows whose `event_type` IS the dotted
/// SPEC s24 code (the orchestrator's `record_outcome_activity` /
/// `record_collisions` write the code as `event_type`). This is how a scenario
/// asserts `GracefulFailureWith` on a stable code rather than message text
/// (STRESS_HARNESS s9).
async fn error_codes_seen(state: &Arc<dyn StateRepo>) -> anyhow::Result<Vec<ErrorCode>> {
    let page = state
        .query_activity(ActivityFilter::default(), PageRequest::first(10_000))
        .await?;
    let mut codes: Vec<ErrorCode> = page
        .rows
        .iter()
        .filter_map(|r| ErrorCode::from_code(&r.event_type))
        .collect();
    codes.sort_by_key(|c| c.code());
    codes.dedup();
    Ok(codes)
}

/// Whether the orchestrator quiesced to a non-running, non-Error terminal
/// state (Idle / Paused / Backoff) - the s6.3 "clean shutdown" check for a
/// soak / drive-side row. An `Error` halt or a still-running phase is NOT a
/// clean shutdown. Read from the REAL terminal state (never hardcoded) so the
/// runner-enforced [`crate::scenario::InvariantOutcome::clean_shutdown`] flag
/// reflects what actually happened.
fn quiesced_clean(state: &OrchestratorState) -> bool {
    matches!(
        state,
        OrchestratorState::Idle { .. }
            | OrchestratorState::Paused { .. }
            | OrchestratorState::Backoff { .. }
    )
}

/// Result of the cross-scenario invariant sweep (STRESS_HARNESS s6.3).
struct InvariantReport {
    /// Live (non-trashed) remote object count at the end of the run.
    final_drive_object_count: u64,
    /// Whether every `status='synced'` file_state row's remote bytes still
    /// equal the current local bytes (the per-scenario data-loss check).
    hash_matches_local: bool,
    /// Notes about any invariant that was checked / any benign observation.
    notes: Vec<String>,
}

/// Run the STRESS_HARNESS s6.3 cross-scenario invariant sweep against the
/// terminal state of a handle. Returns an [`InvariantReport`]; a hard
/// violation (duplicate op-uuid, data loss, pending_ops leak) returns `Err` so
/// the scenario FAILS, distinct from the scenario's own outcome.
///
/// The "bounded memory / no panic / no `unwrap` in logs" invariants are
/// enforced structurally elsewhere (the harness aborts on a panic, and the
/// no-unwrap rule is a code-review/CI gate over `tracing` captures); this sweep
/// covers the state-observable ones: no duplicate Drive object per
/// `client_op_uuid`, no data loss for synced rows, no leaked `pending_ops`.
async fn assert_invariants(
    handle: &DrivenHandle,
    source: &SourceRow,
    remote: &InMemoryRemoteStore,
) -> anyhow::Result<InvariantReport> {
    assert_invariants_opts(handle, source, remote, false).await
}

/// [`assert_invariants`] with explicit options.
///
/// `allow_deferred_reconcile`: when a transient fault hits a fresh small CREATE
/// the executor KEEPS the pending op (DESIGN s5.6 `DeferToReconcile`) as the
/// recovery handle rather than dropping it - and the startup-gated reconcile
/// cannot re-run within a single in-session handle. That is a legitimate
/// recovery artifact, not a stranded leak, so the Drive-side network/5xx
/// scenarios pass `true` here: a DUE pending op is then tolerated IFF it is a
/// well-formed deferred-create op (an `upload` op carrying a `client_op_uuid`
/// with a null `drive_file_id`). Any OTHER due op is still a hard leak.
async fn assert_invariants_opts(
    handle: &DrivenHandle,
    source: &SourceRow,
    remote: &InMemoryRemoteStore,
    allow_deferred_reconcile: bool,
) -> anyhow::Result<InvariantReport> {
    let mut notes = Vec::new();

    // --- No duplicate Drive objects for the same client_op_uuid (s6.3) ------
    // Include trashed objects: a duplicate that was then trashed is still a
    // bug (two creates for one op).
    let all = remote.list_folder_with_trashed(&source.drive_folder_id);
    let mut by_uuid: HashMap<String, u64> = HashMap::new();
    for entry in &all {
        if let Some(uuid) = entry.app_properties.get(CLIENT_OP_UUID_KEY) {
            *by_uuid.entry(uuid.clone()).or_insert(0) += 1;
        }
    }
    if let Some((uuid, n)) = by_uuid.iter().find(|(_, n)| **n > 1) {
        anyhow::bail!("s6.3 invariant violated: {n} Drive objects share client_op_uuid {uuid}");
    }

    // --- No data loss: every synced row's remote object exists + (when
    //     readable) its bytes match local (s6.3) ------------------------------
    //
    // Existence is checked via the FAULT-FREE `list_folder_with_trashed`
    // accessor (a direct in-memory lock, NOT a trait call) so a scenario that
    // leaves the remote in a LATCHED fault state (e.g. auth.invalid_grant,
    // which the fake applies to read calls too) can still have its data-loss
    // invariant verified. The byte-compare goes through the faulted `download`
    // trait method; if the active fault blocks it we degrade to existence-only
    // and record a note rather than spuriously failing the scenario.
    let file_state = handle.state.load_source_file_state(source.id).await?;
    // Stays true: the byte-compare mismatch path below bails hard (an
    // s6.3 data-loss is a scenario failure), so reaching the struct build
    // means every synced row's bytes matched.
    let hash_matches_local = true;
    let src_root = PathBuf::from(&source.local_path);
    // Pre-index the fault-free object listing by id for O(1) existence checks.
    let live_by_id: HashMap<String, bool> = all.iter().map(|e| (e.id.clone(), e.trashed)).collect();
    for (rel, row) in &file_state {
        if row.status != FileStateStatus::Synced {
            continue;
        }
        let drive_file_id = match &row.drive_file_id {
            Some(id) => id,
            None => {
                anyhow::bail!("s6.3 data-loss: synced file_state for {rel} has no drive_file_id");
            }
        };
        // The remote object must still exist AND not be trashed out from under
        // a synced row (fault-free existence check).
        match live_by_id.get(drive_file_id) {
            None => anyhow::bail!(
                "s6.3 data-loss: synced {rel} (id {drive_file_id}) is gone from the remote"
            ),
            Some(true) => anyhow::bail!(
                "s6.3 data-loss: synced {rel} (id {drive_file_id}) was trashed while still marked synced"
            ),
            Some(false) => {}
        }
        // Byte-compare remote vs local (stronger than an md5 check, and works
        // without a hashing dep). Unencrypted source => remote bytes == local.
        let local_bytes = match std::fs::read(src_root.join(rel.as_str())) {
            Ok(b) => b,
            Err(_) => {
                // The local file was deleted but the row is still 'synced':
                // that is a stale row the next cycle would trash, not data
                // loss of the remote. Note it, don't fail.
                notes.push(format!(
                    "synced row {rel} has no local file (pending implicit-delete); skipped byte-compare"
                ));
                continue;
            }
        };
        let mut remote_bytes = Vec::new();
        match remote.download(drive_file_id).await {
            Ok(mut stream) => {
                stream.0.read_to_end(&mut remote_bytes).await?;
                if remote_bytes != local_bytes {
                    anyhow::bail!(
                        "s6.3 data-loss: remote bytes for synced {rel} differ from local ({} vs {} bytes)",
                        remote_bytes.len(),
                        local_bytes.len()
                    );
                }
            }
            Err(e) => {
                // The remote is in a latched-fault state that blocks reads;
                // existence was already verified fault-free above. Degrade to
                // existence-only and record why the byte-compare was skipped.
                notes.push(format!(
                    "byte-compare for synced {rel} skipped: remote read blocked by an active fault ({e}); existence + non-trashed verified fault-free"
                ));
            }
        }
    }

    // --- No pending_ops leak (s6.3): post-run either empty or only future ----
    let now = handle.clock.now_ms();
    let pending = handle.state.get_pending_ops_for_source(source.id).await?;
    let mut deferred = 0u64;
    for op in &pending {
        if op.scheduled_for <= now {
            // A due op is normally a leak. The one legitimate exception is a
            // deferred-create reconcile op when the caller opts in (see the
            // doc on `allow_deferred_reconcile`).
            let is_deferred_create = op.op_type.as_str() == "upload"
                && op
                    .payload_json
                    .get("client_op_uuid")
                    .is_some_and(|v| !v.is_null())
                && op
                    .payload_json
                    .get("drive_file_id")
                    .map(|v| v.is_null())
                    .unwrap_or(true);
            if allow_deferred_reconcile && is_deferred_create {
                deferred += 1;
                continue;
            }
            anyhow::bail!(
                "s6.3 invariant violated: pending_op {} ({}) is due ({} <= {now}) at teardown - a leak, not a legitimate backoff or deferred reconcile",
                op.id,
                op.op_type.as_str(),
                op.scheduled_for
            );
        }
    }
    if deferred > 0 {
        notes.push(format!(
            "{deferred} deferred-create reconcile op(s) remain (DESIGN s5.6 recovery handle; startup reconcile resolves them on next boot)"
        ));
    }
    let future = pending.len() as u64 - deferred;
    if future > 0 {
        notes.push(format!(
            "{future} pending_op(s) remain, all scheduled in the future (legitimate backoff)"
        ));
    }

    // --- Clean shutdown: the orchestrator must rest in a non-Error state -----
    match handle.state().await {
        OrchestratorState::Error { detail } => {
            anyhow::bail!(
                "s6.3 clean-shutdown violated: orchestrator halted in Error state ({})",
                detail.code
            );
        }
        other => notes.push(format!("terminal orchestrator state: {other:?}")),
    }

    // Fault-free terminal object count (reuse the `all` listing from the
    // duplicate-uuid check; the trait `list_folder` would be blocked by a
    // latched fault).
    let final_drive_object_count = all.iter().filter(|e| !e.trashed).count() as u64;
    Ok(InvariantReport {
        final_drive_object_count,
        hash_matches_local,
        notes,
    })
}

// ===========================================================================
// s4.1 - filesystem mutator scenarios
// ===========================================================================

/// Which [`crate::mutator::FsMutation`] flavour a filesystem-mutator scenario
/// runs. Mirrors the s4.1 enum but carries the concrete per-scenario knobs the
/// soak loop needs, so the loop body is a single match.
#[derive(Clone)]
enum FsKind {
    /// Edit a single file every tick (`frequent-edits`).
    EditFile,
    /// `O_TRUNC + write` a single file every tick (`truncate-and-rewrite`).
    TruncateRewrite,
    /// Append a fixed chunk every tick (`append-only-log`).
    AppendOnly,
    /// Rename files within the source dir every tick (`rename-storm`).
    RenameStorm,
    /// Word/Photoshop tmp-then-rename pattern (`editor-tilde-dance`).
    EditorTildeDance,
    /// Atomic replace via `.tmp` + rename over the target (`replace-via-atomic-rename`).
    AtomicReplace,
    /// Lock/unlock a file every tick (`frequent-lock-unlock`). Windows-only.
    LockUnlock,
    /// Hold a file write-exclusive for the duration (`constantly-locked-db`). Windows-only.
    HoldLocked,
}

/// A filesystem-mutator soak scenario (STRESS_HARNESS s4.1). Spawns the
/// mutation loop on a dedicated OS thread (NOT a tokio task, per s4.1: the
/// mutator's scheduling stays independent of Driven's I/O reactor), interleaves
/// [`SOAK_CYCLES`] manual sync cycles WHILE the thread mutates, stops + joins
/// the thread (clean shutdown), then runs [`SETTLE_CYCLES`] settling cycles and
/// asserts eventual consistency + the s6.3 invariants.
struct FsMutatorScenario {
    name: &'static str,
    description: &'static str,
    kind: FsKind,
    /// Whether this flavour needs Windows exclusive-lock semantics.
    needs_windows: bool,
}

impl FsMutatorScenario {
    fn frequent_edits() -> Self {
        Self {
            name: "mutator-fs-frequent-edits",
            description: "s4.1 EditFile soak: a file edited every tick while sync runs; eventual consistency + no corrupt upload",
            kind: FsKind::EditFile,
            needs_windows: false,
        }
    }
    fn truncate_and_rewrite() -> Self {
        Self {
            name: "mutator-fs-truncate-rewrite",
            description: "s4.1 TruncateRewrite soak: O_TRUNC+write every tick (Word/Excel atomic-write); no false synced, eventual consistency",
            kind: FsKind::TruncateRewrite,
            needs_windows: false,
        }
    }
    fn append_only_log() -> Self {
        Self {
            name: "mutator-fs-append-only-log",
            description: "s4.1 AppendOnly soak: file appended every tick; each upload a coherent snapshot, final state matches local",
            kind: FsKind::AppendOnly,
            needs_windows: false,
        }
    }
    fn rename_storm() -> Self {
        Self {
            name: "mutator-fs-rename-storm",
            description: "s4.1 RenameStorm soak: files renamed mid-scan; new path uploaded, old trashed, no data loss",
            kind: FsKind::RenameStorm,
            needs_windows: false,
        }
    }
    fn editor_tilde_dance() -> Self {
        Self {
            name: "mutator-fs-editor-tilde-dance",
            description: "s4.1 EditorTildeDance soak: write ~$tmp, rename over target, delete tmp; current V1 behaviour documented",
            kind: FsKind::EditorTildeDance,
            needs_windows: false,
        }
    }
    fn atomic_replace() -> Self {
        Self {
            name: "mutator-fs-atomic-replace",
            description: "s4.1 AtomicReplace soak: .tmp written then renamed over target every tick; no partial commit, eventual consistency",
            kind: FsKind::AtomicReplace,
            needs_windows: false,
        }
    }
    fn frequent_lock_unlock() -> Self {
        Self {
            name: "mutator-fs-frequent-lock-unlock",
            description: "s4.1 LockUnlock soak (Windows): file locked/unlocked every tick; sharing-violation handled, eventually synced",
            kind: FsKind::LockUnlock,
            needs_windows: true,
        }
    }
    fn constantly_locked_db() -> Self {
        Self {
            name: "mutator-fs-constantly-locked-db",
            description: "s4.1 HoldLocked soak (Windows): a PST-like file held write-exclusive; local.file_locked surfaced, no crash",
            kind: FsKind::HoldLocked,
            needs_windows: true,
        }
    }

    /// Seed the on-disk fixture: a handful of small files plus the mutation
    /// target, so the scan has a stable population around the churning file.
    fn seed_fixture(&self, root: &Path) -> anyhow::Result<()> {
        for i in 0..6u32 {
            write_file(
                root,
                &format!("stable-{i:02}.txt"),
                format!("stable-{i}").as_bytes(),
            )?;
        }
        match self.kind {
            FsKind::RenameStorm => {
                for i in 0..4u32 {
                    write_file(
                        root,
                        &format!("roamer-{i}.dat"),
                        format!("roam-{i}").as_bytes(),
                    )?;
                }
            }
            _ => {
                write_file(root, "target.bin", b"v0-initial-contents")?;
            }
        }
        Ok(())
    }
}

/// Run one filesystem mutation tick against `root`. Pure synchronous std::fs so
/// it can live on the dedicated OS thread. `tick` increments each call.
///
/// Returns `Ok(())` on success; an I/O error (e.g. a transient sharing
/// violation when the scanner has the file open) is swallowed into `Ok` - the
/// mutator deliberately races the scanner and a momentary failure to open the
/// file is the very contention the scenario exercises, not a scenario error.
fn fs_mutation_tick(kind: &FsKind, root: &Path, tick: u64) -> std::io::Result<()> {
    let target = root.join("target.bin");
    let result = match kind {
        FsKind::EditFile => std::fs::write(&target, format!("edit-{tick}").as_bytes()),
        FsKind::TruncateRewrite => {
            // Truncate to zero then write fresh bytes (the Word/Excel pattern).
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .create(true)
                .open(&target)?;
            f.write_all(format!("trunc-rewrite-{tick}-payload").as_bytes())
        }
        FsKind::AppendOnly => {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&target)?;
            f.write_all(format!("[append {tick:08}]").as_bytes())
        }
        FsKind::RenameStorm => {
            // Rotate roamer-i -> roamer-(i renamed) deterministically.
            let from = root.join(format!("roamer-{}.dat", tick % 4));
            let to = root.join(format!("roamer-{}-r{tick}.dat", tick % 4));
            if from.exists() {
                std::fs::rename(&from, &to)
            } else {
                Ok(())
            }
        }
        FsKind::EditorTildeDance => {
            // write ~$target.tmp, rename over target, (no leftover tmp).
            let tmp = root.join("~$target.tmp");
            std::fs::write(&tmp, format!("tilde-{tick}").as_bytes())?;
            std::fs::rename(&tmp, &target)
        }
        FsKind::AtomicReplace => {
            let tmp = root.join(format!("target.bin.tmp{tick}"));
            std::fs::write(&tmp, format!("atomic-{tick}-bytes").as_bytes())?;
            std::fs::rename(&tmp, &target)
        }
        FsKind::LockUnlock | FsKind::HoldLocked => {
            // The lock kinds do NOT use this generic tick path - they run a
            // dedicated Windows exclusive-share-mode holder thread (see
            // `run_lock_holder`). This arm is unreachable for them; kept total
            // so the match is exhaustive.
            Ok(())
        }
    };
    match result {
        Ok(()) => Ok(()),
        // Swallow the race-contention errors (the file being momentarily open
        // by the scanner). Any other error propagates.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Run the Windows exclusive-share-mode lock holder for the `LockUnlock` /
/// `HoldLocked` scenarios (STRESS_HARNESS s4.1 / s3.6 `frequent-lock-unlock`,
/// `constantly-locked-db`).
///
/// Opens `target.bin` with `share_mode(0)` (no shared read/write/delete) via
/// the std-only [`std::os::windows::fs::OpenOptionsExt`] - NO extra crate - so
/// the scanner's `open` of the same path genuinely returns
/// `ERROR_SHARING_VIOLATION`, the real condition SPEC s24 `local.file_locked`
/// covers. `HoldLocked` keeps one handle open for the whole run; `LockUnlock`
/// drops + re-acquires the exclusive handle every ~120ms so the scanner
/// sometimes wins the race (the "eventually synced" half of the assertion).
///
/// Only compiled on Windows; the two lock scenarios are
/// [`Capability::Windows`]-gated so this is never reached elsewhere.
#[cfg(windows)]
fn run_lock_holder(kind: &FsKind, root: &Path, stop: &AtomicBool) -> std::io::Result<u64> {
    use std::os::windows::fs::OpenOptionsExt;
    let target = root.join("target.bin");
    let mut cycles = 0u64;
    let open_exclusive = |path: &Path| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // Open-or-create to take an exclusive lock; do NOT truncate (we
            // are locking the file's existing bytes, not rewriting them).
            .truncate(false)
            // share_mode(0): deny all sharing -> a concurrent open by the
            // scanner fails with ERROR_SHARING_VIOLATION.
            .share_mode(0)
            .open(path)
    };
    match kind {
        FsKind::HoldLocked => {
            // Hold one exclusive handle for the duration (the PST/locked-DB
            // pattern). The scanner cannot open the file the whole time.
            let _held = open_exclusive(&target)?;
            while !stop.load(Ordering::Relaxed) {
                cycles += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        FsKind::LockUnlock => {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(h) = open_exclusive(&target) {
                    // Hold the exclusive lock briefly, then release so the
                    // scanner can sometimes win and eventually sync the file.
                    std::thread::sleep(Duration::from_millis(60));
                    drop(h);
                }
                cycles += 1;
                std::thread::sleep(Duration::from_millis(60));
            }
        }
        _ => {
            // Only the two lock kinds reach here (the caller gates on them);
            // return an error rather than panic if that contract is ever broken.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "run_lock_holder called with a non-lock FsKind",
            ));
        }
    }
    Ok(cycles)
}

#[async_trait]
impl Scenario for FsMutatorScenario {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        self.description
    }
    fn requires(&self) -> CapabilityRequirements {
        if self.needs_windows {
            CapabilityRequirements::of(vec![Capability::Windows])
        } else {
            CapabilityRequirements::none()
        }
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        std::fs::create_dir_all(&ctx.fixture_root)?;
        self.seed_fixture(&ctx.fixture_root)?;
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        // This scenario needs a source pointing at its OWN fixture dir + the
        // concrete fake remote for the fault-free invariant sweep, so it boots
        // a self-contained handle rather than using the generic placeholder the
        // driver passes (whose remote type is erased behind `dyn RemoteStore`
        // and whose source wiring is scenario-specific).
        let state_dir = tempfile::tempdir()?;
        let root_dir = tempfile::tempdir()?;
        self.seed_fixture(root_dir.path())?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let folder = remote.root_id().to_string();
        let local = boot_handle(state_dir.path(), remote.clone()).await?;
        let source = flat_source(local.account_id, root_dir.path(), &folder);
        local.state.upsert_source(&source).await?;

        // Prime: one cycle to upload the initial population before churn.
        // For HoldLocked the file is exclusively locked the WHOLE run, so we do
        // NOT prime it (the prime would also fail to read it); for every other
        // kind the prime establishes the baseline before churn.
        if !matches!(self.kind, FsKind::HoldLocked) {
            local.run_one_cycle().await?;
        }

        // --- spawn the mutator thread (s4.1: dedicated OS thread) -----------
        // Lock kinds run the Windows exclusive-share-mode holder; every other
        // kind runs the generic std::fs mutation loop. Both honour the same
        // stop flag so teardown joins cleanly (s6.3).
        let stop = Arc::new(AtomicBool::new(false));
        let kind = self.kind.clone();
        let root = root_dir.path().to_path_buf();
        let stop_t = stop.clone();
        let mutator_thread = std::thread::Builder::new()
            .name(format!("mutator-{}", self.name))
            .spawn(move || -> std::io::Result<u64> {
                if matches!(kind, FsKind::LockUnlock | FsKind::HoldLocked) {
                    // Windows-only (the scenario is Capability::Windows-gated).
                    #[cfg(windows)]
                    {
                        return run_lock_holder(&kind, &root, &stop_t);
                    }
                    // Off-Windows the capability gate SKIPs before we get here;
                    // this arm is unreachable but keeps the thread total.
                    #[cfg(not(windows))]
                    {
                        let _ = (&kind, &root);
                        while !stop_t.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        return Ok(0);
                    }
                }
                let mut tick = 0u64;
                while !stop_t.load(Ordering::Relaxed) {
                    fs_mutation_tick(&kind, &root, tick)?;
                    tick += 1;
                    // ~10ms cadence keeps the registered scenario fast while
                    // still racing the scanner across many cycles.
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(tick)
            })?;

        // --- interleave sync cycles WHILE the thread mutates ----------------
        let started = std::time::Instant::now();
        for _ in 0..SOAK_CYCLES {
            if started.elapsed() > SCENARIO_WALL_CAP {
                stop.store(true, Ordering::Relaxed);
                let _ = mutator_thread.join();
                anyhow::bail!("{}: exceeded wall-clock cap (harness.timeout)", self.name);
            }
            // A cycle racing live mutation must never crash; a transient
            // file-changed/locked op surfaces as an activity row, not an Err.
            local.run_one_cycle().await?;
        }

        // --- stop + join the mutator (clean shutdown, s6.3) -----------------
        stop.store(true, Ordering::Relaxed);
        let ticks = mutator_thread
            .join()
            .map_err(|_| anyhow::anyhow!("{}: mutator thread panicked", self.name))??;

        // Pin a DETERMINISTIC final state before settling, so the "Drive == local
        // on the next cycle" eventual-consistency check is not at the mercy of
        // filesystem mtime granularity (a final edit whose SIZE matches a
        // previously-synced version could be missed by the (size, mtime)
        // fast-path on a coarse-mtime volume, leaving stale bytes on Drive). We
        // write a UNIQUE-LENGTH sentinel (the e2e `change_five_files` trick:
        // vary the size, not just the content) to the churned file so the scan
        // re-uploads it unconditionally and convergence is genuine. Skipped for
        // rename-storm (no single target file) and the lock kinds (the file is
        // / was exclusively locked; their convergence is asserted via the
        // settle cycles after the lock releases).
        if !matches!(
            self.kind,
            FsKind::RenameStorm | FsKind::LockUnlock | FsKind::HoldLocked
        ) {
            let unique = format!(
                "FINAL-SENTINEL-{}-{}",
                self.name,
                "z".repeat((ticks % 23 + 1) as usize)
            );
            write_file(root_dir.path(), "target.bin", unique.as_bytes())?;
        }

        // --- settle: drive eventual consistency past the last mutation ------
        for _ in 0..SETTLE_CYCLES {
            local.run_one_cycle().await?;
        }

        // --- invariants + per-scenario outcome ------------------------------
        // A file replaced mid-first-upload (truncate/atomic-replace racing the
        // scanner) can leave a deferred-create reconcile op (DESIGN s5.6,
        // SkipPostUpload-create) - the same legitimate recovery handle the
        // Drive-side transients produce, not a leak. Tolerate it here too.
        let report = assert_invariants_opts(&local, &source, &remote, true).await?;
        let codes = error_codes_seen(&local.state).await?;

        // Route the terminal state through the CANONICAL s6.3 sweep so the
        // runner-enforced invariant snapshot is computed the same way as every
        // other category. The local `assert_invariants_opts` above already did
        // the stronger byte-level + deferred-reconcile-aware checks (and bailed
        // on any hard violation); this gives the central runner its snapshot.
        let inv_report =
            crate::scenarios::reporting::assert_invariants(&local, &remote, source.id, &folder)
                .await?;
        let clean_shutdown = quiesced_clean(&local.state().await);

        let mut notes = report.notes;
        notes.push(format!("{} mutation ticks applied", ticks));
        notes.extend(codes.iter().map(|c| format!("surfaced code: {}", c.code())));

        // `HoldLocked` (constantly-locked-db) holds the file write-exclusive for
        // the WHOLE soak, so the scanner is GUARANTEED to hit the sharing
        // violation and `local.file_locked` MUST surface - a real condition, not
        // a faked one. `LockUnlock` (frequent-lock-unlock), by contrast, is a
        // FLAP: it holds the exclusive handle only ~60ms per tick and releases
        // between, precisely "so the scanner can sometimes win and eventually
        // sync the file" (see run_lock_holder). Whether any scan coincides with a
        // locked window is inherently racy, so its s4.1 property is "no crash +
        // eventually synced" (asserted by the final-object-count + no-data-loss
        // invariants below), NOT that local.file_locked is guaranteed to surface.
        // Requiring the code for the flap would be asserting a race, not a
        // behaviour. (Only reached on Windows; the gate SKIPs elsewhere.)
        if matches!(self.kind, FsKind::HoldLocked) && !codes.contains(&ErrorCode::LocalFileLocked) {
            anyhow::bail!(
                "{}: expected local.file_locked to surface while the file was held exclusively for the whole soak; saw {:?}",
                self.name,
                codes
            );
        }

        // Eventual consistency: after settling with no mutation, the steady
        // state must be a no-op (no pending uploads/errors lingering). The
        // invariant sweep already proved no data loss + no duplicates; here we
        // assert the source converged (final object count > 0 since the
        // population is non-empty, OR a rename-storm may have churned names).
        if report.final_drive_object_count == 0 {
            anyhow::bail!(
                "{}: nothing landed on the remote after the soak (expected the stable population)",
                self.name
            );
        }

        Ok(Outcome {
            error_codes_seen: codes,
            final_drive_object_count: report.final_drive_object_count,
            final_hash_matches_local: report.hash_matches_local,
            notes,
            // s4.1 soak over a SINGLE source+remote: route the real terminal
            // state through the canonical s6.3 sweep so the runner enforces it
            // centrally. clean_shutdown is the real quiescence value (the loop
            // joins the mutator thread + drains settle cycles; an Error halt
            // would have bailed in assert_invariants_opts above).
            invariants: Some(inv_report.to_invariant_outcome(clean_shutdown)),
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        // The scenario owns its tempdirs inside run_assertions (dropped there);
        // the harness-provided fixture_root is cleaned by the driver. Nothing
        // additional to release.
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        // The s3.6 rows for these patterns DOCUMENT current V1 behaviour
        // (eventual consistency, no rename-detection, no editor-tmp exclude)
        // rather than asserting a single error code; the s6.3 invariants carry
        // the real PASS/FAIL. So the documented-behaviour outcome is correct,
        // with the convergence + invariant checks in run_assertions.
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// s4.2 - Drive-side mutator scenarios
// ===========================================================================

/// How a Drive-side mutator rigs the fake remote, plus the code it expects.
#[derive(Clone)]
enum DriveKind {
    /// `with_quota_exhausted_after(n)` -> `drive.quota_exhausted`.
    QuotaMidUpload { after_bytes: u64 },
    /// Daily-limit injection -> `drive.daily_quota_exhausted`. Synthesised via
    /// the quota builder is wrong (that is the storage quota); we drive the
    /// daily path through the rate-limit-then-daily contract the fake exposes.
    DailyQuota,
    /// `with_rate_limit_after(n)` -> retried, completes (no surfaced fail).
    RateLimitStorm { after_requests: u64 },
    /// `with_5xx_after(n)` -> transient, surfaces `drive.unreachable`.
    FiveHundredStorm { after_requests: u64 },
    /// `with_md5_mismatch_after(n)` -> `drive.checksum_mismatch`.
    Md5Mismatch { after_uploads: u64 },
    /// `with_invalid_grant_after(n)` -> `auth.invalid_grant`.
    InvalidGrant { after_requests: u64 },
    /// `with_dest_folder_missing()` -> `drive.dest_folder_missing`.
    DestFolderDeleted,
    /// `with_network_drop_after(n)` -> a mid-cycle Err the orchestrator
    /// surfaces; the cycle re-runs and recovers.
    NetworkDrop { after_requests: u64 },
}

/// A Drive-side fault-injection scenario (STRESS_HARNESS s4.2 / s3.7). Builds a
/// fault-rigged [`InMemoryRemoteStore`], runs sync cycles, and asserts the
/// expected stable error code surfaced (or that the run recovered) PLUS the
/// s6.3 invariants. See the module note on why no `Paused` state is asserted.
struct DriveMutatorScenario {
    name: &'static str,
    description: &'static str,
    kind: DriveKind,
    expected: ExpectedOutcome,
}

impl DriveMutatorScenario {
    fn quota_mid_upload() -> Self {
        Self {
            name: "mutator-drive-quota-mid-upload",
            description: "s4.2 InjectQuotaExhausted: 403 storageQuotaExceeded after N bytes; drive.quota_exhausted surfaced, no data loss for files done before",
            kind: DriveKind::QuotaMidUpload { after_bytes: 32 },
            expected: ExpectedOutcome::GracefulFailureWith {
                code: ErrorCode::DriveQuotaExhausted,
            },
        }
    }
    fn daily_quota_exhausted() -> Self {
        Self {
            name: "mutator-drive-daily-quota",
            description: "s4.2/s3.7 dailyLimitExceeded: drive.daily_quota_exhausted surfaced; no crash (midnight-resume timing is M4, not asserted)",
            kind: DriveKind::DailyQuota,
            expected: ExpectedOutcome::GracefulFailureWith {
                code: ErrorCode::DriveDailyQuotaExhausted,
            },
        }
    }
    fn rate_limit_storm() -> Self {
        Self {
            name: "mutator-drive-rate-limit-storm",
            description: "s4.2 InjectRateLimit: a 429 mid-sync is retried with backoff and the sync completes; no surfaced terminal failure",
            kind: DriveKind::RateLimitStorm { after_requests: 4 },
            expected: ExpectedOutcome::Success,
        }
    }
    fn five_hundred_storm() -> Self {
        Self {
            name: "mutator-drive-5xx-storm",
            description: "s4.2 InjectFiveHundred: a 5xx mid-sync surfaces drive.unreachable as a transient; the next cycle recovers",
            kind: DriveKind::FiveHundredStorm { after_requests: 4 },
            expected: ExpectedOutcome::GracefulFailureWith {
                code: ErrorCode::DriveUnreachable,
            },
        }
    }
    fn md5_mismatch() -> Self {
        Self {
            name: "mutator-drive-md5-mismatch",
            description: "s4.2 InjectMd5Mismatch: the post-upload checksum verify fails; drive.checksum_mismatch surfaced, file re-queued not falsely synced",
            kind: DriveKind::Md5Mismatch { after_uploads: 2 },
            expected: ExpectedOutcome::GracefulFailureWith {
                code: ErrorCode::DriveChecksumMismatch,
            },
        }
    }
    fn invalid_grant() -> Self {
        Self {
            name: "mutator-drive-invalid-grant",
            description: "s4.2 InjectInvalidGrant: refresh returns invalid_grant; auth.invalid_grant surfaced, no crash",
            kind: DriveKind::InvalidGrant { after_requests: 3 },
            expected: ExpectedOutcome::GracefulFailureWith {
                code: ErrorCode::AuthInvalidGrant,
            },
        }
    }
    fn dest_folder_deleted() -> Self {
        Self {
            name: "mutator-drive-dest-folder-deleted",
            description: "s4.2 DeleteDestFolder: destination folder 404s; drive.dest_folder_missing surfaced, source halted (no Paused state in M3)",
            kind: DriveKind::DestFolderDeleted,
            expected: ExpectedOutcome::GracefulFailureWith {
                code: ErrorCode::DriveDestFolderMissing,
            },
        }
    }
    fn network_drop() -> Self {
        Self {
            name: "mutator-drive-network-drop",
            description: "s4.2 DropNetwork: a single mid-cycle network drop aborts the cycle; a follow-up cycle recovers with no data loss / duplicates",
            kind: DriveKind::NetworkDrop { after_requests: 3 },
            expected: ExpectedOutcome::DocumentedBehaviour,
        }
    }

    /// Build the fault-rigged remote for this scenario's kind.
    fn build_remote(&self) -> Arc<InMemoryRemoteStore> {
        let base = InMemoryRemoteStore::new();
        let rigged = match &self.kind {
            DriveKind::QuotaMidUpload { after_bytes } => {
                base.with_quota_exhausted_after(*after_bytes)
            }
            DriveKind::DailyQuota => {
                // The fake's `with_daily_quota_after(n)` trips a faithful
                // `403 dailyLimitExceeded` after `n` WriteTarget requests, then
                // LATCHES (the daily window stays closed for the run). Its
                // message carries "daily", so the executor's
                // `classify_drive_error` maps it to `DriveError::DailyQuota` ->
                // `ErrorCode::DriveDailyQuotaExhausted` (distinct from storage
                // `quota_exhausted`). `after(4)` trips MID-run (a couple files
                // land first), matching the sibling request-counted faults and
                // the s9 "no data loss for files done before the failure" check.
                base.with_daily_quota_after(4)
            }
            DriveKind::RateLimitStorm { after_requests } => {
                base.with_rate_limit_after(*after_requests)
            }
            DriveKind::FiveHundredStorm { after_requests } => base.with_5xx_after(*after_requests),
            DriveKind::Md5Mismatch { after_uploads } => {
                base.with_md5_mismatch_after(*after_uploads)
            }
            DriveKind::InvalidGrant { after_requests } => {
                base.with_invalid_grant_after(*after_requests)
            }
            DriveKind::DestFolderDeleted => base.with_dest_folder_missing(),
            DriveKind::NetworkDrop { after_requests } => {
                base.with_network_drop_after(*after_requests)
            }
        };
        Arc::new(rigged)
    }
}

#[async_trait]
impl Scenario for DriveMutatorScenario {
    fn name(&self) -> &'static str {
        self.name
    }
    fn description(&self) -> &'static str {
        self.description
    }
    fn requires(&self) -> CapabilityRequirements {
        // Every s4.2 scenario - including daily-quota - runs against the
        // in-memory fake, so none need a host capability. The fake's
        // `with_daily_quota_after` now emits a faithful `dailyLimitExceeded`
        // (see `build_remote`), so the daily-quota row no longer gates on
        // `cap:real_drive_creds`; it runs hermetically like its siblings.
        CapabilityRequirements::none()
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        std::fs::create_dir_all(&ctx.fixture_root)?;
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        // Every kind - including daily-quota - now runs the same hermetic body:
        // `build_remote` rigs the kind's fault on the fake (the daily-quota row
        // uses the real `with_daily_quota_after` injector), and the run drives
        // cycles, collects the surfaced codes, and sweeps the canonical s6.3
        // invariants. No kind early-returns a capability SKIP any more.
        let state_dir = tempfile::tempdir()?;
        let root_dir = tempfile::tempdir()?;
        // A modest multi-file population so a "after N bytes / N requests"
        // fault lands MID-run (some files done before, some after) - the
        // discriminating setup for the s9 "no data loss for files that
        // completed before the failure" check.
        for i in 0..8u32 {
            write_file(
                root_dir.path(),
                &format!("f{i:02}.txt"),
                format!("payload-{i}-xyz").as_bytes(),
            )?;
        }

        let remote = self.build_remote();
        let folder = remote.root_id().to_string();
        let handle = boot_handle(state_dir.path(), remote.clone()).await?;
        let source = flat_source(handle.account_id, root_dir.path(), &folder);
        handle.state.upsert_source(&source).await?;

        // First cycle hits the fault. Most faults surface as per-op Failed
        // outcomes (cycle returns Ok, ends Idle). The network-drop fault makes
        // the cycle return Err (Fatal); we tolerate that and recover.
        let first = handle.run_one_cycle().await;
        let first_errored = first.is_err();

        // A second cycle: the fault builders are single-shot/after-N, so a
        // re-run recovers (the source was left scan-due). This proves recovery
        // + lets the remaining files land for the no-data-loss check.
        let mut recovered = true;
        for _ in 0..SETTLE_CYCLES {
            if let Err(e) = handle.run_one_cycle().await {
                // A persistent fault (e.g. dest-folder-missing) keeps failing;
                // that is the scenario's point, not a harness error.
                recovered = false;
                let _ = e;
            }
        }

        let codes = error_codes_seen(&handle.state).await?;
        // Drive-side transient faults (network/5xx) on a fresh small CREATE
        // leave a deferred-create reconcile op (DESIGN s5.6); tolerate that as
        // a recovery handle, not a leak.
        let report = assert_invariants_opts(&handle, &source, &remote, true).await?;

        // Canonical s6.3 snapshot for the central runner enforcement, computed
        // the same way as every other category. The stronger byte-level /
        // deferred-reconcile checks above already bailed on a hard violation.
        let inv_report =
            crate::scenarios::reporting::assert_invariants(&handle, &remote, source.id, &folder)
                .await?;
        let clean_shutdown = quiesced_clean(&handle.state().await);

        let mut notes = report.notes;
        notes.push(format!("first cycle errored: {first_errored}"));
        notes.push(format!("recovered on a follow-up cycle: {recovered}"));
        notes.extend(codes.iter().map(|c| format!("surfaced code: {}", c.code())));

        Ok(Outcome {
            error_codes_seen: codes,
            final_drive_object_count: report.final_drive_object_count,
            final_hash_matches_local: report.hash_matches_local,
            notes,
            // s4.2 fault-injection over a SINGLE source+remote: route the real
            // terminal state through the canonical s6.3 sweep. clean_shutdown is
            // the real quiescence value (an Error halt would have bailed in
            // assert_invariants_opts above; a recovered/persistent fault rests
            // in a non-running Idle/Paused/Backoff state).
            invariants: Some(inv_report.to_invariant_outcome(clean_shutdown)),
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        self.expected.clone()
    }
}

// ===========================================================================
// s4.3 - fuzz
// ===========================================================================

/// A tiny deterministic xorshift64* PRNG. Used instead of pulling in `rand` so
/// the harness stays dependency-light and the fuzz sequence is bit-reproducible
/// from a seed (STRESS_HARNESS s4.3 "bit-reproducible" failures).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Avoid the all-zero state (xorshift's fixed point).
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniform in `0..n` (n > 0).
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// One fuzz step the weighted distribution can pick (STRESS_HARNESS s4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FuzzStep {
    /// Write a brand-new file into the source tree.
    AddFile,
    /// Rewrite an existing file's bytes.
    EditFile,
    /// Truncate+rewrite an existing file.
    TruncateFile,
    /// Append to an existing file.
    AppendFile,
    /// Delete an existing file.
    DeleteFile,
    /// Run one sync cycle.
    SyncCycle,
}

/// The weighted step distribution. Edits/syncs are common; deletes rarer. The
/// weights are fixed so a seed fully determines the sequence.
const FUZZ_WEIGHTS: &[(FuzzStep, u64)] = &[
    (FuzzStep::AddFile, 4),
    (FuzzStep::EditFile, 4),
    (FuzzStep::TruncateFile, 2),
    (FuzzStep::AppendFile, 2),
    (FuzzStep::DeleteFile, 1),
    (FuzzStep::SyncCycle, 5),
];

fn pick_step(rng: &mut Rng) -> FuzzStep {
    let total: u64 = FUZZ_WEIGHTS.iter().map(|(_, w)| *w).sum();
    let mut r = rng.below(total);
    for (step, w) in FUZZ_WEIGHTS {
        if r < *w {
            return *step;
        }
        r -= *w;
    }
    FuzzStep::SyncCycle
}

/// A single applied fuzz step, recorded so a failing seed's run is replayable
/// (STRESS_HARNESS s4.3 mutation log).
#[derive(Debug, Clone, serde::Serialize)]
struct FuzzLogEntry {
    step_index: u64,
    step: String,
    target: Option<String>,
}

/// The outcome of a fuzz run, suitable for a failure artifact.
#[derive(Debug, serde::Serialize)]
pub struct FuzzReport {
    /// The seed that produced this run.
    pub seed: u64,
    /// Number of steps applied.
    pub steps: u64,
    /// The applied mutation log (for bit-reproducible replay).
    log: Vec<FuzzLogEntry>,
    /// `None` on success; `Some(reason)` if an invariant was violated.
    pub violation: Option<String>,
}

/// Run a seeded fuzz session (STRESS_HARNESS s4.3): pick a random weighted
/// sequence of filesystem mutations interleaved with sync cycles against a
/// synthesised source tree, then assert the s6.3 post-condition invariants.
///
/// `step_budget` and `wall_cap` BOTH bound the run, whichever is hit first: the
/// registered `fuzz-smoke` row passes a small step budget + the 60s
/// [`SCENARIO_WALL_CAP`] so it stays fast, while the `fuzz --duration D` CLI
/// path passes a very large step budget + `wall_cap = D` so a local soak
/// actually runs for the requested wall-clock (`--duration 6h` soaks 6h, not
/// 60s). On any invariant violation the returned report carries
/// `violation = Some(..)` and the full mutation log, which the caller writes to
/// `target/chaos-fuzz-failures/<seed>.json` for bit-reproducible replay.
pub async fn run_fuzz(
    seed: u64,
    step_budget: u64,
    wall_cap: Duration,
) -> anyhow::Result<FuzzReport> {
    let mut rng = Rng::new(seed);
    let state_dir = tempfile::tempdir()?;
    let root_dir = tempfile::tempdir()?;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let folder = remote.root_id().to_string();
    let handle = boot_handle(state_dir.path(), remote.clone()).await?;
    let source = flat_source(handle.account_id, root_dir.path(), &folder);
    handle.state.upsert_source(&source).await?;

    // Seed an initial file so edit/delete steps have something to act on.
    write_file(root_dir.path(), "seed-000.txt", b"seed")?;
    let mut files: Vec<String> = vec!["seed-000.txt".to_string()];
    let mut next_id: u64 = 1;
    let mut log: Vec<FuzzLogEntry> = Vec::new();

    let started = std::time::Instant::now();
    for step_index in 0..step_budget {
        if started.elapsed() > wall_cap {
            break;
        }
        let step = pick_step(&mut rng);
        let mut target = None;
        match step {
            FuzzStep::AddFile => {
                let name = format!("fz-{next_id:04}.dat");
                next_id += 1;
                write_file(
                    root_dir.path(),
                    &name,
                    format!("add-{step_index}").as_bytes(),
                )?;
                target = Some(name.clone());
                files.push(name);
            }
            FuzzStep::EditFile if !files.is_empty() => {
                let name = files[(rng.below(files.len() as u64)) as usize].clone();
                let _ = std::fs::write(
                    root_dir.path().join(&name),
                    format!("edit-{step_index}-newbytes").as_bytes(),
                );
                target = Some(name);
            }
            FuzzStep::TruncateFile if !files.is_empty() => {
                let name = files[(rng.below(files.len() as u64)) as usize].clone();
                let _ = std::fs::write(
                    root_dir.path().join(&name),
                    format!("t{step_index}").as_bytes(),
                );
                target = Some(name);
            }
            FuzzStep::AppendFile if !files.is_empty() => {
                let name = files[(rng.below(files.len() as u64)) as usize].clone();
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .append(true)
                    .open(root_dir.path().join(&name))
                {
                    let _ = f.write_all(format!("[a{step_index}]").as_bytes());
                }
                target = Some(name);
            }
            FuzzStep::DeleteFile if files.len() > 1 => {
                let idx = (rng.below(files.len() as u64)) as usize;
                let name = files.remove(idx);
                let _ = std::fs::remove_file(root_dir.path().join(&name));
                target = Some(name);
            }
            FuzzStep::SyncCycle => {
                // A fuzz sync cycle must never panic; a transient mid-mutation
                // failure is acceptable and recovered next cycle.
                let _ = handle.run_one_cycle().await;
            }
            // Steps whose precondition didn't hold (e.g. EditFile with no
            // files) degrade to a sync cycle so the budget still advances.
            _ => {
                let _ = handle.run_one_cycle().await;
            }
        }
        log.push(FuzzLogEntry {
            step_index,
            step: format!("{step:?}"),
            target,
        });
    }

    // Settle to a steady state, then assert the s6.3 invariants.
    for _ in 0..SETTLE_CYCLES {
        let _ = handle.run_one_cycle().await;
    }

    let steps = log.len() as u64;
    let violation = match assert_invariants(&handle, &source, &remote).await {
        Ok(_) => None,
        Err(e) => Some(e.to_string()),
    };

    Ok(FuzzReport {
        seed,
        steps,
        log,
        violation,
    })
}

/// Persist a failing fuzz run's seed + mutation log to
/// `target/chaos-fuzz-failures/<seed>.json` (STRESS_HARNESS s4.3) so the
/// failure is bit-reproducible. Returns the path written.
pub fn write_fuzz_failure(report: &FuzzReport) -> anyhow::Result<PathBuf> {
    let dir = PathBuf::from("target/chaos-fuzz-failures");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", report.seed));
    let json = serde_json::to_vec_pretty(report)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Driver for the `mutator fs --scenario <name>` CLI subcommand
/// (STRESS_HARNESS s2.2): look the named filesystem-mutator scenario up and run
/// it. Returns its [`Outcome`]. The Integrate agent wires this into
/// `dispatch::run`.
pub async fn run_fs_mutator(scenario_name: &str) -> anyhow::Result<Outcome> {
    run_named(scenario_name, "mutator-fs-").await
}

/// Driver for the `mutator drive --scenario <name>` CLI subcommand
/// (STRESS_HARNESS s2.2).
pub async fn run_drive_mutator(scenario_name: &str) -> anyhow::Result<Outcome> {
    run_named(scenario_name, "mutator-drive-").await
}

/// Shared body for the two mutator CLI drivers: find the scenario by name,
/// validate its prefix, boot a generic handle, and run its assertions.
async fn run_named(scenario_name: &str, expected_prefix: &str) -> anyhow::Result<Outcome> {
    if !scenario_name.starts_with(expected_prefix) {
        anyhow::bail!("scenario {scenario_name:?} is not a `{expected_prefix}` mutator scenario");
    }
    let scenario = scenarios()
        .into_iter()
        .find(|s| s.name() == scenario_name)
        .ok_or_else(|| anyhow::anyhow!("unknown mutator scenario: {scenario_name}"))?;
    // Honour the capability gate here too (the CLI driver bypasses the
    // registry's run-all gating): a scenario whose host capabilities are not
    // met is SKIPPED-with-reason rather than run against an unsuitable host
    // (STRESS_HARNESS s2.5 / s8). Without this, e.g. a Windows-only lock
    // scenario invoked on Linux would assert a behaviour the platform cannot
    // produce.
    let caps = crate::capabilities::CapabilitySet::probe();
    let missing = scenario.requires().missing(&caps);
    if !missing.is_empty() {
        return Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: 0,
            final_hash_matches_local: true,
            notes: vec![
                format!(
                    "SKIP-by-capability: {} requires {} which the host lacks",
                    scenario_name,
                    missing.join(", ")
                ),
                "invariants: None - capability SKIP early-return; no handle/source/remote booted, so no source+folder snapshot to sweep.".to_string(),
            ],
            // Capability SKIP: nothing ran, so there is no terminal
            // source+remote state for the canonical s6.3 sweep to read.
            invariants: None,
        });
    }
    // Each scenario boots its own self-contained handle in run_assertions; the
    // passed handle is a placeholder satisfying the trait signature.
    let state_dir = tempfile::tempdir()?;
    let remote = Arc::new(InMemoryRemoteStore::new());
    let handle = boot_handle(state_dir.path(), remote).await?;
    scenario.run_assertions(&handle).await
}

/// The registered `fuzz-smoke` scenario (STRESS_HARNESS s4.3): a tiny,
/// FIXED-SEED, bounded fuzz run so the property loop is exercised in the
/// ~5-min `chaos-hermetic` CI job. The long soak run goes through the `fuzz`
/// CLI subcommand (`run_fuzz` with a large step budget), kept distinct so the
/// registered scenario can't accidentally turn into a 6-hour CI step.
struct FuzzSmokeScenario {
    seed: u64,
    steps: u64,
}

impl Default for FuzzSmokeScenario {
    fn default() -> Self {
        // A fixed seed keeps the smoke run deterministic (a flake here is a
        // real regression, not RNG noise). The CLI soak path uses now()/$(date).
        Self {
            seed: 0xC0FF_EE00_1234_5678,
            steps: 120,
        }
    }
}

#[async_trait]
impl Scenario for FuzzSmokeScenario {
    fn name(&self) -> &'static str {
        "fuzz-smoke"
    }
    fn description(&self) -> &'static str {
        "s4.3 fuzz: a fixed-seed bounded weighted mutation+sync soak; asserts no data loss / no duplicate objects / no pending_ops leak"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }

    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        // The registered smoke row stays bounded by BOTH a small step budget
        // and the 60s wall cap, so it can never balloon into a long CI step.
        let report = run_fuzz(self.seed, self.steps, SCENARIO_WALL_CAP).await?;
        if let Some(reason) = &report.violation {
            // Persist the failure for bit-reproducible replay, then fail.
            let path = write_fuzz_failure(&report)?;
            anyhow::bail!(
                "fuzz-smoke seed {} violated an invariant after {} steps ({reason}); replay log at {}",
                report.seed,
                report.steps,
                path.display()
            );
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: 0,
            final_hash_matches_local: true,
            notes: vec![
                format!(
                    "fuzz seed {} ran {} steps; all s6.3 invariants held",
                    report.seed, report.steps
                ),
                "invariants: None - this fuzz row carries its s6.3 checks INLINE: run_fuzz boots+drops its own handle/source/remote and asserts assert_invariants per run, surfacing any violation via FuzzReport.violation which bails above. No handle/source/remote is in scope here to sweep.".to_string(),
            ],
            // Aggregate fuzz summary: run_fuzz owns + drops the handle/source/
            // remote internally and asserts the s6.3 invariants inline (a
            // violation sets FuzzReport.violation, which bails above before this
            // Outcome is built). Nothing coherent is in scope here to sweep.
            invariants: None,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        // Fuzz asserts the post-condition invariants, not a specific code.
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Every registered s4 scenario must have a non-empty, uniquely-prefixed
    /// name + description and be constructible (the registry-shape contract).
    #[test]
    fn registry_is_well_formed() {
        let all = scenarios();
        assert!(!all.is_empty(), "the s4 registry is non-empty");
        let mut names = std::collections::HashSet::new();
        for s in &all {
            assert!(!s.name().is_empty(), "scenario name non-empty");
            assert!(
                !s.description().is_empty(),
                "scenario description non-empty"
            );
            assert!(
                names.insert(s.name()),
                "duplicate scenario name: {}",
                s.name()
            );
            assert!(
                s.name().starts_with("mutator-") || s.name().starts_with("fuzz"),
                "name {} must be mutator-/fuzz- prefixed to de-collide with the catalogue",
                s.name()
            );
        }
    }

    /// The xorshift PRNG is deterministic from a seed (bit-reproducibility, the
    /// load-bearing property for fuzz failure replay).
    #[test]
    fn rng_is_deterministic() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        // A different seed diverges.
        let mut c = Rng::new(43);
        let mut d = Rng::new(42);
        assert_ne!(c.next_u64(), d.next_u64());
    }

    /// The weighted picker only ever returns a step in the distribution and
    /// honours the total weight bound (no out-of-range index).
    #[test]
    fn pick_step_in_distribution() {
        let mut rng = Rng::new(7);
        let valid: std::collections::HashSet<FuzzStep> =
            FUZZ_WEIGHTS.iter().map(|(s, _)| *s).collect();
        for _ in 0..10_000 {
            assert!(valid.contains(&pick_step(&mut rng)));
        }
    }

    /// A fixed-seed fuzz run completes within budget and holds every s6.3
    /// invariant (the actual end-to-end smoke of the s4.3 driver against the
    /// real headless core).
    #[tokio::test]
    async fn fuzz_smoke_holds_invariants() {
        // 80 steps complete in well under the 60s wall cap, so the step budget
        // is the binding bound and the full budget runs.
        let report = run_fuzz(0xABCD_1234, 80, SCENARIO_WALL_CAP)
            .await
            .expect("fuzz run");
        assert_eq!(report.steps, 80, "ran the full budget");
        assert!(
            report.violation.is_none(),
            "no invariant violated: {:?}",
            report.violation
        );
    }

    /// A filesystem-mutator scenario races a real mutation thread against sync
    /// cycles and converges with every invariant intact (the s4.1 mechanism).
    #[tokio::test]
    async fn fs_mutator_edit_converges() {
        let scenario = FsMutatorScenario::frequent_edits();
        let state_dir = tempfile::tempdir().expect("state dir");
        let remote = Arc::new(InMemoryRemoteStore::new());
        let handle = boot_handle(state_dir.path(), remote)
            .await
            .expect("boot placeholder handle");
        let outcome = scenario
            .run_assertions(&handle)
            .await
            .expect("fs mutator runs and converges");
        assert!(
            outcome.final_drive_object_count > 0,
            "the stable population landed on the remote"
        );
        assert!(
            outcome.final_hash_matches_local,
            "no data loss: remote bytes match local for synced files"
        );
    }

    /// A Drive-side mutator surfaces the expected stable error code into the
    /// activity log (the s4.2 mechanism + s9 code-based PASS).
    #[tokio::test]
    async fn drive_mutator_quota_surfaces_code() {
        let scenario = DriveMutatorScenario::quota_mid_upload();
        let state_dir = tempfile::tempdir().expect("state dir");
        let remote = Arc::new(InMemoryRemoteStore::new());
        let handle = boot_handle(state_dir.path(), remote)
            .await
            .expect("boot placeholder handle");
        let outcome = scenario
            .run_assertions(&handle)
            .await
            .expect("drive mutator runs");
        assert!(
            outcome
                .error_codes_seen
                .contains(&ErrorCode::DriveQuotaExhausted),
            "drive.quota_exhausted surfaced; saw {:?}",
            outcome.error_codes_seen
        );
    }

    /// The daily-quota scenario now runs hermetically against the fake's
    /// `with_daily_quota_after` injector and surfaces the real
    /// `DriveDailyQuotaExhausted` code (distinct from storage quota). It is no
    /// longer a capability SKIP - the injector exists.
    #[tokio::test]
    async fn drive_daily_quota_surfaces_code() {
        let scenario = DriveMutatorScenario::daily_quota_exhausted();
        // It runs on a stock host - no real-Drive capability required.
        assert!(
            scenario.requires().required.is_empty(),
            "daily-quota row needs no host capability now"
        );
        let state_dir = tempfile::tempdir().expect("state dir");
        let remote = Arc::new(InMemoryRemoteStore::new());
        let handle = boot_handle(state_dir.path(), remote)
            .await
            .expect("boot placeholder handle");
        let outcome = scenario.run_assertions(&handle).await.expect("runs");
        assert!(
            outcome
                .error_codes_seen
                .contains(&ErrorCode::DriveDailyQuotaExhausted),
            "drive.daily_quota_exhausted surfaced; saw {:?}",
            outcome.error_codes_seen
        );
        // The daily code is classified distinctly from storage quota.
        assert!(
            !outcome
                .error_codes_seen
                .contains(&ErrorCode::DriveQuotaExhausted),
            "daily quota is not misclassified as storage quota; saw {:?}",
            outcome.error_codes_seen
        );
    }
}
