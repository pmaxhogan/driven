//! File-size-extreme scenarios (STRESS_HARNESS s3.2).
//!
//! Every row of the s3.2 catalogue plus the two extra extremes the M3.7
//! brief names explicitly (`0-byte`, `sparse`) that the table folds into
//! prose: `zero-byte-file`, `sparse-file-zeros`, `huge-file-10gb`,
//! `huge-file-50gb-mid-run-crash`, `tiny-files-100k-in-one-dir`,
//! `million-files-nested`.
//!
//! Each scenario drives the HEADLESS core through a [`DrivenHandle`] against
//! the [`InMemoryRemoteStore`] (plus the s5 fault-injection builders where a
//! Drive-side fault is part of the row) over a real temp dir, then asserts:
//!
//! 1. the s6.3 cross-scenario invariants this crate's driver also re-checks
//!    (no data loss, no duplicate remote object, clean shutdown), and
//! 2. the row's own expected outcome (a clean `Success`, or the
//!    documented-behaviour crash-resume path).
//!
//! The big-disk rows (`huge-file-10gb`, `huge-file-50gb-mid-run-crash`,
//! `tiny-files-100k-in-one-dir`, `million-files-nested`) gate on
//! [`Capability::FreeDiskBytes`]; on a host without the headroom they are
//! SKIPPED with the missing capability recorded (STRESS_HARNESS s2.5 / s8),
//! never faked or weakened.
//!
//! ## Driving contract (a surfaced interface finding)
//!
//! The Phase-1 [`Scenario`] trait hands `run_assertions` only a
//! `&DrivenHandle`, with no [`ScenarioContext`] - and [`DrivenHandle`]
//! exposes no fixture-root accessor and no way to recover the concrete
//! fake from its `Arc<dyn RemoteStore>`. A file-size scenario nonetheless
//! needs (a) the source path it wrote in `setup`, (b) the concrete
//! [`InMemoryRemoteStore`] to inspect final remote state, and (c) - for the
//! crash row - a *fault-injected* fake the default boot cannot provide.
//!
//! So each scenario carries its own per-run state in a [`OnceLock`] set in
//! `setup` and boots the handle it asserts against itself, against that same
//! fixture. The `&DrivenHandle` the trait passes is therefore unused by this
//! category; it remains in the signature because the trait fixes it. This is
//! reported to the Integrate agent as the one place the s2.4 sketch and the
//! file-size rows diverge.
//!
//! ## Hashing
//!
//! The small / sparse rows do their data-loss check (`final_hash_matches_local`)
//! by re-reading the bytes Drive actually stored and comparing them to the
//! local file content directly - a dependency-free equivalent of "the recorded
//! hash still matches", streamed in fixed windows so it never buffers the whole
//! file. The huge-file rows (P1-B) instead arm the fake's content oracle (which
//! records only a length + md5, never the bytes) and verify length + md5
//! against the deterministic source pattern via [`deterministic_md5`], so a
//! 10-50 GB upload is proven without buffering or downloading it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use driven_core::state::SourceRow;
use driven_core::types::{AccountId, FileStateStatus, SourceId};

use driven_drive::fake::{InMemoryRemoteStore, CLIENT_OP_UUID_KEY};
use driven_drive::remote_store::RemoteStore;

use crate::capabilities::{Capability, CapabilityRequirements};
use crate::handle::{DrivenHandle, DrivenHandleBuilder};
use crate::scenario::{ExpectedOutcome, Outcome, Scenario, ScenarioContext};

/// 20 GiB of headroom for the deterministic 10 GB file (STRESS_HARNESS s3.2).
const TEN_GB_FREE: u64 = 20 * 1024 * 1024 * 1024;
/// 60 GiB of headroom for the 50 GB crash-resume file (STRESS_HARNESS s3.2).
const FIFTY_GB_FREE: u64 = 60 * 1024 * 1024 * 1024;
/// 1 GiB of headroom for the 100k tiny-file row (STRESS_HARNESS s3.2).
const TINY_FILES_FREE: u64 = 1024 * 1024 * 1024;
/// 8 GiB of headroom for the million-file nested tree (STRESS_HARNESS s3.2).
const MILLION_FILES_FREE: u64 = 8 * 1024 * 1024 * 1024;

/// Every file-size-extreme scenario (STRESS_HARNESS s3.2).
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(ZeroByteFile::default()),
        Box::new(SparseFileZeros::default()),
        Box::new(HugeFile10Gb::default()),
        Box::new(HugeFile50GbMidRunCrash::default()),
        Box::new(TinyFiles100kInOneDir::default()),
        Box::new(MillionFilesNested::default()),
    ]
}

// ---------------------------------------------------------------------------
// Per-run state carried setup -> run_assertions (see the module-level note).
// ---------------------------------------------------------------------------

/// The fixture root a scenario wrote in `setup`, recovered in
/// `run_assertions`. `OnceLock` because the trait's methods take `&self`.
#[derive(Debug, Default)]
struct FixtureState {
    root: OnceLock<PathBuf>,
}

