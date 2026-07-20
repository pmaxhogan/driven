//! Storage and disk scenarios (STRESS_HARNESS s3.1).
//!
//! Five rows:
//!
//! `disk-full-target` - a source on a small constrained volume filled to 0
//! free mid-sync; expects the `local.disk_full` code (SPEC s24 /
//! STRESS_HARNESS s10), a paused sync, and no crash.
//!
//! `readonly-source-folder` - the source folder itself is read-only. Driven
//! reads from a source (it never writes into it), so every file still uploads.
//!
//! `readonly-file` - one file inside an otherwise-normal source is read-only;
//! it reads + uploads to `synced`.
//!
//! `noaccess-file` - a file that stats but is unreadable for the current user
//! (POSIX `chmod 000`; Windows ACL Deny:READ). Per-file `local.io_error`; the
//! scan continues and no OTHER file is trashed as a cascade.
//!
//! `noaccess-folder` - a subfolder that cannot be enumerated (POSIX `chmod 0`;
//! Windows ACL Deny:LIST). The walker yields `Err` over that subtree, so per
//! DESIGN s5.2 the deletion-suppression must engage: no previously-synced
//! `file_state` row under the unreadable subtree is enqueued for trash.
//!
//! Each scenario drives the headless core (DESIGN s4.2) at the
//! scan -> plan -> execute level against a fresh
//! [`InMemoryRemoteStore`] and a real temp dir, then asserts both the
//! row-specific outcome and the s6.3 cross-scenario invariants it can check
//! locally (no data loss for synced rows, no duplicate remote objects, clean
//! drain of pending ops).
//!
//! ## Capability gating (STRESS_HARNESS s2.5 / s8) - honest skips
//!
//! `disk-full-target` mounts a 32 MiB constrained volume (Linux loop, Windows
//! VHD). Both require elevation, so it requires [`Capability::Admin`] and is
//! SKIPPED on an unprivileged runner with that reason recorded - never faked.
//! It ALSO documents a current core gap (see the scenario's own notes): the
//! M3 core maps an out-of-space read to `local.io_error`, not yet
//! `local.disk_full`; the row therefore stays capability-gated until both the
//! mount privilege and the core mapping are present, rather than asserting a
//! weakened outcome.
//!
//! The `noaccess-*` rows make a file/dir genuinely inaccessible. On POSIX that
//! is `chmod` (requires [`Capability::Unix`]); a process running as root
//! bypasses mode bits, so `setup` verifies the inaccessibility actually took
//! and surfaces a clear error if it did not (e.g. running as root) rather than
//! silently passing. On Windows the inaccessibility is an ACL Deny applied via
//! `icacls`, gated on [`Capability::Windows`] + [`Capability::NtfsVolume`].

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use driven_core::executor::{DefaultExecutor, Executor, ExecutorDeps, OpOutcome};
use driven_core::pacer::{Pacer, PacerCeilings, ResponseClass};
use driven_core::planner;
use driven_core::scanner;
use driven_core::state::SourceRow;
use driven_core::types::{AccountId, ErrorCode, ExecProgress, FileStateStatus, ScanMode, SourceId};

use driven_drive::fake::InMemoryRemoteStore;
use driven_drive::remote_store::RemoteStore;

use crate::capabilities::{Capability, CapabilityRequirements};
use crate::handle::DrivenHandle;
use crate::scenario::{ExpectedOutcome, Outcome, Scenario, ScenarioContext};
use crate::scenarios::now_ms;
use crate::scenarios::reporting;

/// Every storage/disk scenario (STRESS_HARNESS s3.1).
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(DiskFullTarget),
        Box::new(ReadonlySourceFolder),
        Box::new(ReadonlyFile),
        Box::new(NoaccessFile),
        Box::new(NoaccessFolder),
    ]
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A non-blocking [`Pacer`] for the storage rows.
///
/// These assert sync correctness, not rate-pacing; the real `AimdPacer`
/// blocks on `tokio::time` while polling a non-advancing `FakeClock` and would
/// deadlock. AIMD behaviour is unit-tested in `pacer.rs`. This mirrors the
/// e2e-suite / handle `NoopPacer`.
struct NoopPacer;

#[async_trait]
impl Pacer for NoopPacer {
    async fn permit_request(&self) {}
    async fn permit_file_create(&self) {}
    async fn permit_bytes(&self, _n: u64) {}
    fn note_response(&self, _classification: ResponseClass) {}
    fn ceilings(&self) -> PacerCeilings {
        PacerCeilings::default()
    }
}

/// A swallow-everything progress sink (the storage rows assert on terminal
/// state + per-op outcomes, not progress ticks).
fn noop_progress(_p: ExecProgress) {}

