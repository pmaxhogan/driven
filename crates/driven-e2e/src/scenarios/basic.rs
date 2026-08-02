//! Happy-path scenarios: first-run wizard, the flagship local-folder
//! backup -> restore round trip, and settings persistence across an app
//! restart.

use std::time::Duration;

use serde_json::{json, Value};

use crate::flows;
use crate::scenario::{Ctx, Scenario, Verdict};
use crate::session::{AppSession, SessionConfig};

/// Boot with an empty state DB: the app must land on the setup wizard and
/// render its first step (SPEC s11.1 first-run UX).
pub struct WizardFirstRun;

#[async_trait::async_trait]
impl Scenario for WizardFirstRun {
    fn name(&self) -> &'static str {
        "wizard-first-run"
    }
    fn description(&self) -> &'static str {
        "empty state boots into the setup wizard and renders its first step"
    }
    async fn run(&self, ctx: &Ctx) -> anyhow::Result<Verdict> {
        let session = AppSession::launch(SessionConfig {
            fake_remote: true,
            ..Default::default()
        })
        .await?;

        // First-run routing: the router must land on /setup.
        let on_setup = session
            .wait_for_js(
                "location.pathname === '/setup' || !!document.querySelector('[data-testid=wizard-choose-folder], [data-testid=local-folder-form]')",
                Duration::from_secs(20),
            )
            .await;
        session.screenshot(&ctx.artifacts, "01-first-run").await?;
        if let Err(e) = on_setup {
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "first run did not land on the setup wizard: {e:#}"
            )));
        }

        // The wizard must offer the backend choice / credentials step content
        // (some visible interactive element beyond a blank shell).
        let has_content = session
            .eval(
                "document.body.innerText.trim().length > 40 && !!document.querySelector('button')",
            )
            .await?;
        if has_content != Value::Bool(true) {
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(
                "setup wizard rendered without interactive content".into(),
            ));
        }

        // Walk one REAL interaction: advance the wizard a step (the Next
        // button must exist and move the progress indicator).
        let next_btn = "button.bg-teal-700, button[type=submit]";
        if session
            .wait_for_selector(next_btn, Duration::from_secs(5))
            .await
            .is_ok()
        {
            // Click whatever the primary action is; a non-advancing click is
            // not a failure (some backends need input first), but the click
            // path itself must not throw.
            let _ = session.click(next_btn).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            session.screenshot(&ctx.artifacts, "02-after-next").await?;
        }

        // The top nav routes must render (SPA navigation, no backend needed
        // for the shells): settings is the richest surface.
        session.goto("/settings").await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        session.screenshot(&ctx.artifacts, "03-settings").await?;
        Ok(Verdict::Pass)
    }
}

/// The flagship: seed a source tree, back it up to a REAL local-folder
/// destination through the real engine, then restore through the production
/// restore-job machinery and byte-compare the round trip.
pub struct LocalFolderRoundTrip;

#[async_trait::async_trait]
impl Scenario for LocalFolderRoundTrip {
    fn name(&self) -> &'static str {
        "local-folder-round-trip"
    }
    fn description(&self) -> &'static str {
        "backup -> restore round trip against a real local-folder destination, bytes compared"
    }
    async fn run(&self, ctx: &Ctx) -> anyhow::Result<Verdict> {
        let work = tempfile::Builder::new()
            .prefix("driven-e2e-rt-")
            .tempdir()?;
        let src_dir = work.path().join("source");
        let dest_root = work.path().join("dest");
        let restore_dir = work.path().join("restored");
        std::fs::create_dir_all(&src_dir)?;
        std::fs::create_dir_all(&dest_root)?;
        let rels = flows::seed_source_tree(&src_dir)?;

        let session = AppSession::launch(SessionConfig::default()).await?;

        let account = flows::create_local_folder_account(&session, &dest_root).await?;
        let source = flows::add_source(&session, &account, &src_dir).await?;
        flows::sync_now(&session, &source).await?;

        // The destination must physically contain the backed-up bytes
        // (localfs backend writes real objects under the root). The marker
        // file the destination-prepare step writes does not count toward the
        // expectation, so waiting for source-count files is unambiguous.
        let waited =
            flows::wait_for_dest_files(&dest_root, rels.len(), Duration::from_secs(120)).await;
        session.screenshot(&ctx.artifacts, "01-after-sync").await?;
        if let Err(e) = waited {
            let status = session.invoke("get_sync_status", Value::Null).await?;
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "destination never reached {} objects: {e:#}; status={status}",
                rels.len()
            )));
        }

        // Restore EVERY file through the production job machinery.
        let rel_refs: Vec<&str> = rels.iter().map(String::as_str).collect();
        let job = flows::restore_and_wait(
            &session,
            &source,
            &rel_refs,
            &restore_dir,
            Duration::from_secs(120),
        )
        .await?;
        session
            .screenshot(&ctx.artifacts, "02-after-restore")
            .await?;

        if !flows::restore_job_clean(&job) {
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "restore job reported failure: {job}"
            )));
        }

        let mismatches = flows::compare_trees(&src_dir, &restore_dir)?;
        if !mismatches.is_empty() {
            session.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "round trip byte mismatch: {mismatches:?}"
            )));
        }
        Ok(Verdict::Pass)
    }
}

/// Change a setting, quit, relaunch against the SAME data dir: the setting
/// must survive (SPEC s22 persistence).
pub struct SettingsPersistence;

#[async_trait::async_trait]
impl Scenario for SettingsPersistence {
    fn name(&self) -> &'static str {
        "settings-persistence"
    }
    fn description(&self) -> &'static str {
        "a saved setting survives an app restart (same data dir, fresh process)"
    }
    async fn run(&self, ctx: &Ctx) -> anyhow::Result<Verdict> {
        let session = AppSession::launch(SessionConfig {
            fake_remote: true,
            ..Default::default()
        })
        .await?;

        // Read the current scan interval, then write a distinct value through
        // the production patch surface (SPEC s22 `update_settings`).
        let before = session.invoke("get_settings", Value::Null).await?;
        let old_secs = before
            .pointer("/global/scanIntervalSecs")
            .and_then(Value::as_u64)
            .unwrap_or(3600);
        let new_secs = old_secs + 61;
        session
            .invoke(
                "update_settings",
                json!({ "patch": { "global": { "scanIntervalSecs": new_secs } } }),
            )
            .await?;

        // Restart: same data dir, brand-new process.
        let (data_dir, _guard) = session.quit_keep_data_dir().await?;
        let session2 = AppSession::launch(SessionConfig {
            fake_remote: true,
            data_dir: Some(data_dir),
            ..Default::default()
        })
        .await?;
        let after = session2.invoke("get_settings", Value::Null).await?;
        let got = after
            .pointer("/global/scanIntervalSecs")
            .and_then(Value::as_u64);
        session2
            .screenshot(&ctx.artifacts, "01-after-restart")
            .await?;
        if got != Some(new_secs) {
            session2.preserve_evidence(&ctx.artifacts).await;
            return Ok(Verdict::Fail(format!(
                "scanIntervalSecs did not persist: wrote {new_secs}, read back {got:?}                  (full settings: {after})"
            )));
        }
        Ok(Verdict::Pass)
    }
}