impl FixtureState {
    /// Record the fixture root in `setup`.
    fn set(&self, root: PathBuf) {
        // A second `setup` on the same instance (re-run) keeps the first
        // root; both point at the same on-disk fixture, so this is benign.
        let _ = self.root.set(root);
    }

    /// The fixture root, or an error if `setup` did not run first.
    fn root(&self) -> anyhow::Result<&PathBuf> {
        self.root
            .get()
            .ok_or_else(|| anyhow::anyhow!("run_assertions called before setup recorded a fixture"))
    }

    /// The `src` subdir where every file-size scenario materialises its tree.
    fn src(&self) -> anyhow::Result<PathBuf> {
        Ok(self.root()?.join("src"))
    }

    /// The hermetic state-db path under the fixture root.
    fn state_db(&self) -> anyhow::Result<PathBuf> {
        Ok(self.root()?.join("state.db"))
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Build a source rooted at `root` uploading into the fake's `folder_id`,
/// gitignore off and no include/exclude filters - the file-size rows test
/// size handling, not the exclude engine.
fn source_in(account: AccountId, root: &Path, folder_id: &str) -> SourceRow {
    SourceRow {
        id: SourceId::new_v4(),
        account_id: account,
        display_name: "file-size".into(),
        enabled: true,
        local_path: root.to_string_lossy().into_owned(),
        drive_folder_id: folder_id.to_string(),
        drive_folder_path: "/file-size".into(),
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

/// Boot a handle over `state_db` wired to `remote`, register a source rooted
/// at `src_root`, and return the live handle plus the source row. The shared
/// entry point every file-size scenario uses to stand up the headless core.
async fn boot_and_register(
    state_db: PathBuf,
    remote: Arc<InMemoryRemoteStore>,
    src_root: &Path,
) -> anyhow::Result<(DrivenHandle, SourceRow)> {
    let handle = DrivenHandleBuilder::new(state_db)
        .remote(remote.clone())
        .boot()
        .await?;
    let folder = remote.root_id().to_string();
    let src = source_in(handle.account_id, src_root, &folder);
    handle.state.upsert_source(&src).await?;
    Ok((handle, src))
}

/// Count non-trashed objects under a folder of the fake.
async fn live_object_count(remote: &InMemoryRemoteStore, folder_id: &str) -> anyhow::Result<u64> {
    let entries = remote.list_folder(folder_id).await?;
    Ok(entries.iter().filter(|e| !e.trashed).count() as u64)
}

/// Assert no two live objects under `folder_id` share a `client_op_uuid`
/// (STRESS_HARNESS s6.3). Returns an error the scenario propagates so the
/// harness records a FAIL rather than a panic.
async fn assert_no_duplicate_op_uuid(
    remote: &InMemoryRemoteStore,
    folder_id: &str,
) -> anyhow::Result<()> {
    let entries = remote.list_folder(folder_id).await?;
    let mut seen: HashMap<String, String> = HashMap::new();
    for e in entries.iter().filter(|e| !e.trashed) {
        if let Some(uuid) = e.app_properties.get(CLIENT_OP_UUID_KEY) {
            if let Some(prev) = seen.insert(uuid.clone(), e.id.clone()) {
                anyhow::bail!(
                    "duplicate remote objects share client_op_uuid {uuid}: {prev} and {}",
                    e.id
                );
            }
        }
    }
    Ok(())
}

/// Stream a remote object and compare it byte-for-byte to the local file at
/// `local`, without buffering either in full. The dependency-free stand-in
/// for "the recorded hash still matches the bytes on Drive" (s6.3 no data
/// loss): if Drive holds exactly the local bytes, every hash trivially agrees.
async fn remote_matches_local(
    remote: &InMemoryRemoteStore,
    file_id: &str,
    local: &Path,
) -> anyhow::Result<bool> {
    const WINDOW: usize = 1024 * 1024;
    let mut rstream = remote.download(file_id).await?.0;
    let mut lfile = tokio::fs::File::open(local).await?;
    let mut rbuf = vec![0u8; WINDOW];
    let mut lbuf = vec![0u8; WINDOW];
    loop {
        let rn = read_fully(&mut rstream, &mut rbuf).await?;
        let ln = read_fully(&mut lfile, &mut lbuf).await?;
        if rn != ln {
            return Ok(false);
        }
        if rn == 0 {
            return Ok(true);
        }
        if rbuf[..rn] != lbuf[..ln] {
            return Ok(false);
        }
    }
}

/// Read up to `buf.len()` bytes, looping over short reads so a window
/// comparison is exact (an `AsyncRead` may return fewer bytes than asked
/// even when more remain).
async fn read_fully<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
) -> anyhow::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = reader.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Verify every `status='synced'` file_state row for `source` against the
/// bytes Drive stored, returning whether all matched (the per-scenario
/// data-loss check; the driver also runs the cross-cutting s6.3 version).
async fn synced_rows_match_remote(
    handle: &DrivenHandle,
    remote: &InMemoryRemoteStore,
    source: &SourceRow,
    src_root: &Path,
) -> anyhow::Result<bool> {
    let rows = handle.state.load_source_file_state(source.id).await?;
    for row in rows.values() {
        if row.status != FileStateStatus::Synced {
            continue;
        }
        let Some(file_id) = row.drive_file_id.as_deref() else {
            // A synced row with no drive id is itself a data-loss bug.
            return Ok(false);
        };
        let local = src_root.join(row.relative_path.as_str());
        if !remote_matches_local(remote, file_id, &local).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Write `len` deterministic bytes to `path`, streaming so even the 50 GB
/// row never holds the file in memory. The byte at logical offset `i` is
/// `(i % 251)` - a seeded, reproducible pattern (matching the e2e suite's
/// generator) so a re-run materialises byte-identical content and the
/// remote/local comparison is meaningful.
async fn write_deterministic(path: &Path, len: u64) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut f = tokio::fs::File::create(path).await?;
    // Cap the scratch block at the file length so the many-tiny-file rows
    // (million-files-nested, tiny-files-100k - mostly 0-4 KiB) do not allocate
    // a 1 MiB buffer per file; a huge file still streams in 1 MiB blocks.
    const MAX_BLOCK: usize = 1024 * 1024;
    let block_len = std::cmp::min(MAX_BLOCK as u64, len.max(1)) as usize;
    let mut block = vec![0u8; block_len];
    let mut written: u64 = 0;
    while written < len {
        let this = std::cmp::min(block_len as u64, len - written) as usize;
        for (j, slot) in block[..this].iter_mut().enumerate() {
            *slot = ((written as usize + j) % 251) as u8;
        }
        f.write_all(&block[..this]).await?;
        written += this as u64;
    }
    f.flush().await?;
    Ok(())
}

/// The md5 of the first `len` bytes of the [`write_deterministic`] pattern
/// (byte at offset `i` is `(i % 251)`), computed by STREAMING the generator
/// through the hasher - never buffering. This is the length+digest oracle the
/// huge-file rows (P1-B) verify against instead of downloading 10-50 GB: the
/// fake's content oracle records the md5 of the bytes it received, and that
/// must equal this digest of the bytes the source actually held.
fn deterministic_md5(len: u64) -> [u8; 16] {
    use md5::{Digest, Md5};
    const BLOCK: usize = 1024 * 1024;
    let mut hasher = Md5::new();
    let mut block = vec![0u8; BLOCK];
    let mut done: u64 = 0;
    while done < len {
        let this = std::cmp::min(BLOCK as u64, len - done) as usize;
        for (j, slot) in block[..this].iter_mut().enumerate() {
            *slot = ((done as usize + j) % 251) as u8;
        }
        hasher.update(&block[..this]);
        done += this as u64;
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&hasher.finalize());
    out
}

// ---------------------------------------------------------------------------
// zero-byte-file (STRESS_HARNESS s3.2 prose: "0-byte")
// ---------------------------------------------------------------------------

/// A single empty file. The degenerate size extreme: the upload pipeline
/// must produce exactly one 0-byte Drive object, mark the row synced, and
/// re-sync as a clean no-op.
#[derive(Default)]
struct ZeroByteFile {
    fixture: FixtureState,
}

#[async_trait]
impl Scenario for ZeroByteFile {
    fn name(&self) -> &'static str {
        "zero-byte-file"
    }
    fn description(&self) -> &'static str {
        "a single 0-byte file uploads as one empty Drive object and re-syncs as a no-op"
    }
    fn requires(&self) -> CapabilityRequirements {
        CapabilityRequirements::none()
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        self.fixture.set(ctx.fixture_root.clone());
        write_deterministic(&ctx.fixture_root.join("src").join("empty.bin"), 0).await?;
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let src_root = self.fixture.src()?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source) =
            boot_and_register(self.fixture.state_db()?, remote.clone(), &src_root).await?;
        let folder = remote.root_id().to_string();

        handle.run_one_cycle().await?;

        let live = live_object_count(&remote, &folder).await?;
        anyhow::ensure!(live == 1, "exactly one object uploaded, got {live}");
        let entries = remote.list_folder(&folder).await?;
        let entry = &entries[0];
        anyhow::ensure!(
            entry.size == Some(0) || entry.size.is_none(),
            "the empty file uploads as a 0-byte object, got size {:?}",
            entry.size
        );
        assert_no_duplicate_op_uuid(&remote, &folder).await?;
        let matches = synced_rows_match_remote(&handle, &remote, &source, &src_root).await?;

        // A second cycle is a clean no-op: still exactly one object.
        handle.run_one_cycle().await?;
        let live2 = live_object_count(&remote, &folder).await?;
        anyhow::ensure!(
            live2 == 1,
            "no-op re-sync kept exactly one object, got {live2}"
        );

        let inv_report =
            crate::scenarios::reporting::assert_invariants(&handle, &remote, source.id, &folder)
                .await?;

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live2,
            final_hash_matches_local: matches,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes: vec!["0-byte file backed up as one empty Drive object".into()],
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// sparse-file-zeros (STRESS_HARNESS s3.2 prose: "sparse"; cross-platform
// logical-content variant of the Windows-only s3.5 `sparse-file` row)
// ---------------------------------------------------------------------------

/// A file whose logical content is mostly zeros. Driven uploads the LOGICAL
/// bytes (per the s3.5 `sparse-file` expectation: "zero ranges become real
/// zeros on the wire"), so the Drive object's size equals the logical size
/// and its bytes are the full zero-padded content. This is the
/// platform-independent sibling of the NTFS `fsutil sparse` row - it does
/// not require the sparse *allocation* (an OS-specific privilege), only that
/// a file with large zero runs round-trips byte-identically.
#[derive(Default)]
struct SparseFileZeros {
    fixture: FixtureState,
}

impl SparseFileZeros {
    /// Logical length: 4 MiB data + 4 MiB zeros + 1 KiB data.
    fn logical_len() -> u64 {
        4 * 1024 * 1024 + 4 * 1024 * 1024 + 1024
    }
}

#[async_trait]
impl Scenario for SparseFileZeros {
    fn name(&self) -> &'static str {
        "sparse-file-zeros"
    }
    fn description(&self) -> &'static str {
        "a file with large zero runs uploads its full logical content; size-on-Drive is the logical size"
    }
    fn requires(&self) -> CapabilityRequirements {
        // 8 MiB logical; runs anywhere.
        CapabilityRequirements::none()
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;
        self.fixture.set(ctx.fixture_root.clone());
        let path = ctx.fixture_root.join("src").join("sparse.bin");
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // A 4 MiB head of real data, then a 4 MiB zero run, then a 1 KiB tail
        // of real data. The zero middle is what a sparse file would leave
        // unallocated; on the wire it must be real zeros.
        let mut f = tokio::fs::File::create(&path).await?;
        f.write_all(&vec![0xABu8; 4 * 1024 * 1024]).await?;
        f.write_all(&vec![0u8; 4 * 1024 * 1024]).await?;
        f.write_all(&vec![0xCDu8; 1024]).await?;
        f.flush().await?;
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let src_root = self.fixture.src()?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source) =
            boot_and_register(self.fixture.state_db()?, remote.clone(), &src_root).await?;
        let folder = remote.root_id().to_string();

        handle.run_one_cycle().await?;

        let live = live_object_count(&remote, &folder).await?;
        anyhow::ensure!(live == 1, "exactly one object uploaded, got {live}");
        let entries = remote.list_folder(&folder).await?;
        let logical = Self::logical_len();
        anyhow::ensure!(
            entries[0].size == Some(logical),
            "size-on-Drive is the logical size {logical}, got {:?}",
            entries[0].size
        );
        assert_no_duplicate_op_uuid(&remote, &folder).await?;
        // Byte-for-byte: the zero middle must be present as real zeros.
        let matches = synced_rows_match_remote(&handle, &remote, &source, &src_root).await?;
        anyhow::ensure!(
            matches,
            "the full logical content (zeros included) round-trips"
        );

        let inv_report =
            crate::scenarios::reporting::assert_invariants(&handle, &remote, source.id, &folder)
                .await?;

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: matches,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes: vec![format!(
                "sparse logical content ({logical} B) uploaded as real zeros; size-on-Drive vs size-on-disk skew documented"
            )],
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// huge-file-10gb (STRESS_HARNESS s3.2)
// ---------------------------------------------------------------------------

/// One large deterministic file driven through the resumable pipeline.
/// Asserts a single object, full byte round-trip, no duplicate, and that the
/// resumable session is consumed on completion. Gated on 20 GiB free so a
/// small host SKIPs cleanly rather than attempting a 10 GB write.
#[derive(Default)]
struct HugeFile10Gb {
    fixture: FixtureState,
}

impl HugeFile10Gb {
    fn file_len() -> u64 {
        10 * 1024 * 1024 * 1024
    }
}

#[async_trait]
impl Scenario for HugeFile10Gb {
    fn name(&self) -> &'static str {
        "huge-file-10gb"
    }
    fn description(&self) -> &'static str {
        "one 10 GB deterministic file completes via the resumable pipeline; bytes round-trip"
    }
    fn requires(&self) -> CapabilityRequirements {
        // Soak-gated (STRESS_HARNESS s3.2): writing + reading a 10 GB file is
        // soak-grade, not PR-gating work, and the 10 GB write can exhaust a CI
        // runner's work volume. Runs in the weekly soak job; SKIPs in the PR
        // matrix.
        CapabilityRequirements::of(vec![
            Capability::Soak,
            Capability::FreeDiskBytes { min: TEN_GB_FREE },
        ])
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        self.fixture.set(ctx.fixture_root.clone());
        write_deterministic(
            &ctx.fixture_root.join("src").join("huge10.bin"),
            Self::file_len(),
        )
        .await?;
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let src_root = self.fixture.src()?;
        // P1-B: arm the streaming content oracle so the fake records only the
        // length + md5 of the uploaded bytes instead of buffering 10 GB in a
        // Vec<u8> (which OOMs / times out). The bytes still stream through the
        // real executor pipeline; verification is by length + digest.
        let remote = Arc::new(InMemoryRemoteStore::new().with_content_oracle());
        let (handle, source) =
            boot_and_register(self.fixture.state_db()?, remote.clone(), &src_root).await?;
        let folder = remote.root_id().to_string();

        handle.run_one_cycle().await?;

        let live = live_object_count(&remote, &folder).await?;
        anyhow::ensure!(
            live == 1,
            "exactly one object after the huge upload, got {live}"
        );
        let entries = remote.list_folder(&folder).await?;
        anyhow::ensure!(
            entries[0].size == Some(Self::file_len()),
            "the full 10 GB landed, got size {:?}",
            entries[0].size
        );
        // Length + digest oracle: the md5 the fake computed over the streamed
        // bytes must equal the md5 of the deterministic source pattern. This is
        // the no-data-loss proof for a file too large to round-trip by download.
        let expected_md5 = deterministic_md5(Self::file_len());
        let matches = entries[0].md5 == Some(expected_md5);
        anyhow::ensure!(
            matches,
            "the 10 GB content md5 round-trips: remote {:?} vs expected {expected_md5:?}",
            entries[0].md5
        );
        assert_no_duplicate_op_uuid(&remote, &folder).await?;
        anyhow::ensure!(
            remote.open_session_count() == 0,
            "the resumable session was consumed on completion"
        );

        let inv_report =
            crate::scenarios::reporting::assert_invariants(&handle, &remote, source.id, &folder)
                .await?;

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: matches,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes: vec![
                "10 GB resumable upload completed; one object; length+md5 verified via content oracle".into(),
            ],
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// huge-file-50gb-mid-run-crash (STRESS_HARNESS s3.2)
// ---------------------------------------------------------------------------

/// A large file whose upload is interrupted mid-stream, then reconciled.
/// The kill is modelled deterministically with the s5
/// `with_network_drop_after` fault (a fatal mid-stream Drive error that
/// aborts the upload exactly as a process death would leave the resumable
/// session); a fresh cycle over the SAME state DB then runs the
/// reconciliation pass (DESIGN s5.6). The expectation is the documented
/// crash-resume behaviour: the session resumes from the persisted offset,
/// exactly one object finalizes, no duplicate.
#[derive(Default)]
struct HugeFile50GbMidRunCrash {
    fixture: FixtureState,
}

impl HugeFile50GbMidRunCrash {
    fn file_len() -> u64 {
        50 * 1024 * 1024 * 1024
    }
}

#[async_trait]
impl Scenario for HugeFile50GbMidRunCrash {
    fn name(&self) -> &'static str {
        "huge-file-50gb-mid-run-crash"
    }
    fn description(&self) -> &'static str {
        "a 50 GB upload interrupted mid-stream resumes from the persisted offset; no duplicate object"
    }
    fn requires(&self) -> CapabilityRequirements {
        // Soak-gated (STRESS_HARNESS s3.2): a 50 GB write/read is soak-grade and
        // far exceeds a PR CI runner's disk; runs in the weekly soak job only.
        CapabilityRequirements::of(vec![
            Capability::Soak,
            Capability::FreeDiskBytes { min: FIFTY_GB_FREE },
        ])
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        self.fixture.set(ctx.fixture_root.clone());
        write_deterministic(
            &ctx.fixture_root.join("src").join("huge50.bin"),
            Self::file_len(),
        )
        .await?;
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let src_root = self.fixture.src()?;
        let state_db = self.fixture.state_db()?;

        // The crash + recovery is a TWO-handle dance over one shared state DB
        // (the real "crash -> reboot -> reconcile" model, DESIGN s5.6). A
        // single orchestrator marks a source reconciled-this-lifetime after
        // its first cycle and would SKIP the resume on a second cycle, so the
        // resume must run under a FRESH handle whose reconcile set is empty -
        // exactly what a process restart provides. The remote, however, is the
        // SAME fake across both (it holds the open resumable session the
        // restart resumes), so it is shared, not re-created.

        // --- phase 1: crash mid-stream -------------------------------------
        // A network drop after the session opens + the first wire chunk acks
        // models the kill: the resumable Create never finalizes.
        // P1-B: arm the content oracle so neither phase buffers 50 GB in the
        // fake's Vec<u8>; the bytes stream through the real pipeline and the
        // resumed object is verified by length + md5. The drop fault and the
        // oracle compose (both are construction-time flags on the same store).
        let remote = Arc::new(
            InMemoryRemoteStore::new()
                .with_network_drop_after(2)
                .with_content_oracle(),
        );
        let folder = remote.root_id().to_string();
        let source = {
            let (handle, source) =
                boot_and_register(state_db.clone(), remote.clone(), &src_root).await?;
            // The mid-stream drop aborts the cycle; the create op is kept with
            // a live resumable session + acked offset (DESIGN s5.4). A cycle
            // error here is the EXPECTED crash, not a scenario failure.
            let _ = handle.run_one_cycle().await;
            anyhow::ensure!(
                live_object_count(&remote, &folder).await? == 0,
                "the object must not finalize before the resume"
            );
            anyhow::ensure!(
                remote.open_session_count() >= 1,
                "a resumable session was left open by the crash"
            );
            // Drop `handle` (process death); the state DB + the open session on
            // the shared remote survive for the restart.
            source
        };

        // --- phase 2: restart -> reconcile resumes the persisted session ----
        // The network drop was single-shot, so phase 2's requests succeed. A
        // fresh handle over the SAME state DB + SAME remote runs reconcile in
        // its first cycle, resuming byte-for-byte from the persisted offset.
        //
        // `DrivenHandleBuilder::boot` seeds a NEW random account on every boot
        // (the committed builder exposes no account pin), so the restart's
        // orchestrator drives a different account id than phase 1's source was
        // registered under, and `reconcile_once` - which iterates the sources
        // FOR the orchestrator's account - would not see it. We re-home the
        // SAME source row (same `source.id`, so the persisted pending_op still
        // matches) onto the restart's account before the cycle. This is a
        // surfaced limitation of the builder, reported to the Integrate agent.
        let handle = DrivenHandleBuilder::new(state_db)
            .remote(remote.clone())
            .boot()
            .await?;
        let rehomed = SourceRow {
            account_id: handle.account_id,
            ..source.clone()
        };
        handle.state.upsert_source(&rehomed).await?;
        handle.run_one_cycle().await?;

        let live = live_object_count(&remote, &folder).await?;
        anyhow::ensure!(live == 1, "resume finalized exactly one object, got {live}");
        let entries = remote.list_folder(&folder).await?;
        anyhow::ensure!(
            entries[0].size == Some(Self::file_len()),
            "the resumed object carries the full 50 GB, got {:?}",
            entries[0].size
        );
        assert_no_duplicate_op_uuid(&remote, &folder).await?;
        anyhow::ensure!(
            remote.open_session_count() == 0,
            "the session was consumed by the resume"
        );
        // Length + digest oracle (P1-B): the resumed object's md5 must equal
        // the deterministic source pattern's md5 - the no-data-loss proof for a
        // file too large to round-trip by download.
        let expected_md5 = deterministic_md5(Self::file_len());
        let matches = entries[0].md5 == Some(expected_md5);
        anyhow::ensure!(
            matches,
            "the resumed 50 GB content md5 round-trips: remote {:?} vs expected {expected_md5:?}",
            entries[0].md5
        );

        // The source row was re-homed onto the restart's account but keeps its
        // original id, so the invariant sweep keys off `source.id` and reads
        // the resumed file_state + drained pending_ops correctly.
        let inv_report =
            crate::scenarios::reporting::assert_invariants(&handle, &remote, source.id, &folder)
                .await?;

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: matches,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes: vec![
                "50 GB upload crashed mid-stream and resumed from the persisted offset; one object, no duplicate".into(),
            ],
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        // Documented crash-resume behaviour, asserted via terminal remote
        // state rather than a surfaced error code.
        ExpectedOutcome::DocumentedBehaviour
    }
}

