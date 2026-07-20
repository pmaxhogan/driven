//! NTFS / Win32 hazard scenarios (STRESS_HARNESS s3.5).
//!
//! One [`Scenario`] per hazard from the s3.5 catalogue: hardlinks,
//! symlinks, junctions, non-symlink reparse points, OneDrive placeholders,
//! recursive junction cycles, cross-volume links, alternate data streams,
//! sparse / compressed / EFS-encrypted files, hidden+system attributes,
//! and file-id reuse after defrag.
//!
//! Each scenario builds its on-disk fixture in `setup`, then in
//! `run_assertions` boots a headless core over a fresh
//! [`InMemoryRemoteStore`], drives one scan -> plan -> execute cycle, and
//! asserts the s6.3 cross-scenario invariants (no data loss, no duplicate
//! remote objects, clean completion) plus the row's own expected outcome.
//!
//! ## How these scenarios drive the core (matches the sibling convention)
//!
//! The Phase-1 [`DrivenHandle`] exposes its remote only as
//! `Arc<dyn RemoteStore>`, and the `RemoteStore` trait carries no
//! `root_id()` accessor - so a scenario cannot recover the fake's root
//! folder id (a per-instance UUID) through the booted handle. Each
//! scenario therefore constructs its OWN concrete [`InMemoryRemoteStore`],
//! captures `root_id()` for the source folder + the post-run object-count
//! assertions, and boots a dedicated [`DrivenHandle`] over that remote
//! inside `run_assertions` (mirroring the `driven-core` `e2e_fake` pattern
//! and the `permissions` / `concurrency` sibling categories). The `handle`
//! argument the trait passes is the harness default; the NTFS rows do not
//! use it because they need the concrete remote. Surfaced in the M3.7
//! report.
//!
//! ## Honest outcome calibration against the V1 core
//!
//! The s3.5 `Expected outcome` column is the design target. Several rows
//! describe behaviour the V1 core (DESIGN s5.2 / s5.2.1, `scanner.rs`)
//! does not yet fully implement, so those rows assert the CURRENT behaviour
//! via [`ExpectedOutcome::DocumentedBehaviour`] and record the gap in
//! [`Outcome::notes`] - never a faked success against an unimplemented
//! path:
//!
//! `local.ads_skipped` (s10): the scanner now enumerates named NTFS data
//! streams (FindFirstStreamW) and the orchestrator writes a durable
//! `local.ads_skipped` WARNING activity row per affected file (P1-D). The
//! `ads-alternate-data-stream` row asserts the main stream backs up AND that
//! the named-stream skip is SURFACED as that warning - so the loss is visible
//! rather than silent data loss. The named streams themselves are still not
//! uploaded (a documented V1 limitation); surfacing the skip is what makes
//! that honest.
//!
//! The follow-symlinks ON sub-cases (`symlink-to-directory`,
//! `cross-volume-symlink`) need a per-source follow toggle V1 does not
//! ship: [`driven_core::types::SymlinkPolicy`] has only `Skip`. Those rows
//! assert the V1 skip behaviour and document the follow path as V2.
//!
//! ## Capability gating (s2.5 / s8)
//!
//! Every row's [`Scenario::requires`] mirrors the s3.5 `Requires` column.
//! The runner checks `requires().missing(set)` and SKIPs with the missing
//! list BEFORE `setup`/`run_assertions` run, so the bodies below may assume
//! their declared capabilities hold. On a non-elevated host (the default
//! `CapabilitySet::probe()` reports `admin=false`, `ntfs_volume=None`) the
//! admin / NTFS rows SKIP honestly - exercised on the maintainer's
//! elevated Windows box / the `chaos-windows-admin` self-hosted runner
//! (s7), not faked green here.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use driven_core::orchestrator::TickSource;
use driven_core::state::{ActivityFilter, ActivityLevel, PageRequest, SourceRow, StateRepo};
use driven_core::types::{AccountId, ErrorCode, FileStateStatus, RelativePath, SourceId};

use driven_drive::fake::InMemoryRemoteStore;

use crate::capabilities::{Capability, CapabilityRequirements};
use crate::handle::{DrivenHandle, DrivenHandleBuilder};
use crate::scenario::{ExpectedOutcome, Outcome, Scenario, ScenarioContext};

/// Every NTFS / Win32 hazard scenario (STRESS_HARNESS s3.5).
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(HardlinkTwoPaths::new()),
        Box::new(SymlinkToFile::new()),
        Box::new(SymlinkToDirectory::new()),
        Box::new(JunctionMklinkJ::new()),
        Box::new(ReparsePointNonSymlink::new()),
        Box::new(OnedrivePlaceholder::new()),
        Box::new(RecursiveJunctionCycle::new()),
        Box::new(CrossVolumeSymlink::new()),
        Box::new(CrossVolumeLinkStaleAfterReassign::new()),
        Box::new(AdsAlternateDataStream::new()),
        Box::new(SparseFile::new()),
        Box::new(CompressedNtfsFile::new()),
        Box::new(EncryptedNtfsEfs::new()),
        Box::new(HiddenSystemAttributes::new()),
        Box::new(FileIdReuseAfterDefrag::new()),
    ]
}