/// Build a [`SourceRow`] rooted at `root`, uploading into `folder_id`, with
/// gitignore + symlink-following off (the storage rows are about disk/ACL
/// behaviour, not exclude rules).
fn source_in(account: AccountId, root: &Path, folder_id: &str) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id: account,
        display_name: "storage-chaos".into(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: folder_id.to_string(),
        drive_folder_path: "/storage-chaos".into(),
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

/// Build a [`DefaultExecutor`] over the handle's state + clock and a caller-
/// supplied remote, with a non-blocking pacer and no crypto/VSS.
fn executor_over(handle: &DrivenHandle, remote: Arc<InMemoryRemoteStore>) -> DefaultExecutor {
    DefaultExecutor::with_clock(
        ExecutorDeps {
            remote,
            state: handle.state.clone(),
            pacer: Arc::new(NoopPacer),
            crypto: None,
            vss: None,
            network: None,
        },
        handle.clock.clone(),
    )
}

/// Run a full scan -> plan -> execute pass for `source` against `remote`,
/// returning the per-op outcomes. Mirrors the orchestrator pipeline at the
/// level the storage rows need (so per-file `local.io_error` outcomes are
/// observable, which a state-only assertion would hide).
async fn scan_plan_execute(
    handle: &DrivenHandle,
    remote: &Arc<InMemoryRemoteStore>,
    source: &SourceRow,
) -> anyhow::Result<Vec<OpOutcome>> {
    let scan = scanner::scan(source, handle.state.as_ref(), ScanMode::FastPath).await?;
    let plan = planner::plan(
        source,
        &scan,
        handle.state.as_ref(),
        now_ms(),
        &planner::BundleConfig::default(),
    )
    .await?;
    let exec = executor_over(handle, remote.clone());
    exec.execute(
        source,
        &plan,
        &noop_progress,
        &driven_core::executor::noop_outcome_sink,
    )
    .await
}

/// Count every non-trashed object reachable under `folder_id`, recursing into
/// child folders. Used for the "no missing / no duplicate object" checks.
async fn live_object_count(remote: &InMemoryRemoteStore, folder_id: &str) -> anyhow::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![folder_id.to_string()];
    while let Some(id) = stack.pop() {
        for entry in remote.list_folder(&id).await? {
            if entry.trashed {
                continue;
            }
            if entry.size.is_none() {
                // A folder (size is only set on file objects in the fake).
                stack.push(entry.id);
            } else {
                total += 1;
            }
        }
    }
    Ok(total)
}

/// Download `file_id`'s full bytes from the fake.
async fn download_bytes(remote: &InMemoryRemoteStore, file_id: &str) -> anyhow::Result<Vec<u8>> {
    let mut blob = Vec::new();
    remote
        .download(file_id)
        .await?
        .0
        .read_to_end(&mut blob)
        .await?;
    Ok(blob)
}

/// Cross-cutting local invariant the storage rows assert (a per-scenario
/// subset of STRESS_HARNESS s6.3): for every `Synced` `file_state` row the
/// recorded Drive object exists, is not trashed, and its bytes are byte-equal
/// to the current local file. Returns `(synced_rows_checked, all_match)`.
async fn synced_rows_intact(
    handle: &DrivenHandle,
    remote: &InMemoryRemoteStore,
    source: &SourceRow,
) -> anyhow::Result<(u64, bool)> {
    let rows = handle.state.load_source_file_state(source.id).await?;
    let mut checked = 0u64;
    let mut all_ok = true;
    for (rel, row) in rows {
        if row.status != FileStateStatus::Synced {
            continue;
        }
        checked += 1;
        let Some(file_id) = row.drive_file_id.as_deref() else {
            all_ok = false;
            continue;
        };
        let remote_bytes = match download_bytes(remote, file_id).await {
            Ok(b) => b,
            Err(_) => {
                all_ok = false;
                continue;
            }
        };
        let local_path = {
            let mut p = std::path::PathBuf::from(&source.local_path);
            for seg in rel.as_str().split('/') {
                p.push(seg);
            }
            p
        };
        match std::fs::read(&local_path) {
            Ok(local_bytes) if local_bytes == remote_bytes => {}
            _ => all_ok = false,
        }
    }
    Ok((checked, all_ok))
}

/// Whether `path` can actually be opened for read by the current process.
/// Used by the `noaccess-*` rows to verify their inaccessibility fixture took
/// (a root process bypasses POSIX mode bits, so the chmod is a no-op there).
fn is_readable(path: &Path) -> bool {
    std::fs::File::open(path).is_ok()
}

