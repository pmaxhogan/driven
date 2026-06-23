//! Permissions and ACL scenarios (STRESS_HARNESS s3.3).
//!
//! Four rows, one [`Scenario`] impl each:
//!
//! `windows-acl-deny-read-file` (Windows + NTFS + admin): an NTFS ACL
//! denies READ on ONE file. Driven surfaces `local.io_error` for that
//! file and uploads the rest of the source.
//!
//! `posix-mode-000` (Unix): a `chmod 000` file the process can stat but
//! not open. Same outcome - `local.io_error` for that file, the rest of
//! the source completes.
//!
//! `windows-acl-deny-enumerate` (Windows + NTFS + admin): an NTFS ACL
//! denies LIST DIRECTORY on a subfolder. The scanner's walker yields an
//! `Err` for that subtree, so the DESIGN s5.2 delete-suppression engages:
//! the already-uploaded files under the unreadable subtree are NOT trashed
//! on Drive even though the scan cannot re-confirm them this cycle.
//!
//! `setuid-files` (Unix): a `chmod 4755` file. Driven uploads it as plain
//! content; the setuid bit is a documented V1 limitation (not preserved on
//! Drive). Asserted as [`ExpectedOutcome::DocumentedBehaviour`].
//!
//! ## How these scenarios drive the core
//!
//! The Phase-1 [`DrivenHandle`] exposes its remote as
//! `Arc<dyn RemoteStore>`, and the `RemoteStore` trait carries no
//! `root_id()` accessor - so a scenario cannot recover the fake's root
//! folder id (a per-instance UUID) through the booted handle. Each
//! scenario therefore constructs its OWN concrete
//! [`InMemoryRemoteStore`], captures `root_id()` for the source folder +
//! the post-run object-count assertions, and boots a dedicated
//! [`DrivenHandle`] over that remote inside `run_assertions`. This mirrors
//! the proven `driven-core` `e2e_fake` pattern (own remote, own state DB,
//! real scan -> plan -> execute pipeline). The `handle` argument the trait
//! passes is the harness default; the permissions rows do not use it
//! because they need the concrete remote. This divergence is surfaced in
//! the M3.7 report.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use driven_core::orchestrator::TickSource;
use driven_core::state::{ActivityFilter, ActivityLevel, PageRequest, SourceRow, StateRepo};
use driven_core::types::{AccountId, ErrorCode, SourceId};

use driven_drive::fake::InMemoryRemoteStore;

use crate::capabilities::{Capability, CapabilityRequirements};
use crate::handle::{DrivenHandle, DrivenHandleBuilder};
use crate::scenario::{ExpectedOutcome, Outcome, Scenario, ScenarioContext};
use crate::scenarios::reporting;

/// Per-scenario fixture handles kept alive between `setup` and
/// `run_assertions` / `teardown`.
///
/// The on-disk source tree and the hermetic state-DB dir both live in
/// throwaway [`tempfile::TempDir`]s; dropping them (on teardown, or if the
/// scenario object drops) removes them. We hold them behind a [`Mutex`] so
/// the otherwise-`&self` trait methods can populate / read / clear them.
#[derive(Default)]
struct FixtureState {
    /// The materialised source tree.
    src_dir: Option<tempfile::TempDir>,
    /// The hermetic state-DB directory.
    state_dir: Option<tempfile::TempDir>,
}

/// Boot a [`DrivenHandle`] over `state_dir`'s SQLite file plus the given
/// concrete remote, register `src_root` as a source uploading into the
/// fake's root folder, and return the handle alongside the source id and
/// remote root folder id.
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
/// with every optional knob defaulted to "off" so the scan is the plain
/// FastPath used by the e2e acceptance suite.
fn make_source(account: AccountId, root: &Path, folder_id: &str) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id: account,
        display_name: "permissions-chaos".to_string(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: folder_id.to_string(),
        drive_folder_path: "/permissions-chaos".to_string(),
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

/// Count non-trashed objects under `folder_id` in the concrete fake.
fn live_object_count(remote: &InMemoryRemoteStore, folder_id: &str) -> usize {
    remote
        .list_folder_with_trashed(folder_id)
        .into_iter()
        .filter(|e| !e.trashed)
        .count()
}

/// Whether the activity log carries at least one row whose `event_type`
/// equals `code`'s stable string AND whose level is `Error`.
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
                min_level: Some(ActivityLevel::Error),
                ..ActivityFilter::default()
            },
            PageRequest {
                page: 0,
                limit: 10_000,
            },
        )
        .await?;
    Ok(!page.rows.is_empty())
}