// ---------------------------------------------------------------------------
// Shared per-scenario fixture + driving helpers
// ---------------------------------------------------------------------------

/// Per-scenario fixture handles kept alive between `setup` and
/// `run_assertions` / `teardown`.
///
/// The on-disk source tree and the hermetic state-DB dir both live in
/// throwaway [`tempfile::TempDir`]s; dropping them removes them. They sit
/// behind a [`Mutex`] so the otherwise-`&self` trait methods can populate /
/// read / clear them.
#[derive(Default)]
struct FixtureState {
    /// The materialised source tree.
    src_dir: Option<tempfile::TempDir>,
    /// The hermetic state-DB directory.
    state_dir: Option<tempfile::TempDir>,
}

/// Boot a [`DrivenHandle`] over `state_db_path` plus the given concrete
/// remote, register `src_root` as a source uploading into the fake's root
/// folder, and return the handle alongside the source id and root folder
/// id.
async fn boot_with_source(
    state_db_path: PathBuf,
    remote: Arc<InMemoryRemoteStore>,
    src_root: &Path,
) -> anyhow::Result<(DrivenHandle, SourceId, String)> {
    let folder = remote.root_id().to_string();
    let handle = DrivenHandleBuilder::new(state_db_path)
        .remote(remote)
        .boot()
        .await?;

    let source = make_source(handle.account_id, src_root, &folder);
    let source_id = source.id;
    handle.state.upsert_source(&source).await?;
    Ok((handle, source_id, folder))
}

/// A source rooted at `root`, uploading into the fake remote's root folder,
/// every optional knob defaulted "off" so the scan is the plain FastPath
/// the e2e acceptance suite uses.
fn make_source(account: AccountId, root: &Path, folder_id: &str) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id: account,
        display_name: "ntfs-chaos".to_string(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: folder_id.to_string(),
        drive_folder_path: "/ntfs-chaos".to_string(),
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

/// Count non-trashed objects directly under `folder_id` in the concrete
/// fake.
fn live_object_count(remote: &InMemoryRemoteStore, folder_id: &str) -> u64 {
    remote
        .list_folder_with_trashed(folder_id)
        .into_iter()
        .filter(|e| !e.trashed)
        .count() as u64
}

/// Count `file_state` rows in `status='synced'` for the source.
async fn synced_count(state: &dyn StateRepo, source_id: SourceId) -> anyhow::Result<u64> {
    let rows = state.load_source_file_state(source_id).await?;
    Ok(rows
        .values()
        .filter(|r| r.status == FileStateStatus::Synced)
        .count() as u64)
}

/// Whether the activity log carries at least one row whose `event_type`
/// equals `code`'s stable string. Used to OBSERVE a surfaced [`ErrorCode`]
/// (the orchestrator records per-file failures with
/// `event_type = code.code()`, DESIGN s5 `record_outcome_activity`).
async fn saw_error_code(
    state: &dyn StateRepo,
    source_id: SourceId,
    code: ErrorCode,
) -> anyhow::Result<bool> {
    let page = state
        .query_activity(
            ActivityFilter {
                source_id: Some(source_id),
                event_types: vec![code.code().to_string()],
                ..ActivityFilter::default()
            },
            PageRequest::first(10_000),
        )
        .await?;
    Ok(!page.rows.is_empty())
}

/// Count activity rows at `Error` level for the source - a generic
/// "anything went wrong?" probe used by the clean-success rows to assert
/// the cycle logged no errors.
async fn error_level_activity_count(
    state: &dyn StateRepo,
    source_id: SourceId,
) -> anyhow::Result<u64> {
    let page = state
        .query_activity(
            ActivityFilter {
                source_id: Some(source_id),
                min_level: Some(ActivityLevel::Error),
                ..ActivityFilter::default()
            },
            PageRequest::first(10_000),
        )
        .await?;
    Ok(page.total)
}

/// The cross-scenario s6.3 invariant subset these rows check against the
/// in-memory fake, delegated to the canonical central helper
/// [`crate::scenarios::reporting::assert_invariants`] so every category
/// computes the s6.3 sweep identically (no data loss, no duplicate
/// `client_op_uuid`, no leaked `pending_ops`). Returns the human notes plus
/// the computed [`reporting::InvariantReport`]; callers feed the report into
/// [`reporting::InvariantReport::to_invariant_outcome`] for the
/// runner-enforced [`Outcome::invariants`] field and read
/// [`reporting::InvariantReport::data_loss_paths`] for the
/// [`Outcome::final_hash_matches_local`] semantic.
///
/// The note vector carries [`reporting::InvariantReport::violation_summary`]
/// only when the report is not clean, so a passing row stays quiet. Takes the
/// booted [`DrivenHandle`] because the central helper needs the clock and
/// `pending_ops` view, not just the remote.
async fn check_shared_invariants(
    handle: &DrivenHandle,
    remote: &InMemoryRemoteStore,
    source_id: SourceId,
    folder_id: &str,
) -> anyhow::Result<(Vec<String>, crate::scenarios::reporting::InvariantReport)> {
    let report =
        crate::scenarios::reporting::assert_invariants(handle, remote, source_id, folder_id)
            .await?;
    let mut notes = Vec::new();
    if !report.ok() {
        notes.push(report.violation_summary());
    }
    Ok((notes, report))
}

/// Pull `(src_root, state_db_path)` from a scenario's [`FixtureState`],
/// erroring loudly if `setup` did not run.
fn fixture_paths(fixture: &Mutex<FixtureState>) -> anyhow::Result<(PathBuf, PathBuf)> {
    let f = fixture.lock().expect("fixture mutex");
    let src = f
        .src_dir
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("setup did not run (no src_dir)"))?
        .path()
        .to_path_buf();
    let db = f
        .state_dir
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("setup did not run (no state_dir)"))?
        .path()
        .join("state.db");
    Ok((src, db))
}

