//! Fault scenarios: the app must SURFACE failures honestly (status + activity)
//! and keep unaffected work flowing - asserted through the same UI/IPC surface
//! a user sees, with the fault injected from OUTSIDE the process.

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use serde_json::{json, Value};

use crate::flows;
use crate::scenario::{Ctx, Scenario, Verdict};
use crate::session::{AppSession, SessionConfig};

/// Fake-remote total outage (armed via the DRIVEN_TEST_FAULT_PLAN seam): a
/// sync against a dead remote must surface an error state - never report
/// success - and the activity surface must carry the failure.
pub struct FakeDriveOutageSurfaced;

#[async_trait::async_trait]
impl Scenario for FakeDriveOutageSurfaced {
    fn name(&self) -> &'static str {
        "fake-drive-outage-surfaced"
    }
    fn description(&self) -> &'static str {
        "a dead remote (fault plan: every request drops) surfaces as error state, not success"
    }
    async fn run(&self, ctx: &Ctx) -> anyhow::Result<Verdict> {
        let work = tempfile::Builder::new()
            .prefix("driven-e2e-outage-")
            .tempdir()?;
        let src_dir = work.path().join("source");
        std::fs::create_dir_all(&src_dir)?;
        flows::seed_source_tree(&src_dir)?;

        let session = AppSession::launch(SessionConfig {
            fake_remote: true,
            fault_plan_json: Some(r#"{"network_drop_every_request": true}"#.to_string()),
            ..Default::default()
        })
        .await?;

        // With DRIVEN_USE_FAKE_REMOTE=1 EVERY account's orchestrator gets the
        // in-memory fake store (assembly's global remote mode), so the cheapest
        // OAuth-free account shape works: a local-folder account whose real
        // destination dir is never touched. The armed fault plan applies to
        // the fake store minted for this account.
        let unused_dest = work.path().join("unused-dest");
        std::fs::create_dir_all(&unused_dest)?;
        let account = flows::create_local_folder_account(&session, &unused_dest).await?;
        // Destination folder: browse the FAKE store through the production
        // picker (its root id is store-minted, not the "root" sentinel).
        let listing = session
            .invoke(
                "pick_drive_folder",
                json!({ "accountId": account, "startFolderId": null, "driveId": null }),
            )
            .await;
        // With every request dropping, even the picker may fail - that is
        // itself proof the fault plan armed; fall back to the sentinel so
        // add_source still goes through (its validation does not hit the
        // remote).
        let folder_id = listing
            .ok()
            .and_then(|l| {
                l.get("currentFolderId")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .unwrap_or_else(|| "root".to_string());
        let source = flows::add_source_to_folder(&session, &account, &src_dir, &folder_id).await?;

        // Trigger a sync; every remote request drops. The app must NOT report
        // the source synced - the failure must surface on the activity/error
        // surface (SPEC s11.4) within the retry window.
        flows::sync_now(&session, &source).await?;
        let surfaced = flows::wait_for_activity(&session, Duration::from_secs(60), "error").await;
        let status = session.invoke("get_sync_status", Value::Null).await?;
        session
            .screenshot(&ctx.artifacts, "01-outage-status")
            .await?;
        // PROVE the fault fired: zero error signal anywhere = fake-green.
        let txt = format!("{status}").to_lowercase();
        if surfaced.is_err() && !(txt.contains("error") || txt.contains("backoff")) {
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "dead remote but no error surfaced on activity or status: {status}"
            )));
        }
        Ok(Verdict::Pass)
    }
}

/// One unreadable source file (mode 000) must fail THAT file with a truthful
/// activity/error surface while the rest of the tree still syncs.
pub struct SourceFileUnreadable;