/// The cross-scenario s6.3 invariant subset the permissions rows assert
/// against the in-memory fake. This delegates to the canonical
/// [`reporting::assert_invariants`] checker (the single source of truth for
/// the s6.3 sweep) and surfaces its violation summary as a human note when
/// any invariant trips. Returns the notes plus the computed
/// [`reporting::InvariantReport`] so the caller can both populate
/// `Outcome::invariants` and derive the per-scenario data-loss flag.
async fn check_shared_invariants(
    handle: &DrivenHandle,
    remote: &InMemoryRemoteStore,
    source_id: SourceId,
    folder_id: &str,
) -> anyhow::Result<(Vec<String>, reporting::InvariantReport)> {
    let mut notes = Vec::new();
    let report = reporting::assert_invariants(handle, remote, source_id, folder_id).await?;
    if !report.ok() {
        notes.push(report.violation_summary());
    }
    Ok((notes, report))
}

// ---------------------------------------------------------------------------
// Unix permission helpers (mode-000, setuid). cfg-gated so the Windows build
// never references `PermissionsExt`.
// ---------------------------------------------------------------------------

/// Apply a raw Unix mode to `path`. Returns the IO error verbatim so a
/// fixture-build failure aborts the scenario (harness self-error) rather
/// than masking it.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms)
}

/// Restore a directory/file to a readable+writable mode so the teardown's
/// tempdir cleanup can remove it (a mode-000 file inside a 0700 dir removes
/// fine, but a `chmod 000` on a *directory* would block cleanup; we only
/// ever mode-000 a file, yet restore defensively).
#[cfg(unix)]
fn restore_mode(path: &Path) {
    let _ = set_mode(path, 0o644);
}

// ---------------------------------------------------------------------------
// Windows ACL helpers (deny-read-file, deny-enumerate). cfg-gated; they shell
// out to `icacls`, which is present on every NTFS-capable Windows host. The
// scenarios that call these are capability-gated on Windows + NTFS + admin,
// so on a non-Windows host this code is never compiled-in AND never reached.
// ---------------------------------------------------------------------------

/// Add a deny ACE for the current user via `icacls`. `perm` is an icacls
/// permission token, e.g. `R` (read) or `RD` (read-data / list-directory).
/// Returns an error if `icacls` is missing or fails so a fixture-build
/// failure is a harness self-error, not a false outcome.
#[cfg(windows)]
fn icacls_deny(path: &Path, perm: &str) -> anyhow::Result<()> {
    icacls_run(path, &["/deny", &format!("*S-1-1-0:({perm})")])
}

/// Remove every deny ACE we added for Everyone so the tempdir teardown can
/// delete the tree. Best-effort: a failure is logged, not propagated.
#[cfg(windows)]
fn icacls_undeny(path: &Path) {
    if let Err(err) = icacls_run(path, &["/remove:d", "*S-1-1-0"]) {
        tracing::warn!(target: "driven_chaos::permissions", path = %path.display(), %err, "icacls undeny failed during teardown");
    }
}