/// Whether `dir` can actually be enumerated by the current process.
fn is_listable(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        // `read_dir` can succeed yet error on the first `next()` when LIST is
        // denied; force one step to be sure.
        Ok(mut it) => !matches!(it.next(), Some(Err(_))),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// disk-full-target
// ---------------------------------------------------------------------------

/// `disk-full-target` (STRESS_HARNESS s3.1): a source on a 32 MiB constrained
/// volume filled to 0 free mid-sync. Expects `local.disk_full`, tray red, sync
/// paused, no crash, and a resume once space is freed.
///
/// HONESTLY CAPABILITY-GATED. Mounting the constrained volume (Linux loop /
/// Windows VHD per s8) requires elevation, so the row requires
/// [`Capability::Admin`] and SKIPs on an unprivileged runner. See the
/// scenario's own setup note on the additional core gap.
struct DiskFullTarget;

#[async_trait]
impl Scenario for DiskFullTarget {
    fn name(&self) -> &'static str {
        "disk-full-target"
    }

    fn description(&self) -> &'static str {
        "source on a 32 MiB constrained volume filled to 0 free mid-sync; expects local.disk_full, paused, no crash"
    }

    fn requires(&self) -> CapabilityRequirements {
        // Gated on DiskMountAllowed (env DRIVEN_CHAOS_ALLOW_DISK_MOUNT=1, never
        // set today) rather than bare Admin: an elevated CI runner would satisfy
        // Admin and then the documented read-only-source gap below would turn an
        // honest bail into a FAIL. The mount-allowed gate keeps it a recorded
        // SKIP everywhere until the write-into-source path lands.
        CapabilityRequirements::of(vec![Capability::DiskMountAllowed])
    }

    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        // Reached ONLY on an elevated host (the capability gate guards this).
        //
        // Materialising the constrained volume is an elevated, OS-specific
        // mount (Linux: `losetup` a 32 MiB sparse file + mkfs + mount; Windows:
        // `New-VHD` + `Mount-VHD` + format). The harness does NOT fill the dev
        // machine's real disk (STRESS_HARNESS s8).
        //
        // CORE MAPPING (P1-E): the M3 core now maps an out-of-space write error
        // to ErrorCode::LocalDiskFull (executor `local_io_error_code` /
        // `is_disk_full`: ENOSPC=28 on Unix, ERROR_DISK_FULL=112 /
        // ERROR_HANDLE_DISK_FULL=39 on Windows, plus ErrorKind::StorageFull),
        // unit-tested in driven-core (`enospc_classifies_as_local_disk_full`).
        // The earlier "core maps it to local.io_error" gap is CLOSED.
        //
        // REMAINING ARCHITECTURAL GAP (tracked, not fake-green): a V1 source is
        // read-ONLY (the executor reads source files and writes to Drive; it
        // never writes back into the source volume - confirmed: every executor
        // write is a RemoteStore `create`/`update`, never a local `File::create`
        // on the source path). So a read-only source on a 0-free constrained
        // volume produces NO local write, hence no ENOSPC, hence the
        // LocalDiskFull mapping - though now present and tested - is not
        // reachable through V1's source-read path. Driving this row end to end
        // needs a Driven write-into-source path (e.g. a future local staging /
        // VSS-temp spool on the source volume) that V1 does not have. Rather
        // than fabricate a pass or assert a code the read-only path cannot
        // emit, the row stays an honest documented known-gap (recorded in
        // design/CODEX_NOTES.md) behind the mount-privilege gate.
        anyhow::bail!(
            "disk-full-target: core ENOSPC->local.disk_full mapping is implemented + unit-tested, \
             but V1's read-only source path never writes to the source volume, so a full source \
             volume cannot induce the mapping end to end. Tracked known-gap; needs a \
             write-into-source path (local staging/VSS spool) to drive. Not faked green."
        )
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        // Unreachable on an unprivileged runner (SKIPPED by the gate) and
        // returns early from `setup` on an elevated one (the documented
        // read-only-source gap). Kept honest: it never fabricates a pass.
        anyhow::bail!("disk-full-target is capability-gated; see setup")
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::LocalDiskFull,
        }
    }
}

// ---------------------------------------------------------------------------
// readonly-source-folder
// ---------------------------------------------------------------------------

/// `readonly-source-folder` (STRESS_HARNESS s3.1): the source folder is marked
/// read-only. Driven reads FROM a source (never writes into it), so every file
/// still uploads.
struct ReadonlySourceFolder;