/// Clear a scenario's [`FixtureState`], dropping (and thus deleting) the
/// throwaway tempdirs.
fn clear_fixture(fixture: &Mutex<FixtureState>) {
    let mut f = fixture.lock().expect("fixture mutex");
    f.src_dir = None;
    f.state_dir = None;
}

/// Create a real symlink to a file, OS-appropriate. On Windows needs
/// `SeCreateSymbolicLinkPrivilege` (admin / dev-mode); rows using this gate
/// `Capability::Admin` so it only runs where the privilege is held.
fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (target, link);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks unsupported on this platform",
        ))
    }
}

/// Create a real symlink to a directory, OS-appropriate.
fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link)
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (target, link);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks unsupported on this platform",
        ))
    }
}

/// Write `contents` to `root`-relative `rel`, creating parent dirs.
fn write_file(root: &Path, rel: &str, contents: &[u8]) -> std::io::Result<()> {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

// ===========================================================================
// 1. hardlink-two-paths            (Windows, admin, cap:ntfs_volume)
// ===========================================================================

/// `mklink /H` two paths to the same inode under a source. Per DESIGN
/// s5.2.1 each path is uploaded independently (bytes duplicated on Drive);
/// two `file_state` rows, two Drive objects. Documented.
struct HardlinkTwoPaths {
    fixture: Mutex<FixtureState>,
}

impl HardlinkTwoPaths {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for HardlinkTwoPaths {
    fn name(&self) -> &'static str {
        "hardlink-two-paths"
    }
    fn description(&self) -> &'static str {
        "two hardlinks to one inode each upload independently; two Drive objects"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![
            Capability::Windows,
            Capability::Admin,
            Capability::NtfsVolume,
        ])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        // The primary file plus a hardlink to it. To the walker both paths
        // are ordinary readable files (a hardlink is not a reparse point),
        // so both are backed up as independent objects.
        let primary = root.join("primary.bin");
        std::fs::write(&primary, b"hardlinked-content")?;
        std::fs::hard_link(&primary, root.join("alias.bin"))?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "primary + hardlink both backed up independently (live objects={live}); DESIGN s5.2.1 documents bytes duplicated on Drive"
        ));
        if live != 2 {
            notes.push(format!("expected 2 live objects, observed {live}"));
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty() && live == 2,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ===========================================================================
// 2. symlink-to-file               (Windows, admin)
// ===========================================================================

/// `mklink` to a file under a source. Default: skipped per DESIGN s5.2.1
/// (don't follow). V1 ships only `SymlinkPolicy::Skip`, so the per-source
/// follow toggle is documented as V2 and not exercised here.
struct SymlinkToFile {
    fixture: Mutex<FixtureState>,
}

impl SymlinkToFile {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for SymlinkToFile {
    fn name(&self) -> &'static str {
        "symlink-to-file"
    }
    fn description(&self) -> &'static str {
        "symlink to a file is skipped (not followed, not backed up) by default"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows, Capability::Admin])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        // A real file (backed up) plus a symlink TO it (skipped). The target
        // lives inside the source so the only reason it is not double-counted
        // is the skip policy, not exclusion.
        let target = root.join("real.txt");
        std::fs::write(&target, b"symlink-target")?;
        symlink_file(&target, &root.join("link.txt"))?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "symlink skipped per SymlinkPolicy::Skip; only the real target backed up (live objects={live}). Per-source follow toggle is V2."
        ));
        if live != 1 {
            notes.push(format!(
                "expected 1 live object (real file only), observed {live}"
            ));
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty() && live == 1,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// 3. symlink-to-directory          (Windows, admin)
// ===========================================================================

/// `mklink /D` to a directory inside the source. Skipped by default. With
/// follow-symlinks ON the walker's cycle detection must engage if the link
/// target is an ancestor - that follow path is V2 (no toggle in V1),
/// documented but not exercised.
struct SymlinkToDirectory {
    fixture: Mutex<FixtureState>,
}

impl SymlinkToDirectory {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for SymlinkToDirectory {
    fn name(&self) -> &'static str {
        "symlink-to-directory"
    }
    fn description(&self) -> &'static str {
        "directory symlink is skipped by default; follow-ON cycle detection is V2"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows, Capability::Admin])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        // A real subtree with one file, plus a directory symlink pointing at
        // the source root (an ancestor) - the cycle the follow path would
        // have to detect. With Skip the walker never descends it.
        write_file(root, "sub/keep.txt", b"in-subtree")?;
        symlink_dir(root, &root.join("loop_link"))?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        // Completion proves the skip policy: a followed ancestor symlink
        // would recurse forever.
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "directory symlink (-> ancestor) skipped, no infinite descent; real subtree backed up (live objects={live}). Follow-ON cycle detection is V2."
        ));
        if live != 1 {
            notes.push(format!(
                "expected 1 live object (sub/keep.txt), observed {live}"
            ));
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty() && live == 1,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// 4. junction-mklink-j             (Windows, admin, cap:ntfs_volume)
// ===========================================================================

/// `mklink /J` to another folder (same volume). Junctions are reparse
/// points, treated like symlinks: skipped by default. Rust reports `true`
/// for `is_symlink()` on NTFS junctions, so the scanner's symlink-skip path
/// covers them.
struct JunctionMklinkJ {
    fixture: Mutex<FixtureState>,
}

impl JunctionMklinkJ {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for JunctionMklinkJ {
    fn name(&self) -> &'static str {
        "junction-mklink-j"
    }
    fn description(&self) -> &'static str {
        "NTFS junction (reparse point) is skipped by default like a symlink"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![
            Capability::Windows,
            Capability::Admin,
            Capability::NtfsVolume,
        ])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        write_file(root, "data/file.txt", b"behind-junction")?;
        // A directory symlink stands in for a junction: both are reparse
        // points the scanner detects via is_symlink and skips. A true
        // `mklink /J` junction has the same reparse-point category; the skip
        // behaviour under test is identical.
        symlink_dir(&root.join("data"), &root.join("junction"))?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "junction (reparse point) skipped; backing target not double-counted via the junction path (live objects={live})"
        ));
        if live != 1 {
            notes.push(format!(
                "expected 1 live object (data/file.txt), observed {live}"
            ));
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty() && live == 1,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// 5. reparse-point-non-symlink     (Windows, admin, cap:ntfs_volume)
// ===========================================================================

/// A NTFS Dedup / Storage-Replica reparse point (synthesised via `fsutil
/// reparsepoint`). Read normally - the OS handles the indirection.
///
/// Synthesising a NON-symlink reparse point needs the Win32
/// `DeviceIoControl(FSCTL_SET_REPARSE_POINT)` device-IO path, not reachable
/// from `std`; the harness adds no winapi dependency. The scenario stands
/// up a plain readable file (the post-OS-resolve view of such a reparse
/// point) and documents that the genuine non-symlink reparse fixture
/// requires a Win32 device-IO helper the harness lacks.
struct ReparsePointNonSymlink {
    fixture: Mutex<FixtureState>,
}

impl ReparsePointNonSymlink {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for ReparsePointNonSymlink {
    fn name(&self) -> &'static str {
        "reparse-point-non-symlink"
    }
    fn description(&self) -> &'static str {
        "non-symlink reparse point reads normally; genuine fixture needs Win32 device-IO"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![
            Capability::Windows,
            Capability::Admin,
            Capability::NtfsVolume,
        ])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        // The OS-resolved view of a Dedup / Storage-Replica reparse point is
        // an ordinary readable file; that is what Driven sees and backs up.
        write_file(
            root,
            "deduped.bin",
            b"content-behind-a-non-symlink-reparse-point",
        )?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "non-symlink reparse point reads transparently; file backed up (live objects={live})"
        ));
        notes.push(
            "genuine FSCTL_SET_REPARSE_POINT fixture needs a Win32 device-IO helper the harness does not bundle; OS-resolved view exercised instead".to_string(),
        );
        if live != 1 {
            notes.push(format!("expected 1 live object, observed {live}"));
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty() && live == 1,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// 6. onedrive-placeholder          (Windows)
// ===========================================================================

/// A file marked `FILE_ATTRIBUTE_RECALL_ON_OPEN` (OneDrive Files-On-Demand
/// placeholder). Per DESIGN s5.2.1 the scanner detects the attribute and
/// skips by default (`scanner.rs` already implements this).
///
/// Setting the recall-on-open attribute needs `SetFileAttributes` (Win32),
/// not reachable from `std`; the harness adds no winapi dep. The scenario
/// asserts a hydrated file is backed up and documents that the dehydrated
/// placeholder bit could not be synthesised in-harness (the skip itself is
/// unit-tested in `scanner.rs`).
struct OnedrivePlaceholder {
    fixture: Mutex<FixtureState>,
}

impl OnedrivePlaceholder {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for OnedrivePlaceholder {
    fn name(&self) -> &'static str {
        "onedrive-placeholder"
    }
    fn description(&self) -> &'static str {
        "cloud-only placeholder is skipped (RECALL_ON_OPEN); attr needs Win32 to set"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        // A hydrated (normal) file is backed up; the dehydrated-placeholder
        // skip is exercised in scanner.rs unit tests where the attribute can
        // be forced.
        write_file(root, "hydrated.txt", b"hydrated-onedrive-file")?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "hydrated file backed up normally (live objects={live})"
        ));
        notes.push(
            "RECALL_ON_OPEN skip is implemented in scanner.rs; marking a placeholder needs Win32 SetFileAttributes the harness does not bundle, so the dehydrated case is documented not exercised here".to_string(),
        );
        if live != 1 {
            notes.push(format!("expected 1 live object, observed {live}"));
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty() && live == 1,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// 7. recursive-junction-cycle      (Windows, admin, cap:ntfs_volume)
// ===========================================================================

/// Folder `A/B` contains a junction `loop` pointing at `A`. The walker's
/// cycle defence (`follow_links(false)`, so the reparse point is never
/// descended) engages; no infinite loop; finite completion. A wall-clock
/// cap in the runner catches a regression.
struct RecursiveJunctionCycle {
    fixture: Mutex<FixtureState>,
}

impl RecursiveJunctionCycle {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for RecursiveJunctionCycle {
    fn name(&self) -> &'static str {
        "recursive-junction-cycle"
    }
    fn description(&self) -> &'static str {
        "junction cycle A/B/loop -> A completes finitely; no infinite walk"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![
            Capability::Windows,
            Capability::Admin,
            Capability::NtfsVolume,
        ])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        // A/B with a real file, and A/B/loop -> A (a directory reparse point
        // forming a cycle). Skip policy means `loop` is never descended.
        write_file(root, "A/B/leaf.txt", b"in-the-cycle")?;
        symlink_dir(&root.join("A"), &root.join("A/B/loop"))?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        // Completion is the assertion: a followed cycle would never return.
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "junction cycle did not cause infinite descent; scan completed (live objects={live})"
        ));
        if live != 1 {
            notes.push(format!(
                "expected 1 live object (A/B/leaf.txt), observed {live}"
            ));
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty() && live == 1,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// 8. cross-volume-symlink          (Windows, admin)
// ===========================================================================

/// Symlink under source on volume X points to a file on volume Y; then
/// unmount Y. Default: skipped (not followed) - so the unmount of Y is
/// irrelevant under V1's skip policy. The follow-ON + Y-unmounted
/// `local.io_error` path is V2 (no follow toggle in V1), documented.
struct CrossVolumeSymlink {
    fixture: Mutex<FixtureState>,
}

impl CrossVolumeSymlink {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for CrossVolumeSymlink {
    fn name(&self) -> &'static str {
        "cross-volume-symlink"
    }
    fn description(&self) -> &'static str {
        "cross-volume symlink skipped by default; follow-ON io_error path is V2"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows, Capability::Admin])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        write_file(root, "local_real.txt", b"on-volume-X")?;
        // A symlink whose target need not resolve: under Skip the target
        // (cross-volume, possibly unmounted) is never read. Point it at a
        // non-existent path to model "Y unmounted". A creation failure here
        // is non-fatal - the scenario asserts only the real file's backup and
        // the no-crash property.
        let _ = symlink_file(
            Path::new("Y:/gone/elsewhere.txt"),
            &root.join("dangling_link.txt"),
        );

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        // No crash even though the link dangles, because it is never followed.
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "dangling cross-volume symlink skipped (not followed); real file backed up, no crash (live objects={live}). Follow-ON + unmounted-Y local.io_error path is V2."
        ));
        if live != 1 {
            notes.push(format!(
                "expected 1 live object (local_real.txt), observed {live}"
            ));
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty() && live == 1,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// 9. cross-volume-link-stale-after-reassign   (Windows, admin)
// ===========================================================================

/// A drive-letter-based link on volume X; X is re-lettered. Driven does not
/// chase drive-letter rewrites; affected paths surface as `local.io_error`
/// and the failure is documented in the activity log.
///
/// Re-lettering a real volume mid-run needs `mountvol` / admin volume
/// management the harness will not perform on a dev box. The scenario backs
/// up a stable file and documents that the io_error-on-stale-path surfacing
/// needs real volume management; identity stays `(source_id,
/// relative_path)`.
struct CrossVolumeLinkStaleAfterReassign {
    fixture: Mutex<FixtureState>,
}

impl CrossVolumeLinkStaleAfterReassign {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for CrossVolumeLinkStaleAfterReassign {
    fn name(&self) -> &'static str {
        "cross-volume-link-stale-after-reassign"
    }
    fn description(&self) -> &'static str {
        "drive-letter reassignment not chased; stale path -> local.io_error (documented)"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows, Capability::Admin])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        write_file(root, "stable.txt", b"keyed-by-source-id-and-relative-path")?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "file backed up before any reassignment (live objects={live}); identity is (source_id, relative_path), not drive letter"
        ));
        notes.push(
            "actual drive-letter reassignment needs mountvol/admin volume management; the local.io_error-on-stale-path surfacing is documented, not exercised on a dev box".to_string(),
        );
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty(),
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// 10. ads-alternate-data-stream    (Windows, cap:ntfs_volume)
// ===========================================================================