#[cfg(windows)]
fn icacls_run(path: &Path, extra: &[&str]) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new("icacls");
    cmd.arg(path);
    for a in extra {
        cmd.arg(a);
    }
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("spawning icacls failed: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "icacls {} {:?} failed: {}",
            path.display(),
            extra,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

// ===========================================================================
// Row: posix-mode-000
// ===========================================================================

/// A `chmod 000` file inside an otherwise-normal source: stat-able but not
/// open-able. Driven surfaces `local.io_error` for that file and uploads
/// the rest of the source (STRESS_HARNESS s3.3).
struct PosixMode000 {
    fixture: Mutex<FixtureState>,
}

impl PosixMode000 {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for PosixMode000 {
    fn name(&self) -> &'static str {
        "posix-mode-000"
    }

    fn description(&self) -> &'static str {
        "Unix chmod-000 file: local.io_error for that file, rest of the source uploads"
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Unix])
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();

        // Three readable files + one we will lock down to mode 000.
        std::fs::write(root.join("a.txt"), b"alpha")?;
        std::fs::write(root.join("b.txt"), b"bravo")?;
        std::fs::write(root.join("c.txt"), b"charlie")?;
        let locked = root.join("locked.bin");
        std::fs::write(&locked, b"unreadable bytes")?;

        #[cfg(unix)]
        set_mode(&locked, 0o000)?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = {
            let f = self.fixture.lock().expect("fixture mutex");
            let src = f
                .src_dir
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("setup did not run"))?
                .path()
                .to_path_buf();
            let db = f
                .state_dir
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("setup did not run"))?
                .path()
                .join("state.db");
            (src, db)
        };

        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;

        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let mut notes = Vec::new();

        // The 3 readable files uploaded; the locked one did not.
        let live = live_object_count(&remote, &folder);
        notes.push(format!("{live} of 4 files uploaded (1 locked)"));

        let saw_io_error =
            saw_error_code(handle.state.as_ref(), source_id, ErrorCode::LocalIoError).await?;

        let (mut inv_notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.append(&mut inv_notes);

        // On a non-Unix host the scenario is SKIPPED by the capability gate
        // and run_assertions never executes, so the `cfg(unix)` outcome below
        // is the only path that runs in practice. We assert it directly: the
        // io_error must surface and exactly the 3 readable files upload.
        let error_codes_seen = if saw_io_error {
            vec![ErrorCode::LocalIoError]
        } else {
            vec![]
        };
        if !saw_io_error {
            notes.push("expected local.io_error for the mode-000 file but none was logged".into());
        }
        if live != 3 {
            notes.push(format!("expected 3 uploaded readable files, got {live}"));
        }

        Ok(Outcome {
            error_codes_seen,
            final_drive_object_count: live as u64,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty()
                && live == 3
                && saw_io_error,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let mut f = self.fixture.lock().expect("fixture mutex");
        #[cfg(unix)]
        if let Some(dir) = f.src_dir.as_ref() {
            restore_mode(&dir.path().join("locked.bin"));
        }
        f.src_dir = None;
        f.state_dir = None;
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::LocalIoError,
        }
    }
}

// ===========================================================================
// Row: setuid-files
// ===========================================================================

/// A `chmod 4755` (setuid) file. Driven uploads its plain content; the
/// setuid bit is a documented V1 limitation - not preserved on Drive
/// (STRESS_HARNESS s3.3). Asserted as documented behaviour: the file
/// uploads as ordinary bytes and the scan completes cleanly.
struct SetuidFiles {
    fixture: Mutex<FixtureState>,
}

impl SetuidFiles {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for SetuidFiles {
    fn name(&self) -> &'static str {
        "setuid-files"
    }