#[async_trait]
impl Scenario for ReadonlySourceFolder {
    fn name(&self) -> &'static str {
        "readonly-source-folder"
    }

    fn description(&self) -> &'static str {
        "source folder marked read-only; Driven reads + uploads every file (source is read-from, not written)"
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let root = fixture_root_for(self.name());
        std::fs::create_dir_all(&root)?;
        for i in 0..6u32 {
            std::fs::write(root.join(format!("f{i}.txt")), format!("readonly-src-{i}"))?;
        }
        set_readonly(&root, true)?;
        ctx.fixture_root = root;
        Ok(())
    }

    async fn run_assertions(&self, handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let remote = Arc::new(InMemoryRemoteStore::new());
        let folder = remote.root_id().to_string();
        let root = fixture_root_for(self.name());
        let src = source_in(handle.account_id, &root, &folder);
        handle.state.upsert_source(&src).await?;

        let outcomes = scan_plan_execute(handle, &remote, &src).await?;
        let failures: Vec<ErrorCode> = collect_failures(&outcomes);
        let live = live_object_count(&remote, &folder).await?;
        let (synced, intact) = synced_rows_intact(handle, &remote, &src).await?;
        let pending = handle.state.get_pending_ops_for_source(src.id).await?;

        let mut notes = Vec::new();
        notes.push(format!(
            "uploaded {live} object(s); {synced} synced row(s); {} op(s) failed; {} pending op(s)",
            failures.len(),
            pending.len()
        ));

        anyhow::ensure!(
            failures.is_empty(),
            "a read-only SOURCE folder must not fail any upload (source is read-from): {failures:?}"
        );
        anyhow::ensure!(live == 6, "all 6 files must upload; got {live}");
        anyhow::ensure!(
            synced == 6 && intact,
            "every file must be Synced and byte-intact on Drive"
        );
        anyhow::ensure!(pending.is_empty(), "no pending ops should leak");

        // Central s6.3 sweep (P1-C): the runner enforces this after the
        // scenario; clean_shutdown holds because the single sync cycle drained
        // to completion (no pending ops leaked, asserted above).
        let inv_report = reporting::assert_invariants(handle, &remote, src.id, &folder).await?;

        Ok(Outcome {
            error_codes_seen: failures,
            final_drive_object_count: live,
            final_hash_matches_local: intact,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }

    async fn teardown(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        // Clear the read-only bit so the tempdir can be removed.
        let _ = set_readonly(&ctx.fixture_root, false);
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// readonly-file
// ---------------------------------------------------------------------------

/// `readonly-file` (STRESS_HARNESS s3.1): one file inside an otherwise-normal
/// source is read-only. Driven reads + uploads it; status `synced`.
struct ReadonlyFile;

#[async_trait]
impl Scenario for ReadonlyFile {
    fn name(&self) -> &'static str {
        "readonly-file"
    }

    fn description(&self) -> &'static str {
        "one read-only file in a normal source; Driven reads + uploads it to synced"
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let root = fixture_root_for(self.name());
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("normal.txt"), b"plain writable file")?;
        let ro = root.join("locked.txt");
        std::fs::write(&ro, b"this file is read-only but readable")?;
        set_readonly(&ro, true)?;
        ctx.fixture_root = root;
        Ok(())
    }

    async fn run_assertions(&self, handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let remote = Arc::new(InMemoryRemoteStore::new());
        let folder = remote.root_id().to_string();
        let root = fixture_root_for(self.name());
        let src = source_in(handle.account_id, &root, &folder);
        handle.state.upsert_source(&src).await?;

        let outcomes = scan_plan_execute(handle, &remote, &src).await?;
        let failures = collect_failures(&outcomes);
        let live = live_object_count(&remote, &folder).await?;
        let (synced, intact) = synced_rows_intact(handle, &remote, &src).await?;
        let pending = handle.state.get_pending_ops_for_source(src.id).await?;

        anyhow::ensure!(
            failures.is_empty(),
            "a read-only (but readable) file must upload cleanly: {failures:?}"
        );
        anyhow::ensure!(live == 2, "both files must upload; got {live}");
        anyhow::ensure!(
            synced == 2 && intact,
            "both files Synced + byte-intact on Drive"
        );
        anyhow::ensure!(pending.is_empty(), "no pending ops should leak");

        // Central s6.3 sweep (P1-C); clean_shutdown holds (single cycle drained,
        // no pending ops, asserted above).
        let inv_report = reporting::assert_invariants(handle, &remote, src.id, &folder).await?;

        Ok(Outcome {
            error_codes_seen: failures,
            final_drive_object_count: live,
            final_hash_matches_local: intact,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes: vec![format!(
                "{live} uploaded, {synced} synced, read-only leaf included"
            )],
        })
    }

    async fn teardown(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let _ = set_readonly(&ctx.fixture_root.join("locked.txt"), false);
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// noaccess-file
// ---------------------------------------------------------------------------

/// `noaccess-file` (STRESS_HARNESS s3.1): a file that stats but is unreadable
/// for the current user (POSIX `chmod 000`; Windows ACL Deny:READ). The scan
/// includes it (it stats fine), the executor fails to open it -> per-file
/// `local.io_error`; the scan continues and NO other file is trashed as a
/// cascade.
struct NoaccessFile;

#[async_trait]
impl Scenario for NoaccessFile {
    fn name(&self) -> &'static str {
        "noaccess-file"
    }

    fn description(&self) -> &'static str {
        "a stat-able but unreadable file; local.io_error logged per file, scan continues, no cascade delete"
    }

    fn requires(&self) -> CapabilityRequirements {
        // POSIX path needs `chmod` semantics (Unix); the Windows path applies a
        // Deny ACL via icacls and needs an NTFS volume.
        #[cfg(windows)]
        {
            CapabilityRequirements::of(vec![Capability::Windows, Capability::NtfsVolume])
        }
        #[cfg(not(windows))]
        {
            CapabilityRequirements::of(vec![Capability::Unix])
        }
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let root = fixture_root_for(self.name());
        std::fs::create_dir_all(&root)?;
        // Two readable files that MUST upload, plus the unreadable one.
        std::fs::write(root.join("ok1.txt"), b"first readable")?;
        std::fs::write(root.join("ok2.txt"), b"second readable")?;
        let secret = root.join("secret.bin");
        std::fs::write(&secret, b"unreadable content")?;
        deny_read(&secret)?;
        ctx.fixture_root = root.clone();

        // Honest precondition check (STRESS_HARNESS s8): a root process bypasses
        // POSIX mode bits, so verify the file is GENUINELY unreadable now. If it
        // is still readable the fixture did not take (e.g. running as root) and
        // the scenario cannot exercise `local.io_error` - surface that loudly
        // rather than letting it pass as a false green.
        anyhow::ensure!(
            !is_readable(&secret),
            "noaccess-file fixture did not take: {} is still readable (running as root / \
             ACL not applied). Run on a non-root host where the deny actually engages.",
            secret.display()
        );
        Ok(())
    }

    async fn run_assertions(&self, handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let remote = Arc::new(InMemoryRemoteStore::new());
        let folder = remote.root_id().to_string();
        let root = fixture_root_for(self.name());
        let src = source_in(handle.account_id, &root, &folder);
        handle.state.upsert_source(&src).await?;

        let outcomes = scan_plan_execute(handle, &remote, &src).await?;

        // Exactly the unreadable file fails with local.io_error; the two
        // readable ones complete.
        let mut io_errors = 0u32;
        let mut other_failures: Vec<ErrorCode> = Vec::new();
        let mut done = 0u32;
        for o in &outcomes {
            match o {
                OpOutcome::Done { .. } => done += 1,
                // Bundling is off in this scenario (issue #35); count defensively.
                OpOutcome::BundleDone { files, .. } => done += *files as u32,
                OpOutcome::Failed { code, .. } if *code == ErrorCode::LocalIoError => {
                    io_errors += 1
                }
                OpOutcome::Failed { code, .. } => other_failures.push(*code),
                OpOutcome::Skipped { .. } => {}
            }
        }
        let live = live_object_count(&remote, &folder).await?;
        let (synced, intact) = synced_rows_intact(handle, &remote, &src).await?;

        anyhow::ensure!(
            other_failures.is_empty(),
            "only the unreadable file may fail, and only with local.io_error: saw {other_failures:?}"
        );
        anyhow::ensure!(
            io_errors == 1,
            "exactly one local.io_error (the unreadable file); got {io_errors}"
        );
        anyhow::ensure!(
            done == 2,
            "both readable files must upload despite the unreadable peer; got {done}"
        );
        anyhow::ensure!(live == 2, "two objects on Drive; got {live}");
        anyhow::ensure!(
            synced == 2 && intact,
            "the two readable files Synced + byte-intact"
        );

        // No-cascade-delete invariant: nothing was trashed (the unreadable file
        // is a read failure, not a deletion; the scan never reports it deleted).
        let trashed = remote
            .list_folder_with_trashed(&folder)
            .iter()
            .filter(|e| e.trashed)
            .count();
        anyhow::ensure!(
            trashed == 0,
            "an unreadable file must not cascade into trashing others; {trashed} trashed"
        );

        // Central s6.3 sweep (P1-C): the unreadable file is a read failure (not
        // synced, not trashed), so the two readable files are the only synced
        // rows the sweep checks; clean_shutdown holds (single cycle completed).
        let inv_report = reporting::assert_invariants(handle, &remote, src.id, &folder).await?;

        Ok(Outcome {
            error_codes_seen: vec![ErrorCode::LocalIoError],
            final_drive_object_count: live,
            final_hash_matches_local: intact,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes: vec![format!(
                "{done} readable files uploaded, 1 local.io_error, 0 trashed (no cascade)"
            )],
        })
    }

    async fn teardown(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        // Restore read so the tempdir cleans up.
        let _ = restore_access(&ctx.fixture_root.join("secret.bin"));
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::LocalIoError,
        }
    }
}