#[async_trait::async_trait]
impl Scenario for SourceFileUnreadable {
    fn name(&self) -> &'static str {
        "source-file-unreadable"
    }
    fn description(&self) -> &'static str {
        "an EACCES source file fails alone; the rest of the tree syncs and the error is surfaced"
    }
    async fn run(&self, ctx: &Ctx) -> anyhow::Result<Verdict> {
        // Root bypasses POSIX modes; the container runs the suite as the
        // non-root `driven` user, but guard anyway (mirrors driven-chaos).
        if nix_is_root() {
            return Ok(Verdict::Skip(
                "running as root: chmod 000 does not deny".into(),
            ));
        }
        let work = tempfile::Builder::new()
            .prefix("driven-e2e-eacces-")
            .tempdir()?;
        let src_dir = work.path().join("source");
        let dest_root = work.path().join("dest");
        std::fs::create_dir_all(&src_dir)?;
        std::fs::create_dir_all(&dest_root)?;
        let rels = flows::seed_source_tree(&src_dir)?;
        // Deny read on one file.
        let denied = src_dir.join("hello.txt");
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000))?;

        let session = AppSession::launch(SessionConfig::default()).await?;
        let account = flows::create_local_folder_account(&session, &dest_root).await?;
        let source = flows::add_source(&session, &account, &src_dir).await?;
        flows::sync_now(&session, &source).await?;

        // The OTHER files must land despite the denied one.
        let waited =
            flows::wait_for_dest_files(&dest_root, rels.len() - 1, Duration::from_secs(120)).await;
        session.screenshot(&ctx.artifacts, "01-after-sync").await?;
        if let Err(e) = waited {
            let _ = std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o644));
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "expected {} synced objects despite one EACCES file: {e:#}",
                rels.len() - 1
            )));
        }

        // The failure must be SURFACED on the production activity IPC.
        let surfaced =
            flows::wait_for_activity(&session, Duration::from_secs(60), "hello.txt").await;
        // Restore access so tempdir cleanup works either way.
        let _ = std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o644));
        if let Err(e) = surfaced {
            let page = flows::activity_page(&session).await.unwrap_or(Value::Null);
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "EACCES failure never surfaced on the activity feed: {e:#}; page={page}"
            )));
        }
        Ok(Verdict::Pass)
    }
}

/// Destination on a tiny tmpfs: filling it mid-backup must surface a disk-full
/// class error, not a silent success. SKIPS unless the runner provides the
/// mount (docker run --tmpfs /e2e-small-dest:rw,size=1m).
pub struct DestDiskFull;

#[async_trait::async_trait]
impl Scenario for DestDiskFull {
    fn name(&self) -> &'static str {
        "dest-disk-full"
    }
    fn description(&self) -> &'static str {
        "a full destination filesystem surfaces a disk-full error instead of fake success"
    }
    async fn run(&self, ctx: &Ctx) -> anyhow::Result<Verdict> {
        let small = std::path::Path::new("/e2e-small-dest");
        if !small.is_dir() {
            return Ok(Verdict::Skip(
                "no /e2e-small-dest tmpfs mount (run with --tmpfs /e2e-small-dest:rw,size=1m)"
                    .into(),
            ));
        }
        let work = tempfile::Builder::new()
            .prefix("driven-e2e-full-")
            .tempdir()?;
        let src_dir = work.path().join("source");
        std::fs::create_dir_all(&src_dir)?;
        // 4 MiB of source into a 1 MiB destination: must not fit.
        let big: Vec<u8> = (0u8..=255).cycle().take(4 * 1024 * 1024).collect();
        std::fs::write(src_dir.join("big.bin"), &big)?;
        let dest_root = small.join(format!("dest-{}", uuid_ish()));
        std::fs::create_dir_all(&dest_root)?;

        let session = AppSession::launch(SessionConfig::default()).await?;
        let account = flows::create_local_folder_account(&session, &dest_root).await?;
        let source = flows::add_source(&session, &account, &src_dir).await?;
        flows::sync_now(&session, &source).await?;

        // 4 MiB cannot land in a 1 MiB tmpfs: the failure must surface on
        // the activity feed (level error / a storage-class code) - the
        // orchestrator legitimately returns to idle between retries, so the
        // status DTO alone is not the assertion surface.
        let surfaced = flows::wait_for_activity(&session, Duration::from_secs(90), "error").await;
        let status = session.invoke("get_sync_status", Value::Null).await?;
        session.screenshot(&ctx.artifacts, "01-disk-full").await?;
        // Cleanup so later scenarios reusing the tmpfs are unaffected.
        let _ = std::fs::remove_dir_all(&dest_root);
        if let Err(e) = surfaced {
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "4 MiB into a 1 MiB destination surfaced no error on activity: {e:#};                  status={status}"
            )));
        }
        Ok(Verdict::Pass)
    }
}

/// `true` when the process runs as uid 0.
fn nix_is_root() -> bool {
    // SAFETY: geteuid has no failure modes.
    unsafe { libc_geteuid() == 0 }
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

/// A cheap unique suffix without a uuid dependency.
fn uuid_ish() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