    fn description(&self) -> &'static str {
        "Unix chmod-4755 file: uploaded as plain content, setuid bit not preserved (documented)"
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![Capability::Unix])
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();

        std::fs::write(root.join("plain.txt"), b"ordinary")?;
        let suid = root.join("setuid.bin");
        std::fs::write(&suid, b"#!/bin/true setuid payload")?;

        // 0o4755 = setuid + rwxr-xr-x. Readable, so it WILL upload; the point
        // is that the setuid bit is dropped on Drive (which has no concept of
        // it), surfaced once per source as a V1 limitation.
        #[cfg(unix)]
        set_mode(&suid, 0o4755)?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = {
            let f = self.fixture.lock().expect("fixture mutex");
            let src = f
                .src_dir
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("setup did not run"))?
                .path()
                .to_path_buf();
            let db = f
                .state_dir
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("setup did not run"))?
                .path()
                .join("state.db");
            (src, db)
        };

        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;

        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let mut notes = Vec::new();

        // Both files upload as plain content - the setuid file is just bytes
        // on Drive; the bit cannot round-trip (DocumentedBehaviour).
        let live = live_object_count(&remote, &folder);
        notes.push(format!(
            "{live} of 2 files uploaded; setuid bit not represented on Drive (V1 limitation)"
        ));

        let (mut inv_notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.append(&mut inv_notes);

        if live != 2 {
            notes.push(format!("expected 2 uploaded files, got {live}"));
        }

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live as u64,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty() && live == 2,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = None;
        f.state_dir = None;
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ===========================================================================
// Row: windows-acl-deny-read-file
// ===========================================================================

/// An NTFS ACL denying READ on one file inside an otherwise-normal source.
/// Driven surfaces `local.io_error` for that file and uploads the rest
/// (STRESS_HARNESS s3.3). Windows + NTFS + admin.
struct WindowsAclDenyReadFile {
    fixture: Mutex<FixtureState>,
}

impl WindowsAclDenyReadFile {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for WindowsAclDenyReadFile {
    fn name(&self) -> &'static str {
        "windows-acl-deny-read-file"
    }

    fn description(&self) -> &'static str {
        "NTFS ACL denies READ on one file: local.io_error for it, rest of the source uploads"
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![
            Capability::Windows,
            Capability::NtfsVolume,
            Capability::Admin,
        ])
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();

        std::fs::write(root.join("a.txt"), b"alpha")?;
        std::fs::write(root.join("b.txt"), b"bravo")?;
        std::fs::write(root.join("c.txt"), b"charlie")?;
        let denied = root.join("denied.bin");
        std::fs::write(&denied, b"unreadable bytes")?;

        // Deny READ for Everyone (SID S-1-1-0). The file still stats (the
        // parent dir is enumerable), but opening it for read fails -> the
        // executor maps the open failure to local.io_error.
        #[cfg(windows)]
        icacls_deny(&denied, "R")?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = {
            let f = self.fixture.lock().expect("fixture mutex");
            let src = f
                .src_dir
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("setup did not run"))?
                .path()
                .to_path_buf();
            let db = f
                .state_dir
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("setup did not run"))?
                .path()
                .join("state.db");
            (src, db)
        };

        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;

        handle.orchestrator.run_cycle(TickSource::Manual).await?;

        let mut notes = Vec::new();
        let live = live_object_count(&remote, &folder);
        notes.push(format!("{live} of 4 files uploaded (1 read-denied)"));

        let saw_io_error =
            saw_error_code(handle.state.as_ref(), source_id, ErrorCode::LocalIoError).await?;

        let (mut inv_notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.append(&mut inv_notes);

        let error_codes_seen = if saw_io_error {
            vec![ErrorCode::LocalIoError]
        } else {
            notes.push(
                "expected local.io_error for the read-denied file but none was logged".into(),
            );
            vec![]
        };
        if live != 3 {
            notes.push(format!("expected 3 uploaded readable files, got {live}"));
        }

        Ok(Outcome {
            error_codes_seen,
            final_drive_object_count: live as u64,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty()
                && live == 3
                && saw_io_error,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let mut f = self.fixture.lock().expect("fixture mutex");
        #[cfg(windows)]
        if let Some(dir) = f.src_dir.as_ref() {
            icacls_undeny(&dir.path().join("denied.bin"));
        }
        f.src_dir = None;
        f.state_dir = None;
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::GracefulFailureWith {
            code: ErrorCode::LocalIoError,
        }
    }
}

// ===========================================================================
// Row: windows-acl-deny-enumerate
// ===========================================================================

/// An NTFS ACL denying LIST DIRECTORY on a subfolder. The scanner's walker
/// yields an `Err` for that subtree, so the DESIGN s5.2 delete-suppression
/// must engage: already-uploaded files under the unreadable subtree are NOT
/// trashed on Drive (STRESS_HARNESS s3.3). Windows + NTFS + admin.
///
/// The scenario runs TWO cycles against the SAME remote + state DB: cycle 1
/// with the subfolder readable (everything uploads), then we deny LIST and
/// run cycle 2. The discriminating assertion is that cycle 2 trashes
/// NOTHING - a naive diff that treated the now-unenumerable subtree as
/// "all deleted" would trash those objects, which the suppression prevents.
struct WindowsAclDenyEnumerate {
    fixture: Mutex<FixtureState>,
}

impl WindowsAclDenyEnumerate {
    fn new() -> Self {
        Self {
            fixture: Mutex::new(FixtureState::default()),
        }
    }
}

#[async_trait]
impl Scenario for WindowsAclDenyEnumerate {
    fn name(&self) -> &'static str {
        "windows-acl-deny-enumerate"
    }

    fn description(&self) -> &'static str {
        "NTFS ACL denies LIST on a subfolder: walker Err + delete-suppression; no trash cascade"
    }

    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::of(vec![
            Capability::Windows,
            Capability::NtfsVolume,
            Capability::Admin,
        ])
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let src_dir = tempfile::tempdir()?;
        let state_dir = tempfile::tempdir()?;
        let root = src_dir.path();

        // Two top-level files plus a subfolder of three. The subfolder is the
        // one we will deny LIST on for cycle 2.
        std::fs::write(root.join("top1.txt"), b"top one")?;
        std::fs::write(root.join("top2.txt"), b"top two")?;
        let sub = root.join("secret");
        std::fs::create_dir_all(&sub)?;
        std::fs::write(sub.join("s1.txt"), b"secret one")?;
        std::fs::write(sub.join("s2.txt"), b"secret two")?;
        std::fs::write(sub.join("s3.txt"), b"secret three")?;

        ctx.fixture_root = root.to_path_buf();
        ctx.cacheable = false;
        let mut f = self.fixture.lock().expect("fixture mutex");
        f.src_dir = Some(src_dir);
        f.state_dir = Some(state_dir);
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let (src_root, state_db) = {
            let f = self.fixture.lock().expect("fixture mutex");
            let src = f
                .src_dir
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("setup did not run"))?
                .path()
                .to_path_buf();
            let db = f
                .state_dir
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("setup did not run"))?
                .path()
                .join("state.db");
            (src, db)
        };

        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source_id, folder) =
            boot_with_source(state_db, remote.clone(), &src_root).await?;

        // --- cycle 1: subfolder readable -> everything uploads --------------
        handle.orchestrator.run_cycle(TickSource::Manual).await?;
        let after_cycle1 = live_object_count(&remote, &folder);
        let mut notes = Vec::new();
        notes.push(format!(
            "cycle 1 uploaded {after_cycle1} objects (expect 5)"
        ));

        // --- deny LIST DIRECTORY on the subfolder ---------------------------
        // icacls `RD` = Read Data / List Directory. Denying it for Everyone
        // makes the walker's enumeration of `secret/` return an Err, which the
        // scanner attributes to the `secret` prefix and suppresses any
        // deletion under it (DESIGN s5.2 step 3).
        let sub = src_root.join("secret");
        #[cfg(windows)]
        icacls_deny(&sub, "RD")?;
        let _ = &sub; // referenced on non-windows to avoid unused warnings

        // --- cycle 2: subfolder unenumerable -> NOTHING trashed -------------
        handle.orchestrator.run_cycle(TickSource::Manual).await?;
        let after_cycle2 = live_object_count(&remote, &folder);
        notes.push(format!(
            "cycle 2 (LIST denied) left {after_cycle2} live objects (expect 5 - no trash cascade)"
        ));

        // Re-allow LIST so the data-loss invariant + teardown can read state.
        #[cfg(windows)]
        icacls_undeny(&sub);

        let trashed = remote
            .list_folder_with_trashed(&folder)
            .into_iter()
            .filter(|e| e.trashed)
            .count();
        if trashed != 0 {
            notes.push(format!(
                "DELETE-SUPPRESSION FAILED: {trashed} objects trashed under the unreadable subtree"
            ));
        }

        let (mut inv_notes, inv_report) =
            check_shared_invariants(&handle, &remote, source_id, &folder).await?;
        notes.append(&mut inv_notes);

        let suppression_held = after_cycle1 == 5 && after_cycle2 == 5 && trashed == 0;
        if !suppression_held {
            notes.push("expected 5 live objects across both cycles with zero trashes".into());
        }

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: after_cycle2 as u64,
            final_hash_matches_local: inv_report.data_loss_paths.is_empty() && suppression_held,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        let mut f = self.fixture.lock().expect("fixture mutex");
        #[cfg(windows)]
        if let Some(dir) = f.src_dir.as_ref() {
            icacls_undeny(&dir.path().join("secret"));
        }
        f.src_dir = None;
        f.state_dir = None;
        Ok(())
    }

    fn expected_outcome(&self) -> ExpectedOutcome {
        // The expected behaviour is the suppression itself (no trash
        // cascade), asserted via the object-count snapshot in
        // run_assertions rather than an error code.
        ExpectedOutcome::DocumentedBehaviour
    }
}

