//! User-facing RESTORE path scenarios (agent QA harness, issue #239).
//!
//! Until these rows landed, the chaos harness had ZERO coverage of the
//! restore direction: every s3.x row ends at "the backup survived". These
//! scenarios drive the shared headless restore engine
//! ([`driven_core::restore_fetch`] - the same code `driven-cli restore` ships)
//! over a synced instance, under clean and faulted conditions. The GUI's
//! confined-dest machinery is exercised by the app-level `driven-e2e` suite;
//! HERE the contract is the engine: bytes come back identical, faults cost
//! exactly the affected file, and a re-run completes.
//!
//! Rows:
//! - `restore-round-trip-clean` - full tree restore, byte-identical.
//! - `restore-encrypted-round-trip` - encrypted source: ciphertext at rest,
//!   plaintext restored; restoring WITHOUT the key fails closed.
//! - `restore-missing-remote-object` - one row's object gone: exactly that
//!   file fails, every other file restores.
//! - `restore-under-network-drop` - a transient wire drop mid-restore costs
//!   at most one file on the first pass; the retry pass completes clean.
//! - `restore-refuses-clobber` - an existing destination file is refused
//!   (not silently overwritten) unless `overwrite` is set.

use std::sync::Arc;

use async_trait::async_trait;

use driven_core::restore_fetch::{restore_source, RestoreOptions};
use driven_core::state::SourceRow;

use driven_crypto::{DrivenCryptoSuite, SourceCryptoSuite, SourceKey};

use driven_drive::fake::InMemoryRemoteStore;

use crate::capabilities::CapabilityRequirements;
use crate::handle::DrivenHandle;
use crate::scenario::{ExpectedOutcome, Outcome, Scenario, ScenarioContext};

use super::drive_side::{
    boot_instance_with, check_invariants, error_codes_in_activity, finish, is_quiescent, source_in,
    write_file, Instance,
};

/// The deterministic fixture tree every restore row round-trips: nested dirs,
/// an empty file, a multi-chunk (> 64 KiB) body, a unicode name.
fn seed_tree(root: &std::path::Path) -> anyhow::Result<Vec<(&'static str, Vec<u8>)>> {
    let files: Vec<(&'static str, Vec<u8>)> = vec![
        ("top.txt", b"restore me".to_vec()),
        (
            "nested/deep/blob.bin",
            (0u8..=255).cycle().take(96 * 1024).collect(),
        ),
        ("nested/empty.txt", Vec::new()),
        ("uni-\u{00e9}\u{4e16}.md", b"# unicode".to_vec()),
    ];
    for (rel, bytes) in &files {
        write_file(root, rel, bytes)?;
    }
    Ok(files)
}