/// File `foo.txt` with an ADS `foo.txt:hidden`. The main stream is backed
/// up; the ADS is lost.
///
/// The s10 `local.ads_skipped` code exists in [`ErrorCode`] but NOTHING in
/// the V1 core emits it - the scanner has no ADS enumeration. This scenario
/// honestly documents that gap: the main stream backs up, the ADS is
/// silently dropped, and NO `local.ads_skipped` activity row is produced.
/// It asserts the gap (the code was NOT seen) rather than faking the
/// not-yet-implemented surfacing.
struct AdsAlternateDataStream {
    fixture: Mutex<FixtureState>,
}

impl AdsAlternateDataStream {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for AdsAlternateDataStream {
    fn name(&self) -> &'static str {
        "ads-alternate-data-stream"
    }
    fn description(&self) -> &'static str {
        "main stream backed up; named ADS not backed up but surfaced as a local.ads_skipped warning"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows, Capability::NtfsVolume])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        let main = root.join("foo.txt");
        std::fs::write(&main, b"main-stream-content")?;
        // Write an alternate data stream via the `path:stream` Win32 naming
        // (NTFS-only). The scanner must detect this named stream and surface a
        // local.ads_skipped warning; if the fixture write fails the scenario
        // cannot exercise the detection, so surface that loudly below.
        #[cfg(windows)]
        {
            let ads_path = format!("{}:hidden", main.display());
            std::fs::write(&ads_path, b"secret-ads-bytes")?;
        }

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        // P1-D: the scanner now enumerates named NTFS data streams
        // (FindFirstStreamW) and the orchestrator writes a durable
        // local.ads_skipped WARNING activity row per affected file. The win
        // condition is that the named stream loss is SURFACED, not silent.
        let saw_ads =
            saw_error_code(handle.state.as_ref(), source_id, ErrorCode::LocalAdsSkipped).await?;
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "main stream backed up (live objects={live}); named ADS not backed up but local.ads_skipped surfaced={saw_ads} (P1-D: scanner enumerates streams, orchestrator emits the warning - the loss is visible, not silent)."
        ));
        anyhow::ensure!(
            live >= 1,
            "the main (unnamed) stream must still be backed up; got {live} live objects"
        );
        // The defining assertion: the named-stream skip MUST be surfaced as a
        // local.ads_skipped warning (SPEC s24). A false here means the scanner
        // silently dropped the ADS - exactly the silent data loss P1-D closes.
        anyhow::ensure!(
            saw_ads,
            "local.ads_skipped must be surfaced for a file carrying named NTFS data streams; \
             the scanner did not emit it (silent ADS loss regression)"
        );
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty() && live >= 1,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        // The main stream backs up cleanly and the named-stream skip is
        // surfaced as a warning (not an error code), so the row documents the
        // V1 ADS behaviour and asserts the warning inline above.
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// 11. sparse-file                  (Windows, cap:ntfs_volume)
// ===========================================================================