// ---------------------------------------------------------------------------
// tiny-files-100k-in-one-dir (STRESS_HARNESS s3.2)
// ---------------------------------------------------------------------------

/// Many small files in one directory: 100 000 files of 0-1 KiB. Asserts the
/// pacer does not deadlock (the cycle completes - the harness wall-clock cap
/// catches a deadlock as `harness.timeout`) and every file uploads exactly
/// once - no duplicate, no missing. Gated on 1 GiB free.
#[derive(Default)]
struct TinyFiles100kInOneDir {
    fixture: FixtureState,
}

impl TinyFiles100kInOneDir {
    fn count() -> u32 {
        100_000
    }
}

#[async_trait]
impl Scenario for TinyFiles100kInOneDir {
    fn name(&self) -> &'static str {
        "tiny-files-100k-in-one-dir"
    }
    fn description(&self) -> &'static str {
        "100k files of 0-1 KiB in one directory complete without the pacer deadlocking"
    }
    fn requires(&self) -> CapabilityRequirements {
        // Soak-gated (STRESS_HARNESS s3.2): runs in the weekly soak job, SKIPs
        // in the per-PR matrix - a 100k-file scan is not PR-gating work.
        CapabilityRequirements::of(vec![
            Capability::Soak,
            Capability::FreeDiskBytes {
                min: TINY_FILES_FREE,
            },
        ])
    }
    fn wall_cap(&self) -> std::time::Duration {
        // Scanning + uploading 100k real files is a deterministic, steadily-
        // progressing workload that can run past the 300s default on a slower
        // CI runner in a debug build - not a hang. Larger finite cap.
        std::time::Duration::from_secs(900)
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        self.fixture.set(ctx.fixture_root.clone());
        let dir = ctx.fixture_root.join("src");
        tokio::fs::create_dir_all(&dir).await?;
        // Sizes 0..=1024 cycle deterministically; file f000000 is the 0-byte edge.
        for i in 0..Self::count() {
            let len = (i % 1025) as u64;
            write_deterministic(&dir.join(format!("f{i:06}.dat")), len).await?;
        }
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let src_root = self.fixture.src()?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source) =
            boot_and_register(self.fixture.state_db()?, remote.clone(), &src_root).await?;
        let folder = remote.root_id().to_string();

        // A completing cycle IS the no-deadlock assertion.
        handle.run_one_cycle().await?;

        let live = live_object_count(&remote, &folder).await?;
        anyhow::ensure!(
            live == Self::count() as u64,
            "every tiny file uploaded exactly once, got {live} of {}",
            Self::count()
        );
        assert_no_duplicate_op_uuid(&remote, &folder).await?;
        let rows = handle.state.load_source_file_state(source.id).await?;
        let synced = rows
            .values()
            .filter(|r| r.status == FileStateStatus::Synced)
            .count();
        anyhow::ensure!(
            synced == Self::count() as usize,
            "every file_state row is synced, got {synced}"
        );

        let inv_report =
            crate::scenarios::reporting::assert_invariants(&handle, &remote, source.id, &folder)
                .await?;

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: true,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes: vec![format!(
                "{} tiny files completed; wall-clock recorded as a regression metric, not asserted",
                Self::count()
            )],
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// million-files-nested (STRESS_HARNESS s3.2)
// ---------------------------------------------------------------------------