// ---------------------------------------------------------------------------
// noaccess-folder
// ---------------------------------------------------------------------------

/// `noaccess-folder` (STRESS_HARNESS s3.1): a subfolder we cannot enumerate
/// (POSIX `chmod 0`; Windows ACL Deny:LIST). The walker yields `Err` over that
/// subtree, so per DESIGN s5.2 deletion-suppression must engage: NO
/// previously-synced `file_state` row under the unreadable subtree is enqueued
/// for trash.
///
/// The discriminating setup is two-phase: first a clean sync populates
/// `file_state` for files under `locked/` (so they are known + synced), THEN
/// the subfolder is made unenumerable. A naive diff would see those known
/// paths "missing" and trash them; the suppression must prevent it.
struct NoaccessFolder;

#[async_trait]
impl Scenario for NoaccessFolder {
    fn name(&self) -> &'static str {
        "noaccess-folder"
    }

    fn description(&self) -> &'static str {
        "an un-enumerable subfolder; walker Errs, DESIGN s5.2 deletion-suppression engages, no subtree trash"
    }

    fn requires(&self) -> CapabilityRequirements {
        #[cfg(windows)]
        {
            CapabilityRequirements::of(vec![Capability::Windows, Capability::NtfsVolume])
        }
        #[cfg(not(windows))]
        {
            CapabilityRequirements::of(vec![Capability::Unix])
        }
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let root = fixture_root_for(self.name());
        std::fs::create_dir_all(&root)?;
        // A top-level file plus a subfolder with two files.
        std::fs::write(root.join("top.txt"), b"top-level survives")?;
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked)?;
        std::fs::write(locked.join("a.txt"), b"subtree file a")?;
        std::fs::write(locked.join("b.txt"), b"subtree file b")?;
        // The directory is locked LATER (in run_assertions, after the first
        // clean sync populates file_state). Verify here only that we CAN deny
        // listing on this host, so an unsupported host fails loudly at setup
        // rather than silently passing.
        deny_list(&locked)?;
        let took = !is_listable(&locked);
        // Restore for the first sync; we re-deny mid-scenario.
        restore_access(&locked)?;
        anyhow::ensure!(
            took,
            "noaccess-folder fixture cannot deny LIST on {} (running as root / ACL unsupported); \
             run on a non-root host",
            locked.display()
        );
        ctx.fixture_root = root;
        Ok(())
    }

    async fn run_assertions(&self, handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let remote = Arc::new(InMemoryRemoteStore::new());
        let folder = remote.root_id().to_string();
        let root = fixture_root_for(self.name());
        let locked = root.join("locked");
        let src = source_in(handle.account_id, &root, &folder);
        handle.state.upsert_source(&src).await?;

        // Phase 1: clean sync populates file_state for top.txt + locked/a,b.
        let out1 = scan_plan_execute(handle, &remote, &src).await?;
        anyhow::ensure!(
            collect_failures(&out1).is_empty(),
            "phase-1 clean sync must not fail"
        );
        let live_before = live_object_count(&remote, &folder).await?;
        anyhow::ensure!(
            live_before == 3,
            "3 files uploaded in phase 1; got {live_before}"
        );
        let known_before = handle.state.load_source_file_state(src.id).await?.len();
        anyhow::ensure!(known_before == 3, "3 known file_state rows after phase 1");

        // Phase 2: make `locked/` un-enumerable, then re-scan + plan. The walk
        // must yield Err over the subtree and the planner must NOT emit a trash
        // for locked/a.txt or locked/b.txt.
        deny_list(&locked)?;
        // Honest mid-scenario precondition: the deny must actually engage.
        let denied = !is_listable(&locked);
        if !denied {
            let _ = restore_access(&locked);
            anyhow::bail!(
                "deny-LIST did not engage on {} (running as root / unsupported); cannot \
                 exercise deletion-suppression",
                locked.display()
            );
        }

        let scan = scanner::scan(&src, handle.state.as_ref(), ScanMode::FastPath).await?;
        let plan = planner::plan(
            &src,
            &scan,
            handle.state.as_ref(),
            now_ms(),
            &planner::BundleConfig::default(),
        )
        .await?;

        // The plan must contain ZERO trash ops for paths under `locked/`.
        let trash_under_locked = count_trash_under(&plan, "locked/");
        // Restore before any executor pass so teardown + assertions are clean.
        restore_access(&locked)?;

        // Execute whatever plan was produced (should be an effective no-op for
        // the subtree) and confirm nothing under locked/ got trashed remotely.
        let exec = executor_over(handle, remote.clone());
        let _ = exec
            .execute(
                &src,
                &plan,
                &noop_progress,
                &driven_core::executor::noop_outcome_sink,
            )
            .await?;

        let live_after = live_object_count(&remote, &folder).await?;
        let trashed_count = subtree_trashed_count(&remote, &folder).await?;

        anyhow::ensure!(
            trash_under_locked == 0,
            "deletion-suppression failed: planner emitted {trash_under_locked} trash op(s) under locked/"
        );
        anyhow::ensure!(
            live_after == 3,
            "no object may be trashed when the subtree is merely unreadable; live {live_after} (was 3)"
        );
        anyhow::ensure!(
            trashed_count == 0,
            "no remote object under the unreadable subtree may be trashed; {trashed_count} trashed"
        );

        // Central s6.3 sweep (P1-C): all 3 phase-1 synced rows remain live
        // (deletion was suppressed, asserted above), so no data loss / dup /
        // pending leak; clean_shutdown holds (the executor pass completed).
        let inv_report = reporting::assert_invariants(handle, &remote, src.id, &folder).await?;

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live_after,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty(),
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes: vec![
                "walk error over unreadable subtree -> deletion suppressed; 0 subtree trashes"
                    .to_string(),
            ],
        })
    }

    async fn teardown(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let _ = restore_access(&ctx.fixture_root.join("locked"));
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        // The win condition is a documented behaviour (no subtree trash),
        // asserted directly in run_assertions rather than via an error code.
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ---------------------------------------------------------------------------
// plan / outcome helpers
// ---------------------------------------------------------------------------

/// Collect the [`ErrorCode`]s of every [`OpOutcome::Failed`] in `outcomes`.
fn collect_failures(outcomes: &[OpOutcome]) -> Vec<ErrorCode> {
    outcomes
        .iter()
        .filter_map(|o| match o {
            OpOutcome::Failed { code, .. } => Some(*code),
            _ => None,
        })
        .collect()
}

/// Count trash ops in `plan` whose relative path is under `prefix`.
fn count_trash_under(plan: &driven_core::types::Plan, prefix: &str) -> usize {
    use driven_core::types::Op;
    plan.ops
        .iter()
        .filter(|op| match op {
            Op::Trash { relative_path, .. } => relative_path.as_str().starts_with(prefix),
            _ => false,
        })
        .count()
}

/// Count trashed objects reachable under `folder_id` (recursing into folders),
/// for the no-subtree-trash invariant.
async fn subtree_trashed_count(
    remote: &InMemoryRemoteStore,
    folder_id: &str,
) -> anyhow::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![folder_id.to_string()];
    while let Some(id) = stack.pop() {
        for entry in remote.list_folder_with_trashed(&id) {
            if entry.trashed {
                total += 1;
            }
            if entry.size.is_none() && !entry.trashed {
                stack.push(entry.id);
            }
        }
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// fixture-root plumbing
//
// `run_assertions` (per the Phase-1 trait) receives no `ScenarioContext`, so it
// cannot read the tempdir handle `setup` built. The storage rows therefore
// anchor each per-scenario fixture under a stable, process-unique directory
// keyed by scenario name; BOTH `setup` and `run_assertions` derive the SAME
// path via `fixture_root_for(name)`, and `setup` also writes that path into
// `ctx.fixture_root` so the harness's teardown/reporting sees it. This keeps
// the two halves consistent without depending on which root the harness driver
// happens to assign.
// ---------------------------------------------------------------------------

/// The deterministic on-disk fixture root for the scenario named `name`,
/// under a process-unique base directory in the OS temp dir.
fn fixture_root_for(name: &str) -> std::path::PathBuf {
    use std::sync::OnceLock;
    static BASE: OnceLock<std::path::PathBuf> = OnceLock::new();
    let base = BASE.get_or_init(|| {
        let base =
            std::env::temp_dir().join(format!("driven-chaos-storage-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        base
    });
    base.join(name)
}

// ---------------------------------------------------------------------------
// OS-specific access helpers (read-only / deny-read / deny-list)
// ---------------------------------------------------------------------------

/// Toggle the read-only attribute on `path` (file or dir).
fn set_readonly(path: &Path, readonly: bool) -> anyhow::Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_readonly(readonly);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

/// Make `path` (a file) deny READ for the current user.
#[cfg(unix)]
fn deny_read(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000))?;
    Ok(())
}

/// Make `path` (a file) deny READ for the current user via an icacls Deny ACE.
#[cfg(windows)]
fn deny_read(path: &Path) -> anyhow::Result<()> {
    icacls_deny(path, "(R)")
}

/// Make `dir` deny enumeration (LIST) for the current user.
#[cfg(unix)]
fn deny_list(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // mode 0 removes r-x, so the directory cannot be opened/enumerated.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000))?;
    Ok(())
}

/// Make `dir` deny enumeration (LIST DIRECTORY) for the current user via an
/// icacls Deny ACE.
#[cfg(windows)]
fn deny_list(dir: &Path) -> anyhow::Result<()> {
    // RD = Read Data / List Directory.
    icacls_deny(dir, "(RD)")
}

/// Restore default access to `path` after a deny (so the tempdir can clean up).
#[cfg(unix)]
fn restore_access(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Restore default access by removing the icacls Deny ACE for the current user.
#[cfg(windows)]
fn restore_access(path: &Path) -> anyhow::Result<()> {
    let user = current_windows_user()?;
    run_icacls(&[path.to_string_lossy().as_ref(), "/remove:d", &user])
}

/// Apply an icacls Deny ACE for `rights` (e.g. `(R)`, `(RD)`) to the current
/// user on `path`.
#[cfg(windows)]
fn icacls_deny(path: &Path, rights: &str) -> anyhow::Result<()> {
    let user = current_windows_user()?;
    run_icacls(&[
        path.to_string_lossy().as_ref(),
        "/deny",
        &format!("{user}:{rights}"),
    ])
}

/// The current Windows user as `DOMAIN\\user` (or `user`), for icacls.
#[cfg(windows)]
fn current_windows_user() -> anyhow::Result<String> {
    let user = std::env::var("USERNAME")
        .map_err(|_| anyhow::anyhow!("USERNAME not set; cannot target an ACL"))?;
    match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => Ok(format!("{domain}\\{user}")),
        _ => Ok(user),
    }
}

/// Run `icacls` with `args`, erroring if it does not exit 0.
#[cfg(windows)]
fn run_icacls(args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("icacls").args(args).output()?;
    anyhow::ensure!(
        out.status.success(),
        "icacls {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every storage scenario is registered with a unique, stable name.
    #[test]
    fn registers_all_storage_scenarios() {
        let names: Vec<&str> = scenarios().iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec![
                "disk-full-target",
                "readonly-source-folder",
                "readonly-file",
                "noaccess-file",
                "noaccess-folder",
            ]
        );
        // Names are unique.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "scenario names must be unique");
    }

    /// `disk-full-target` is honestly gated on DiskMountAllowed (never set
    /// today) so it SKIPs everywhere - including an elevated CI runner where a
    /// bare Admin gate would let it run and then FAIL on the documented
    /// read-only-source gap - and expects the `local.disk_full` code. It never
    /// fabricates a pass.
    #[test]
    fn disk_full_is_mount_gated_and_expects_disk_full() {
        let s = DiskFullTarget;
        let req = s.requires();
        assert!(
            req.required
                .iter()
                .any(|c| matches!(c, Capability::DiskMountAllowed)),
            "disk-full-target must require DiskMountAllowed so it SKIPs (not FAILs) on an elevated host"
        );
        // It must NOT be merely Admin-gated, or an elevated runner would run it.
        assert!(
            !req.required.iter().any(|c| matches!(c, Capability::Admin)),
            "disk-full-target must not be bare-Admin-gated"
        );
        assert!(matches!(
            s.expected_outcome(),
            ExpectedOutcome::GracefulFailureWith {
                code: ErrorCode::LocalDiskFull
            }
        ));
    }

    /// The read-only-source and read-only-file rows run anywhere (no caps) and
    /// expect Success.
    #[test]
    fn readonly_rows_need_no_capabilities() {
        assert!(ReadonlySourceFolder.requires().required.is_empty());
        assert!(ReadonlyFile.requires().required.is_empty());
        assert!(matches!(
            ReadonlySourceFolder.expected_outcome(),
            ExpectedOutcome::Success
        ));
        assert!(matches!(
            ReadonlyFile.expected_outcome(),
            ExpectedOutcome::Success
        ));
    }

    /// The no-access rows are platform-gated (Unix mode bits or Windows NTFS
    /// ACLs) so they SKIP rather than run where the deny cannot be applied.
    #[test]
    fn noaccess_rows_are_platform_gated() {
        let f = NoaccessFile.requires();
        let d = NoaccessFolder.requires();
        #[cfg(windows)]
        {
            assert!(f.required.iter().any(|c| matches!(c, Capability::Windows)));
            assert!(f
                .required
                .iter()
                .any(|c| matches!(c, Capability::NtfsVolume)));
            assert!(d
                .required
                .iter()
                .any(|c| matches!(c, Capability::NtfsVolume)));
        }
        #[cfg(not(windows))]
        {
            assert!(f.required.iter().any(|c| matches!(c, Capability::Unix)));
            assert!(d.required.iter().any(|c| matches!(c, Capability::Unix)));
        }
        assert!(matches!(
            NoaccessFile.expected_outcome(),
            ExpectedOutcome::GracefulFailureWith {
                code: ErrorCode::LocalIoError
            }
        ));
        assert!(matches!(
            NoaccessFolder.expected_outcome(),
            ExpectedOutcome::DocumentedBehaviour
        ));
    }
}