/// A sparse file with only a little allocated data. Driven uploads the
/// LOGICAL content (zero ranges become real zeros on the wire); the
/// size-on-Drive vs size-on-disk skew is documented. Reading a sparse file
/// is transparent (the OS materialises zeros), so this is a clean Success.
struct SparseFile {
    fixture: Mutex<FixtureState>,
}

impl SparseFile {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for SparseFile {
    fn name(&self) -> &'static str {
        "sparse-file"
    }
    fn description(&self) -> &'static str {
        "sparse file uploads its logical content (zeros materialised); clean success"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows, Capability::NtfsVolume])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        // 4 MiB of data then a 4 MiB logical-zero tail (kept modest so CI
        // stays fast). Setting the NTFS sparse flag needs `fsutil sparse
        // setflag` (Win32 FSCTL); the logical read path is identical with or
        // without it, so the harness writes the logical content directly.
        let mut buf = vec![0xABu8; 4 * 1024 * 1024];
        buf.extend(std::iter::repeat_n(0u8, 4 * 1024 * 1024));
        std::fs::write(root.join("sparse.bin"), &buf)?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let synced = synced_count(handle.state.as_ref(), source_id).await?;
        let errors = error_level_activity_count(handle.state.as_ref(), source_id).await?;
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "sparse file uploaded its full logical content (synced rows={synced}, live objects={live}, error rows={errors}); size-on-Drive == logical size, larger than size-on-disk (documented skew)"
        ));
        if live != 1 || synced != 1 || errors != 0 {
            notes.push("expected exactly one fully-synced object and no errors".to_string());
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty()
                && live == 1
                && errors == 0,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ===========================================================================
// 12. compressed-ntfs-file         (Windows, cap:ntfs_volume)
// ===========================================================================

/// File with NTFS compression on (`compact /c`). Reads transparently as
/// plaintext; uploads the decompressed bytes. NTFS compression is invisible
/// to read APIs, so this is a clean Success whether or not the `compact`
/// attribute is actually set.
struct CompressedNtfsFile {
    fixture: Mutex<FixtureState>,
}

impl CompressedNtfsFile {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for CompressedNtfsFile {
    fn name(&self) -> &'static str {
        "compressed-ntfs-file"
    }
    fn description(&self) -> &'static str {
        "NTFS-compressed file reads transparently; decompressed bytes uploaded"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows, Capability::NtfsVolume])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        let file = root.join("compressible.txt");
        // Highly compressible content (so NTFS compression would actually
        // shrink it). The read returns the decompressed bytes regardless.
        let content: Vec<u8> = std::iter::repeat_n(b'Z', 256 * 1024).collect();
        std::fs::write(&file, &content)?;
        // Best-effort enable NTFS compression; the read path is unaffected if
        // this fails (e.g. on a non-NTFS temp volume).
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("compact")
                .arg("/c")
                .arg(&file)
                .output();
        }

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let synced = synced_count(handle.state.as_ref(), source_id).await?;
        let errors = error_level_activity_count(handle.state.as_ref(), source_id).await?;
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "compressed file read as plaintext and uploaded decompressed (synced rows={synced}, error rows={errors})"
        ));
        if live != 1 || synced != 1 || errors != 0 {
            notes.push("expected exactly one fully-synced object and no errors".to_string());
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty()
                && live == 1
                && errors == 0,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ===========================================================================
// 13. encrypted-ntfs-efs           (Windows, cap:ntfs_volume)
// ===========================================================================

/// EFS-encrypted file owned by the current user. Same-user: reads + uploads
/// (the OS decrypts on read). The different-user (elevation) path surfaces
/// `local.io_error` and is the admin/elevation case - documented, not
/// exercised same-process.
struct EncryptedNtfsEfs {
    fixture: Mutex<FixtureState>,
}

impl EncryptedNtfsEfs {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for EncryptedNtfsEfs {
    fn name(&self) -> &'static str {
        "encrypted-ntfs-efs"
    }
    fn description(&self) -> &'static str {
        "same-user EFS file decrypts on read and uploads; different-user io_error documented"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows, Capability::NtfsVolume])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        let file = root.join("secret.txt");
        std::fs::write(&file, b"efs-protected-content")?;
        // Best-effort EFS-encrypt for the current user (`cipher /e`). The
        // same-user read returns plaintext with or without the attribute, so
        // a failure does not invalidate the scenario.
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("cipher")
                .arg("/e")
                .arg(&file)
                .output();
        }

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let synced = synced_count(handle.state.as_ref(), source_id).await?;
        let errors = error_level_activity_count(handle.state.as_ref(), source_id).await?;
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "same-user EFS file uploaded after transparent decrypt (synced rows={synced}, error rows={errors}); different-user local.io_error path needs a second principal + EncryptFile, documented not exercised"
        ));
        if live != 1 || synced != 1 || errors != 0 {
            notes.push("expected exactly one fully-synced object and no errors".to_string());
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty()
                && live == 1
                && errors == 0,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ===========================================================================
// 14. hidden-system-attributes     (Windows)
// ===========================================================================

/// File with `+H +S` (hidden + system) attributes. Backed up normally - the
/// `ignore` crate honours Driven's `hidden(false)` setting (SPEC s6), so a
/// hidden file is NOT excluded by the hidden default.
struct HiddenSystemAttributes {
    fixture: Mutex<FixtureState>,
}

impl HiddenSystemAttributes {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for HiddenSystemAttributes {
    fn name(&self) -> &'static str {
        "hidden-system-attributes"
    }
    fn description(&self) -> &'static str {
        "hidden+system file is backed up (ignore honours hidden(false))"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        let file = root.join("hidden_sys.dat");
        std::fs::write(&file, b"hidden-and-system")?;
        // Set +H +S via `attrib`. A failure to set the bits does not
        // invalidate the test (the scanner backs the file up either way); it
        // just makes the "hidden" aspect a no-op on that host.
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("attrib")
                .args(["+H", "+S"])
                .arg(&file)
                .output();
        }

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        let synced = synced_count(handle.state.as_ref(), source_id).await?;
        let errors = error_level_activity_count(handle.state.as_ref(), source_id).await?;
        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "hidden+system file backed up (synced rows={synced}, error rows={errors}); ignore honours hidden(false) per SPEC s6"
        ));
        if live != 1 || synced != 1 || errors != 0 {
            notes.push("expected the hidden+system file to be fully synced".to_string());
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty()
                && live == 1
                && errors == 0,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ===========================================================================
// 15. file-id-reuse-after-defrag   (Windows, admin)
// ===========================================================================

/// Synthetic: two `file_state` rows whose recorded inode / file-index later
/// collide post-defrag. Driven keys local identity by `(source_id,
/// relative_path)`, NOT by inode (DESIGN s5.2.3), so a file-id collision
/// causes no misbehaviour. Asserted by snapshot diff: two distinct paths
/// stay two distinct rows / objects even when an external file-id would
/// alias them.
struct FileIdReuseAfterDefrag {
    fixture: Mutex<FixtureState>,
}

impl FileIdReuseAfterDefrag {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for FileIdReuseAfterDefrag {
    fn name(&self) -> &'static str {
        "file-id-reuse-after-defrag"
    }
    fn description(&self) -> &'static str {
        "identity is (source_id, relative_path) not inode; file-id reuse causes no aliasing"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Windows, Capability::Admin])
    }
    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();
        // Two distinct files. Post-defrag the OS could hand the second the
        // first's freed file-index; Driven never reads inode, so both remain
        // independent backups keyed by path.
        write_file(root, "alpha.txt", b"alpha-bytes")?;
        write_file(root, "beta.txt", b"beta-bytes")?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = fixture_paths(&self.fixture)?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;
        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let live = live_object_count(&remote, &folder);
        // Both paths persist as distinct rows; no row was overwritten by an
        // aliased file-id (which would have collapsed them to one).
        let rows = handle.state.load_source_file_state(source_id).await?;
        let alpha = RelativePath::try_from("alpha.txt".to_string())
            .map_err(|e| anyhow::anyhow!("rel alpha: {e}"))?;
        let beta = RelativePath::try_from("beta.txt".to_string())
            .map_err(|e| anyhow::anyhow!("rel beta: {e}"))?;
        let both_present = rows.contains_key(&alpha) && rows.contains_key(&beta);

        let (mut notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.push(format!(
            "two distinct paths remain two distinct rows/objects (live objects={live}, both keys present={both_present}); identity is (source_id, relative_path), inode never consulted"
        ));
        if !both_present || live != 2 {
            notes.push("expected two independent backups keyed by path".to_string());
        }
        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty()
                && both_present
                && live == 2,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        clear_fixture(&self.fixture);
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every NTFS scenario is registered, uniquely named, kebab-case, and
    /// declares the platform capability the catalogue lists.
    #[test]
    fn registry_is_complete_and_well_formed() {
        let all = scenarios();
        assert_eq!(all.len(), 15, "s3.5 lists 15 NTFS hazards");

        let mut names: Vec<&str> = all.iter().map(|s| s.name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "scenario names must be unique");

        for s in &all {
            let name = s.name();
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{name} must be kebab-case ascii"
            );
            assert!(!s.description().is_empty(), "{name} needs a description");
            // Every s3.5 row is Windows-scoped; the requirement set must say so.
            let reqs = s.requires();
            assert!(
                reqs.required.contains(&Capability::Windows),
                "{name} must require Windows per s3.5"
            );
        }
    }

    /// The admin rows must SKIP (report missing capabilities) on a
    /// non-elevated host - the honest s2.5/s8 behaviour, not a fake green.
    #[test]
    fn admin_rows_skip_without_elevation() {
        use crate::capabilities::CapabilitySet;
        let bare = CapabilitySet::default(); // admin=false, ntfs_volume=None
        let hardlink = HardlinkTwoPaths::new();
        let missing = hardlink.requires().missing(&bare);
        assert!(
            missing.iter().any(|m| m == "admin"),
            "hardlink-two-paths must report missing admin on a bare host, got {missing:?}"
        );
    }

    /// `setup` materialises a real on-disk fixture for a representative
    /// non-link, non-privileged row, so the fixture-build + tempdir wiring is
    /// exercised even where the link rows SKIP. Also confirms teardown clears
    /// the fixture state.
    #[tokio::test]
    async fn setup_then_teardown_round_trips_fixture() {
        let scenario = HiddenSystemAttributes::new();
        let mut ctx = ScenarioContext::default();
        scenario
            .setup(&mut ctx)
            .await
            .expect("setup builds fixture");
        assert!(
            ctx.fixture_root.join("hidden_sys.dat").exists(),
            "setup must write the source file under fixture_root"
        );
        scenario
            .teardown(&mut ctx)
            .await
            .expect("teardown clears fixture");
        let f = scenario.fixture.lock().expect("fixture mutex");
        assert!(
            f.src_dir.is_none() && f.state_dir.is_none(),
            "teardown must drop the tempdirs"
        );
    }
}