/// 1000 subdirs x 1000 files (0-4 KiB each). The most expensive fixture
/// (STRESS_HARNESS s3.2 note: ~15 min on an SSD), cacheable under
/// `target/chaos-fixtures/`. Asserts the scanner's memory stays bounded
/// (RSS delta < 100 MiB) and the SQLite state + FTS index build without
/// error. The fixture is marked cacheable so the driver does not delete it
/// on teardown.
#[derive(Default)]
struct MillionFilesNested {
    fixture: FixtureState,
}

impl MillionFilesNested {
    fn dirs() -> u32 {
        1000
    }
    fn files_per_dir() -> u32 {
        1000
    }
    fn total() -> u64 {
        Self::dirs() as u64 * Self::files_per_dir() as u64
    }
}

#[async_trait]
impl Scenario for MillionFilesNested {
    fn name(&self) -> &'static str {
        "million-files-nested"
    }
    fn description(&self) -> &'static str {
        "1000x1000 nested files; scanner RSS delta stays bounded and the SQLite/FTS state builds"
    }
    fn requires(&self) -> CapabilityRequirements {
        // Soak-gated (STRESS_HARNESS s3.2): runs in the weekly soak job, SKIPs
        // in the per-PR matrix - a 1M-file scan is not PR-gating work.
        CapabilityRequirements::of(vec![
            Capability::Soak,
            Capability::FreeDiskBytes {
                min: MILLION_FILES_FREE,
            },
        ])
    }
    fn wall_cap(&self) -> std::time::Duration {
        // Scanning 1,000,000 real files + building the SQLite/FTS state is a
        // deterministic, steadily-progressing workload that legitimately runs
        // well past the 300s default in a debug build - it is not a hang. Give
        // it a large finite cap so a genuine infinite loop is still caught.
        std::time::Duration::from_secs(1800)
    }

    async fn setup(&self, ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        // The expensive cacheable fixture: only materialise it if it is not
        // already present (the driver sets cacheable so this is not deleted).
        ctx.cacheable = true;
        self.fixture.set(ctx.fixture_root.clone());
        let root = ctx.fixture_root.join("src");
        // If a complete prior build exists (sentinel present), reuse it.
        let sentinel = ctx.fixture_root.join(".million-files-complete");
        if tokio::fs::try_exists(&sentinel).await.unwrap_or(false) {
            return Ok(());
        }
        for d in 0..Self::dirs() {
            let sub = root.join(format!("d{d:04}"));
            tokio::fs::create_dir_all(&sub).await?;
            for f in 0..Self::files_per_dir() {
                let len = ((d.wrapping_add(f)) % 4097) as u64; // 0..=4096
                write_deterministic(&sub.join(format!("f{f:04}.dat")), len).await?;
            }
        }
        tokio::fs::write(&sentinel, b"ok").await?;
        Ok(())
    }

    async fn run_assertions(&self, _handle: &DrivenHandle) -> anyhow::Result<Outcome> {
        let src_root = self.fixture.src()?;
        let remote = Arc::new(InMemoryRemoteStore::new());
        let (handle, source) =
            boot_and_register(self.fixture.state_db()?, remote.clone(), &src_root).await?;
        let folder = remote.root_id().to_string();

        // Sample RSS before/after the scan to assert the < 100 MiB delta. The
        // probe is best-effort: if the platform read is unavailable the bound
        // check is skipped (recorded in notes) rather than faked as passing.
        let rss_before = current_rss_bytes();
        handle.run_one_cycle().await?;
        let rss_after = current_rss_bytes();

        let live = live_object_count(&remote, &folder).await?;
        anyhow::ensure!(
            live == Self::total(),
            "every nested file uploaded exactly once, got {live} of {}",
            Self::total()
        );
        assert_no_duplicate_op_uuid(&remote, &folder).await?;

        let rows = handle.state.load_source_file_state(source.id).await?;
        anyhow::ensure!(
            rows.len() == Self::total() as usize,
            "every file has a file_state row, got {}",
            rows.len()
        );
        // Exercise the FTS path so a build error surfaces here.
        let _ = handle
            .state
            .search_files(Some(source.id), "dat", 10)
            .await?;

        let mut notes = vec![format!(
            "{} nested files; SQLite + FTS built",
            Self::total()
        )];
        match (rss_before, rss_after) {
            (Some(b), Some(a)) => {
                let delta = a.saturating_sub(b);
                let bound = 100 * 1024 * 1024;
                anyhow::ensure!(
                    delta < bound,
                    "scanner RSS delta {delta} exceeded the {bound}-byte bound"
                );
                notes.push(format!("scanner RSS delta {delta} B (< 100 MiB)"));
            }
            _ => notes
                .push("RSS probe unavailable on this platform; memory bound not checked".into()),
        }

        let inv_report =
            crate::scenarios::reporting::assert_invariants(&handle, &remote, source.id, &folder)
                .await?;

        Ok(Outcome {
            error_codes_seen: vec![],
            final_drive_object_count: live,
            final_hash_matches_local: true,
            invariants: Some(inv_report.to_invariant_outcome(true)),
            notes,
        })
    }

    async fn teardown(&self, _ctx: &mut ScenarioContext) -> anyhow::Result<()> {
        // Cacheable fixture: the driver preserves fixture_root because
        // ctx.cacheable was set in setup. Nothing to release here.
        Ok(())
    }
    fn expected_outcome(&self) -> ExpectedOutcome {
        ExpectedOutcome::Success
    }
}