/// Every permissions/ACL scenario (STRESS_HARNESS s3.3).
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(WindowsAclDenyReadFile::new()),
        Box::new(PosixMode000::new()),
        Box::new(WindowsAclDenyEnumerate::new()),
        Box::new(SetuidFiles::new()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry exposes all four s3.3 rows with stable kebab names.
    #[test]
    fn registers_all_four_rows() {
        let names: Vec<&str> = scenarios().iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec![
                "windows-acl-deny-read-file",
                "posix-mode-000",
                "windows-acl-deny-enumerate",
                "setuid-files",
            ]
        );
    }

    /// Each row declares the platform/privilege gate STRESS_HARNESS s3.3
    /// requires, so the runner SKIPs (never fails) on a host that lacks it.
    #[test]
    fn capability_gates_match_the_spec() {
        for s in scenarios() {
            let req = s.requires().required;
            match s.name() {
                "posix-mode-000" | "setuid-files" => {
                    assert!(req.contains(&Capability::Unix), "{} needs Unix", s.name());
                }
                "windows-acl-deny-read-file" | "windows-acl-deny-enumerate" => {
                    assert!(req.contains(&Capability::Windows));
                    assert!(req.contains(&Capability::NtfsVolume));
                    assert!(req.contains(&Capability::Admin));
                }
                other => panic!("unexpected scenario {other}"),
            }
        }
    }

    /// The two Unix rows surface the documented outcome: mode-000 expects a
    /// graceful `local.io_error`; setuid is a documented limitation.
    #[test]
    fn expected_outcomes_match_the_spec() {
        for s in scenarios() {
            match s.name() {
                "posix-mode-000" | "windows-acl-deny-read-file" => assert!(matches!(
                    s.expected_outcome(),
                    ExpectedOutcome::GracefulFailureWith {
                        code: ErrorCode::LocalIoError
                    }
                )),
                "setuid-files" | "windows-acl-deny-enumerate" => assert!(matches!(
                    s.expected_outcome(),
                    ExpectedOutcome::DocumentedBehaviour
                )),
                other => panic!("unexpected scenario {other}"),
            }
        }
    }

    /// On a Unix host the mode-000 row runs end-to-end against the in-memory
    /// fake: the locked file surfaces `local.io_error`, the other three
    /// upload, and the shared invariants hold. (On Windows this row is
    /// capability-SKIPPED, so the test only drives the impl where it applies.)
    #[cfg(unix)]
    #[tokio::test]
    async fn posix_mode_000_uploads_rest_and_logs_io_error() {
        let scenario = PosixMode000::new();
        let mut ctx = ScenarioContext::default();
        scenario.setup(&mut ctx).await.expect("setup");

        // A throwaway harness-default handle is required by the signature but
        // unused by this row (it boots its own concrete remote internally).
        let dir = tempfile::tempdir().unwrap();
        let handle = DrivenHandleBuilder::new(dir.path().join("ignored.db"))
            .boot()
            .await
            .expect("default handle");

        let outcome = scenario.run_assertions(&handle).await.expect("run");
        scenario.teardown(&mut ctx).await.expect("teardown");

        assert_eq!(
            outcome.final_drive_object_count, 3,
            "exactly the 3 readable files uploaded: {:?}",
            outcome.notes
        );
        assert!(
            outcome.error_codes_seen.contains(&ErrorCode::LocalIoError),
            "the mode-000 file must surface local.io_error: {:?}",
            outcome.notes
        );
        assert!(
            outcome.final_hash_matches_local,
            "shared invariants must hold: {:?}",
            outcome.notes
        );
    }

    /// On a Unix host the setuid row uploads both files as plain content with
    /// no error and clean invariants - the documented V1 behaviour.
    #[cfg(unix)]
    #[tokio::test]
    async fn setuid_uploads_as_plain_content() {
        let scenario = SetuidFiles::new();
        let mut ctx = ScenarioContext::default();
        scenario.setup(&mut ctx).await.expect("setup");

        let dir = tempfile::tempdir().unwrap();
        let handle = DrivenHandleBuilder::new(dir.path().join("ignored.db"))
            .boot()
            .await
            .expect("default handle");

        let outcome = scenario.run_assertions(&handle).await.expect("run");
        scenario.teardown(&mut ctx).await.expect("teardown");

        assert_eq!(
            outcome.final_drive_object_count, 2,
            "both files upload as plain content: {:?}",
            outcome.notes
        );
        assert!(
            outcome.error_codes_seen.is_empty(),
            "setuid upload is clean (no error code): {:?}",
            outcome.notes
        );
        assert!(
            outcome.final_hash_matches_local,
            "invariants hold: {:?}",
            outcome.notes
        );
    }
}