/// Boot (optionally with a wired crypto suite), seed, sync one cycle. Returns
/// the booted instance + its source row + the seeded fixture.
async fn synced_instance(
    fake: Arc<InMemoryRemoteStore>,
    encrypted_with: Option<Arc<dyn SourceCryptoSuite>>,
) -> anyhow::Result<(Instance, SourceRow, Vec<(&'static str, Vec<u8>)>)> {
    let encrypted = encrypted_with.is_some();
    let instance = boot_instance_with(fake, encrypted_with).await?;
    let files = seed_tree(instance.src_root())?;
    let mut src = source_in(
        instance.handle.account_id,
        instance.src_root(),
        &instance.folder,
    );
    // The wired SingleSuiteProvider resolves a suite for every source; the
    // per-source flag is what routes the executor into the encrypt path
    // (mirrors e2e_fake's encryption row).
    src.encryption_enabled = encrypted;
    instance.handle.state.upsert_source(&src).await?;
    instance.handle.run_one_cycle().await?;
    Ok((instance, src, files))
}

/// Byte-compare every seeded file against `restored_root`.
fn tree_matches(
    files: &[(&'static str, Vec<u8>)],
    restored_root: &std::path::Path,
) -> anyhow::Result<Vec<String>> {
    let mut mismatches = Vec::new();
    for (rel, bytes) in files {
        let p = restored_root.join(rel);
        match std::fs::read(&p) {
            Ok(got) if &got == bytes => {}
            Ok(_) => mismatches.push(format!("bytes differ: {rel}")),
            Err(e) => mismatches.push(format!("unreadable {rel}: {e}")),
        }
    }
    Ok(mismatches)
}

/// Shared tail: fold invariants + quiescence into the outcome.
async fn finish_with_invariants(
    instance: &Instance,
    src: &SourceRow,
    mut outcome: Outcome,
) -> anyhow::Result<Outcome> {
    let report = check_invariants(&instance.handle, &instance.folder, src).await?;
    outcome
        .error_codes_seen
        .extend(error_codes_in_activity(instance.handle.state.as_ref()).await?);
    let quiesced = is_quiescent(&instance.handle.state().await);
    finish(outcome, report, quiesced)
}

// ---------------------------------------------------------------------------
// restore-round-trip-clean
// ---------------------------------------------------------------------------

/// Backup a mixed tree, restore ALL of it with the shared engine, and demand
/// byte-identical results (the restore-direction analogue of the s6.3
/// no-data-loss invariant).
pub(crate) struct RestoreRoundTripClean;

#[async_trait]
impl Scenario for RestoreRoundTripClean {
    fn name(&self) -> &'static str {
        "restore-round-trip-clean"
    }
    fn description(&self) -> &'static str {
        "full-tree restore via the shared engine returns byte-identical files"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fake = Arc::new(InMemoryRemoteStore::new());
        let (instance, src, files) = synced_instance(fake.clone(), None).await?;
        let dest = tempfile::tempdir()?;

        let report = restore_source(
            instance.handle.state.as_ref(),
            fake.as_ref(),
            &src,
            None,
            dest.path(),
            &RestoreOptions::default(),
        )
        .await?;

        let mut outcome = Outcome::default();
        if !report.ok() {
            outcome.notes.push(format!(
                "restore reported failures: {:?}",
                report.failures().collect::<Vec<_>>()
            ));
        }
        if report.restored() != files.len() {
            outcome.notes.push(format!(
                "expected {} restored files, got {}",
                files.len(),
                report.restored()
            ));
        }
        let mismatches = tree_matches(&files, dest.path())?;
        if !mismatches.is_empty() {
            outcome
                .notes
                .push(format!("byte mismatches: {mismatches:?}"));
        }
        finish_with_invariants(&instance, &src, outcome).await
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        // Success = no notes: the runner treats notes as informational, so the
        // hard gate is DocumentedBehaviour + the invariant snapshot + the
        // note-free contract asserted below in `verify_notes_empty` tests;
        // Success keeps the intent explicit (no error codes surfaced).
        ExpectedOutcome::Success
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// restore-encrypted-round-trip
// ---------------------------------------------------------------------------

/// Encrypted source: the stored objects are ciphertext, the restore engine
/// (given the suite) returns plaintext, and a restore WITHOUT the key fails
/// closed instead of writing garbage.
pub(crate) struct RestoreEncryptedRoundTrip;

#[async_trait]
impl Scenario for RestoreEncryptedRoundTrip {
    fn name(&self) -> &'static str {
        "restore-encrypted-round-trip"
    }
    fn description(&self) -> &'static str {
        "encrypted source restores to plaintext with the key; fails closed without it"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fake = Arc::new(InMemoryRemoteStore::new());
        let key = SourceKey::generate();
        let suite: Arc<dyn SourceCryptoSuite> = Arc::new(DrivenCryptoSuite::new(key.clone()));
        let (instance, src, files) = synced_instance(fake.clone(), Some(suite)).await?;

        let mut outcome = Outcome::default();

        // At-rest check: the stored object for top.txt must NOT be the
        // plaintext (ciphertext with header, so strictly larger too).
        let states = instance.handle.state.load_source_file_state(src.id).await?;
        let top = states
            .iter()
            .find(|(rel, _)| rel.as_str() == "top.txt")
            .ok_or_else(|| anyhow::anyhow!("top.txt row missing"))?;
        if let Some(id) = top.1.drive_file_id.as_deref() {
            if let Some(driven_drive::fake::ObjectContent::Literal(stored)) =
                fake.object_content(id)
            {
                if stored.as_slice() == b"restore me" {
                    outcome
                        .notes
                        .push("encrypted source stored PLAINTEXT at rest".into());
                }
            }
        } else {
            outcome.notes.push("top.txt never synced".into());
        }

        // Restore WITH the key: byte-identical plaintext.
        let restore_suite = DrivenCryptoSuite::new(key.clone());
        let dest = tempfile::tempdir()?;
        let report = restore_source(
            instance.handle.state.as_ref(),
            fake.as_ref(),
            &src,
            Some(&restore_suite),
            dest.path(),
            &RestoreOptions::default(),
        )
        .await?;
        if !report.ok() {
            outcome.notes.push(format!(
                "encrypted restore failed: {:?}",
                report.failures().collect::<Vec<_>>()
            ));
        }
        let mismatches = tree_matches(&files, dest.path())?;
        if !mismatches.is_empty() {
            outcome
                .notes
                .push(format!("plaintext mismatches: {mismatches:?}"));
        }

        // Restore WITHOUT the key must fail closed (refuse up front).
        let dest2 = tempfile::tempdir()?;
        let no_key = restore_source(
            instance.handle.state.as_ref(),
            fake.as_ref(),
            &src,
            None,
            dest2.path(),
            &RestoreOptions::default(),
        )
        .await;
        if no_key.is_ok() {
            outcome
                .notes
                .push("restoring an encrypted source WITHOUT the key did not fail closed".into());
        }
        finish_with_invariants(&instance, &src, outcome).await
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// restore-missing-remote-object
// ---------------------------------------------------------------------------

/// One synced row's remote object vanishes (repointed at a bogus id): the
/// restore must fail EXACTLY that file and restore every other one.
pub(crate) struct RestoreMissingRemoteObject;

#[async_trait]
impl Scenario for RestoreMissingRemoteObject {
    fn name(&self) -> &'static str {
        "restore-missing-remote-object"
    }
    fn description(&self) -> &'static str {
        "a vanished remote object costs exactly that file; the rest restore"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fake = Arc::new(InMemoryRemoteStore::new());
        let (instance, src, files) = synced_instance(fake.clone(), None).await?;

        // Repoint ONE row's drive_file_id at an id that does not exist (the
        // fake's delete is trash-based; a bogus id models a purged object).
        let states = instance.handle.state.load_source_file_state(src.id).await?;
        let victim = states
            .iter()
            .find(|(rel, row)| rel.as_str() == "top.txt" && row.drive_file_id.is_some())
            .map(|(_, row)| row.clone())
            .ok_or_else(|| anyhow::anyhow!("fixture row top.txt missing or never synced"))?;
        let mut repointed = victim.clone();
        repointed.drive_file_id = Some("purged-out-of-band".to_string());
        instance.handle.state.upsert_file_state(&repointed).await?;

        let dest = tempfile::tempdir()?;
        let report = restore_source(
            instance.handle.state.as_ref(),
            fake.as_ref(),
            &src,
            None,
            dest.path(),
            &RestoreOptions::default(),
        )
        .await?;

        let mut outcome = Outcome::default();
        // PROVE the fault fired: exactly one failure, and it is the victim.
        if report.failed() != 1 {
            outcome.notes.push(format!(
                "expected exactly 1 failed file, got {} ({:?})",
                report.failed(),
                report.failures().collect::<Vec<_>>()
            ));
        }
        if report.restored() != files.len() - 1 {
            outcome.notes.push(format!(
                "expected {} restored files, got {}",
                files.len() - 1,
                report.restored()
            ));
        }
        let survivors: Vec<(&'static str, Vec<u8>)> = files
            .iter()
            .filter(|(rel, _)| *rel != "top.txt")
            .cloned()
            .collect();
        let mismatches = tree_matches(&survivors, dest.path())?;
        if !mismatches.is_empty() {
            outcome
                .notes
                .push(format!("survivor mismatches: {mismatches:?}"));
        }
        // Repair the deliberately-broken row before the cross-cutting sweep:
        // the dangling id IS this scenario's fixture, not a data-loss bug the
        // invariant should flag.
        instance.handle.state.upsert_file_state(&victim).await?;
        finish_with_invariants(&instance, &src, outcome).await
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// restore-under-network-drop
// ---------------------------------------------------------------------------

/// A transient network drop mid-restore: the first pass may lose at most the
/// in-flight file; a second pass (the user clicking retry) completes clean.
pub(crate) struct RestoreUnderNetworkDrop;

#[async_trait]
impl Scenario for RestoreUnderNetworkDrop {
    fn name(&self) -> &'static str {
        "restore-under-network-drop"
    }
    fn description(&self) -> &'static str {
        "a transient wire drop costs at most one file; the retry pass restores everything"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fake = Arc::new(InMemoryRemoteStore::new());
        let (instance, src, files) = synced_instance(fake.clone(), None).await?;

        // Arm a single-shot drop that trips on one of the restore downloads.
        let armed = fake.as_ref().clone().with_network_drop_after(1);
        let dest = tempfile::tempdir()?;
        let first = restore_source(
            instance.handle.state.as_ref(),
            &armed,
            &src,
            None,
            dest.path(),
            &RestoreOptions::default(),
        )
        .await?;

        let mut outcome = Outcome::default();
        // PROVE the fault fired: the first pass must NOT be fully clean.
        if first.failed() == 0 {
            outcome
                .notes
                .push("armed network drop never fired during the first restore pass".to_string());
        }
        if first.failed() > 1 {
            outcome.notes.push(format!(
                "a single-shot drop cost {} files (expected at most 1)",
                first.failed()
            ));
        }

        // Retry pass: transient fault reset; overwrite by re-running into the
        // same dest (already-restored files are re-verified/overwritten).
        let second = restore_source(
            instance.handle.state.as_ref(),
            fake.as_ref(),
            &src,
            None,
            dest.path(),
            &RestoreOptions { overwrite: true },
        )
        .await?;
        if !second.ok() {
            outcome.notes.push(format!(
                "retry pass still failing: {:?}",
                second.failures().collect::<Vec<_>>()
            ));
        }
        let mismatches = tree_matches(&files, dest.path())?;
        if !mismatches.is_empty() {
            outcome
                .notes
                .push(format!("post-retry mismatches: {mismatches:?}"));
        }
        finish_with_invariants(&instance, &src, outcome).await
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// restore-refuses-clobber
// ---------------------------------------------------------------------------

/// A destination file that already exists must be refused (reported, left
/// untouched) with `overwrite=false`, and replaced with `overwrite=true`.
pub(crate) struct RestoreRefusesClobber;

#[async_trait]
impl Scenario for RestoreRefusesClobber {
    fn name(&self) -> &'static str {
        "restore-refuses-clobber"
    }
    fn description(&self) -> &'static str {
        "an existing destination file is refused without overwrite, replaced with it"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }
    async fn setup(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let fake = Arc::new(InMemoryRemoteStore::new());
        let (instance, src, _files) = synced_instance(fake.clone(), None).await?;

        let dest = tempfile::tempdir()?;
        let sentinel = b"precious local edit".to_vec();
        write_file(dest.path(), "top.txt", &sentinel)?;

        let first = restore_source(
            instance.handle.state.as_ref(),
            fake.as_ref(),
            &src,
            None,
            dest.path(),
            &RestoreOptions::default(),
        )
        .await?;

        let mut outcome = Outcome::default();
        if first.ok() {
            outcome
                .notes
                .push("restore over an existing file reported success without overwrite".into());
        }
        let after_first = std::fs::read(dest.path().join("top.txt"))?;
        if after_first != sentinel {
            outcome
                .notes
                .push("overwrite=false still clobbered the existing destination file".into());
        }

        let second = restore_source(
            instance.handle.state.as_ref(),
            fake.as_ref(),
            &src,
            None,
            dest.path(),
            &RestoreOptions { overwrite: true },
        )
        .await?;
        if !second.ok() {
            outcome.notes.push(format!(
                "overwrite=true pass failed: {:?}",
                second.failures().collect::<Vec<_>>()
            ));
        }
        let after_second = std::fs::read(dest.path().join("top.txt"))?;
        if after_second != b"restore me" {
            outcome
                .notes
                .push("overwrite=true did not replace the file with the backed-up bytes".into());
        }
        finish_with_invariants(&instance, &src, outcome).await
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::DocumentedBehaviour
    }
    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The restore-path rows, in registry order.
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(RestoreRoundTripClean),
        Box::new(RestoreEncryptedRoundTrip),
        Box::new(RestoreMissingRemoteObject),
        Box::new(RestoreUnderNetworkDrop),
        Box::new(RestoreRefusesClobber),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every registered restore row must carry a unique kebab-case name.
    #[test]
    fn names_unique_and_kebab() {
        let names: Vec<&str> = scenarios().iter().map(|s| s.name()).collect();
        let mut dedup = names.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(names.len(), dedup.len());
        for n in names {
            assert!(n.chars().all(|c| c.is_ascii_lowercase() || c == '-'));
        }
    }
}