// ---------------------------------------------------------------------------
// Best-effort resident-set-size probe for the memory-bound assertion.
// ---------------------------------------------------------------------------

/// Current process resident set size in bytes, or `None` if the platform
/// read is unavailable. Linux reads `/proc/self/statm` (pages * page size);
/// other platforms return `None` so the dependent assertion records "not
/// checked" rather than a fabricated pass (STRESS_HARNESS s8 honest skip).
fn current_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        // 4 KiB is the page size on every Linux target the harness runs on;
        // sysconf(_SC_PAGESIZE) would need libc, which the crate does not pull.
        Some(resident_pages * 4096)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry exposes exactly the six file-size rows, each with a
    /// stable kebab-case name (STRESS_HARNESS s3.2 + the brief's 0-byte /
    /// sparse extras).
    #[test]
    fn registers_every_file_size_scenario() {
        let names: Vec<&str> = scenarios().iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec![
                "zero-byte-file",
                "sparse-file-zeros",
                "huge-file-10gb",
                "huge-file-50gb-mid-run-crash",
                "tiny-files-100k-in-one-dir",
                "million-files-nested",
            ]
        );
    }

    /// The big-disk rows gate on free-disk capability so they SKIP rather
    /// than try to write tens of gigabytes on a small host (STRESS_HARNESS
    /// s2.5 / s8). The two small rows run anywhere.
    #[test]
    fn capability_gates_match_the_catalogue() {
        for s in scenarios() {
            let req = s.requires();
            match s.name() {
                "zero-byte-file" | "sparse-file-zeros" => {
                    assert!(req.required.is_empty(), "{} runs anywhere", s.name());
                }
                "huge-file-10gb"
                | "huge-file-50gb-mid-run-crash"
                | "tiny-files-100k-in-one-dir"
                | "million-files-nested" => {
                    assert!(
                        req.required
                            .iter()
                            .any(|c| matches!(c, Capability::FreeDiskBytes { .. })),
                        "{} gates on free disk",
                        s.name()
                    );
                }
                other => panic!("unexpected scenario {other}"),
            }
        }
    }

    /// The two ungated rows (`zero-byte-file`, `sparse-file-zeros`) drive the
    /// real headless core to a clean upload + byte round-trip end to end.
    #[tokio::test]
    async fn zero_byte_and_sparse_round_trip() {
        for scenario in [
            Box::new(ZeroByteFile::default()) as Box<dyn Scenario>,
            Box::new(SparseFileZeros::default()) as Box<dyn Scenario>,
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut ctx = ScenarioContext {
                fixture_root: dir.path().to_path_buf(),
                cacheable: false,
            };
            scenario.setup(&mut ctx).await.expect("setup");

            // The trait passes a handle; this category boots its own (see the
            // module note), so a throwaway default handle satisfies the call.
            let throwaway = tempfile::tempdir().expect("tempdir");
            let handle = DrivenHandleBuilder::new(throwaway.path().join("state.db"))
                .boot()
                .await
                .expect("boot throwaway");

            let outcome = scenario.run_assertions(&handle).await.expect("assertions");
            assert_eq!(outcome.final_drive_object_count, 1, "{}", scenario.name());
            assert!(outcome.final_hash_matches_local, "{}", scenario.name());

            scenario.teardown(&mut ctx).await.expect("teardown");
        }
    }
}
