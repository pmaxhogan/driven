//! Headless restore: pull a source's backed-up bytes off the remote and write
//! them to a destination directory, with NO GUI and no Tauri involvement.
//!
//! The GUI restore path lives in `src-tauri/src/commands/restore.rs`; it owns
//! progress events, cancellation, a per-item selection model, point-in-time
//! version selection, and the handle-based anti-TOCTOU destination confinement
//! (`mod confine`). None of that is reproduced here. This module exists so an
//! automated agent (or a developer at a terminal) can do a full backup ->
//! restore round trip and byte-compare the result, which is otherwise only
//! reachable by clicking through the app.
//!
//! # Deliberate non-goals
//!
//! - **No destination confinement.** The GUI resolves every write against a
//!   verified parent directory handle so a symlink swapped in mid-restore
//!   cannot redirect the write outside the chosen folder (R2-P1-1). Here the
//!   destination is supplied by whoever runs the process - a test harness or the
//!   operator themselves - so plain `std::fs` / `tokio::fs` writes are used.
//!   Do NOT reuse this engine to service an untrusted destination path.
//! - **No cancellation and no progress events.** A run goes to completion; the
//!   caller learns what happened from the returned [`RestoreReport`].
//! - **No point-in-time versions.** Only the CURRENT contents of each
//!   `file_state` row are restored, never a superseded `file_versions` row.
//!
//! # What it does mirror
//!
//! The parts that decide whether restored bytes are CORRECT are copied
//! faithfully from the production path, because a round-trip harness that
//! decodes differently from the app would validate the wrong thing:
//!
//! - the on-wire framing for an encrypted object (a fixed [`HEADER_LEN`]-byte
//!   header, then [`PLAINTEXT_CHUNK_LEN`]+tag ciphertext frames, `decrypt_chunk`
//!   for every frame but the last and `decrypt_last` for the trailer);
//! - the restorability rule (`synced` AND (a standalone object OR a bundle
//!   membership)), with a bundle consulted only when `drive_file_id` is NULL;
//! - post-decrypt verification of BOTH the plaintext BLAKE3 and the plaintext
//!   LENGTH against the `file_state` row, on the standalone and the bundled path
//!   alike;
//! - the two bundle size caps ([`MAX_BUNDLE_OBJECT_BYTES`] on the downloaded
//!   object, [`MAX_BUNDLE_DECOMPRESSED`] on the extracted archive), which are the
//!   gzip-bomb / tampered-object guard rather than incidental tuning.
//!
//! The framing loop is duplicated rather than shared with `src-tauri` on
//! purpose: extracting it would mean threading the app's `ErrorCode` taxonomy
//! through a generic, and the two decoders are pinned to the same reality by
//! deriving their frame size from [`driven_crypto::content::PLAINTEXT_CHUNK_LEN`]
//! plus [`frame_len_matches_the_encryptor`], which asserts the constant against
//! what a real [`driven_crypto::ContentEncryptor`] actually emits.
//!
//! # Which rows are enumerated
//!
//! [`restore_source`] reads the whole `file_state` map for the source via
//! [`StateRepo::load_source_file_state`], NOT
//! [`StateRepo::list_file_state_under_prefix`]. The prefix query takes a `limit`
//! and offers no cursor - its `limit + 1` convention exists so a caller can
//! DETECT truncation, not paginate past it - so an engine contracted to restore
//! EVERYTHING cannot be built on it without silently dropping rows on a large
//! source. `load_source_file_state` is uncapped, carries the `hash_blake3` the
//! prefix row lacks, and is the same query the scanner already runs per cycle,
//! so its memory profile is one the engine already accepts.
//!
//! # StateRepo fakes
//!
//! [`StateRepo::get_bundle_ref_for_member`] has a trait DEFAULT returning
//! `None`; only `SqliteStateRepo` overrides it. Against a fake repo that models
//! no bundles, every bundled member therefore reports as
//! [`SkipReason::NotUploaded`] rather than failing - fine for a fake with no
//! bundles, misleading for one that has them. Restore against the real repo.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use blake3::Hasher as Blake3;
use driven_crypto::content::PLAINTEXT_CHUNK_LEN;
use driven_crypto::{ContentDecryptor, SourceCryptoSuite, HEADER_LEN};
use driven_drive::remote_store::RemoteStore;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::state::{FileStateRow, SourceRow, StateRepo};
use crate::types::{FileStateStatus, RelativePath};

/// Tracing target for the headless restore engine.
const TARGET: &str = "driven::restore_fetch";

/// The XChaCha20-Poly1305 AEAD tag appended to every STREAM frame.
const TAG_LEN: usize = 16;

/// One ciphertext frame on the wire: a full plaintext chunk plus its AEAD tag.
///
/// This MUST equal what [`driven_crypto::ContentEncryptor::encrypt_chunk`]
/// emits for a full chunk - STREAM BE32 is boundary-sensitive, so decrypting at
/// any other size fails the tag rather than producing wrong bytes. Pinned by
/// [`frame_len_matches_the_encryptor`].
const CIPHERTEXT_FRAME: usize = PLAINTEXT_CHUNK_LEN + TAG_LEN;

/// Cap on the bytes downloaded for one `.tar.gz` bundle object (post-decrypt).
/// A bundle is size-capped at build time, so a larger object means a tampered
/// or corrupt one; failing beats growing a `Vec` unboundedly.
pub const MAX_BUNDLE_OBJECT_BYTES: u64 = 32 * 1024 * 1024;

/// Cap on the bytes a bundle may decompress to during member extraction (the
/// gzip-bomb guard, enforced inside [`crate::bundle::extract_member`]).
pub const MAX_BUNDLE_DECOMPRESSED: u64 = 64 * 1024 * 1024;

/// Suffix for the temp file a restore writes before it is verified and renamed
/// into place, so a failed hash / length check never leaves a partial file
/// looking like restored data.
const TEMP_SUFFIX: &str = ".driven-restore-tmp";

/// Why a `file_state` row was skipped rather than restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The row has no byte source: no standalone `drive_file_id` and no bundle
    /// membership. The file was never uploaded (or its object was cleared for
    /// re-upload), so there is nothing on the remote to fetch.
    NotUploaded,
    /// The row has a byte source but its status is not
    /// [`FileStateStatus::Synced`], so the remote bytes are not known to match
    /// the recorded hash. Mirrors the GUI's restorability rule.
    NotSynced(FileStateStatus),
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::NotUploaded => f.write_str("never uploaded (no remote object)"),
            // `FileStateStatus` has no `Display`; the Debug spelling (`Pending`,
            // `Corrupt`, ...) is what the rest of the crate logs too.
            SkipReason::NotSynced(status) => write!(f, "status is {status:?}, not synced"),
        }
    }
}

/// What happened to one file during a restore run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOutcome {
    /// The bytes were downloaded, decrypted if needed, verified against the
    /// recorded BLAKE3 + length, and written to the destination.
    Restored {
        /// Plaintext bytes written.
        bytes: u64,
        /// Whether the byte source was a `.tar.gz` bundle rather than a
        /// standalone object.
        from_bundle: bool,
    },
    /// The row was not attempted; see [`SkipReason`].
    Skipped(SkipReason),
    /// The restore was attempted and failed. `reason` is a human-readable
    /// message (download failure, decrypt failure, hash mismatch, write error).
    Failed(String),
}

/// One file's line in a [`RestoreReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileReport {
    /// The plaintext relative path under the source root (the `file_state` key).
    pub relative_path: RelativePath,
    /// What happened to it.
    pub outcome: FileOutcome,
}

/// The result of one [`restore_source`] run: one entry per `file_state` row,
/// ordered by relative path so two runs over the same state produce identical
/// reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Per-file outcomes, sorted by `relative_path`.
    pub files: Vec<FileReport>,
}

impl RestoreReport {
    /// Number of files written to the destination.
    pub fn restored(&self) -> usize {
        self.files
            .iter()
            .filter(|f| matches!(f.outcome, FileOutcome::Restored { .. }))
            .count()
    }

    /// Number of files that were attempted and failed.
    pub fn failed(&self) -> usize {
        self.files
            .iter()
            .filter(|f| matches!(f.outcome, FileOutcome::Failed(_)))
            .count()
    }

    /// Number of files deliberately not attempted (never uploaded / not synced).
    pub fn skipped(&self) -> usize {
        self.files
            .iter()
            .filter(|f| matches!(f.outcome, FileOutcome::Skipped(_)))
            .count()
    }

    /// Total plaintext bytes written across every restored file.
    pub fn bytes_restored(&self) -> u64 {
        self.files
            .iter()
            .map(|f| match f.outcome {
                FileOutcome::Restored { bytes, .. } => bytes,
                _ => 0,
            })
            .sum()
    }

    /// Whether the run is a success: ZERO failures.
    ///
    /// A SKIP is deliberately not a failure. A row that was never uploaded, or
    /// that is `pending` because the local file changed after its last upload,
    /// has no restorable bytes on the remote - the backup is behaving correctly
    /// and there is nothing to fix. Only an attempted-and-failed restore (a dead
    /// remote object, a bad decrypt, a hash mismatch, a write error) means the
    /// backup did not survive the round trip. Callers that want a stricter gate
    /// should assert on [`Self::skipped`] themselves.
    pub fn ok(&self) -> bool {
        self.failed() == 0
    }

    /// The failed entries, for rendering a summary.
    pub fn failures(&self) -> impl Iterator<Item = (&RelativePath, &str)> {
        self.files.iter().filter_map(|f| match &f.outcome {
            FileOutcome::Failed(reason) => Some((&f.relative_path, reason.as_str())),
            _ => None,
        })
    }
}

/// Knobs for one [`restore_source`] run.
#[derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    /// When `false` (the default), a destination file that already exists is
    /// reported as a failure rather than overwritten, so a restore into a
    /// populated directory cannot silently clobber it. A round-trip harness
    /// restores into an empty temp dir and does not care; a human pointing at a
    /// real folder very much does.
    pub overwrite: bool,
}

/// A ONE-ENTRY cache of the most recently downloaded, decrypted `.tar.gz`
/// bundle, so a bundle serving many members is fetched once instead of once per
/// member.
///
/// Deliberately a single slot rather than a map, matching the production
/// restore's `BundleCache`: a bundle object can be up to
/// [`MAX_BUNDLE_OBJECT_BYTES`], so an unevicted map would hold every bundle a
/// run touched live at once - gigabytes on a source with a few hundred bundles,
/// during the operation a backup tool can least afford to run out of memory.
/// One slot bounds peak memory at one bundle and still hits on every member,
/// because [`restore_source`] walks rows in `relative_path` order and a bundle
/// packs one directory, so its members are contiguous. A miss costs a
/// re-download, never a wrong answer.
#[derive(Default)]
struct BundleCache {
    entry: Option<(String, Arc<Vec<u8>>)>,
}

impl BundleCache {
    /// The cached bytes for `drive_file_id`, if it is the slot's occupant.
    fn get(&self, drive_file_id: &str) -> Option<Arc<Vec<u8>>> {
        self.entry
            .as_ref()
            .filter(|(id, _)| id == drive_file_id)
            .map(|(_, bytes)| bytes.clone())
    }

    /// Replace the slot, dropping whatever bundle it held.
    fn put(&mut self, drive_file_id: String, bytes: Arc<Vec<u8>>) {
        self.entry = Some((drive_file_id, bytes));
    }
}

/// Restore every restorable file of `source` from `remote` into `dest_dir`,
/// preserving relative paths.
///
/// `crypto` must be `Some` for an encrypted source (the caller unwraps the
/// per-source key); passing `None` for one makes every download fail its
/// decrypt, which is loud but wasteful, so callers should fail closed before
/// getting here. Passing `Some` for a PLAINTEXT source is equally wrong and is
/// rejected up front, since the stored objects carry no crypto header.
///
/// Returns `Err` only for a failure that aborts the whole run (the state query
/// itself failing, or an unusable destination). A per-file problem is recorded
/// in the [`RestoreReport`] and the run continues, so one dead remote object
/// does not cost the user every other file.
pub async fn restore_source(
    state: &dyn StateRepo,
    remote: &dyn RemoteStore,
    source: &SourceRow,
    crypto: Option<&dyn SourceCryptoSuite>,
    dest_dir: &Path,
    opts: &RestoreOptions,
) -> Result<RestoreReport> {
    if source.encryption_enabled && crypto.is_none() {
        anyhow::bail!(
            "source {} is encrypted but no crypto suite was supplied; refusing to restore \
             (every object would fail to decrypt)",
            source.id
        );
    }
    if !source.encryption_enabled && crypto.is_some() {
        anyhow::bail!(
            "source {} is NOT encrypted but a crypto suite was supplied; refusing to restore \
             (its objects carry no crypto header)",
            source.id
        );
    }

    tokio::fs::create_dir_all(dest_dir)
        .await
        .with_context(|| format!("create restore destination {}", dest_dir.display()))?;

    let rows = state
        .load_source_file_state(source.id)
        .await
        .with_context(|| format!("load file_state for source {}", source.id))?;

    // Sort by path so the report (and the order objects are fetched in) is
    // deterministic across runs - a harness diffing two reports should see
    // differences only where behaviour differs.
    let mut ordered: Vec<(RelativePath, FileStateRow)> = rows.into_iter().collect();
    ordered.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));

    let mut bundle_cache = BundleCache::default();
    let mut report = RestoreReport::default();

    for (relative_path, row) in ordered {
        let outcome = restore_one(
            state,
            remote,
            source,
            crypto,
            dest_dir,
            opts,
            &relative_path,
            &row,
            &mut bundle_cache,
        )
        .await;
        if let FileOutcome::Failed(reason) = &outcome {
            tracing::warn!(
                target: TARGET,
                source_id = %source.id,
                file = %relative_path,
                %reason,
                "headless restore failed for one file"
            );
        }
        report.files.push(FileReport {
            relative_path,
            outcome,
        });
    }

    tracing::info!(
        target: TARGET,
        source_id = %source.id,
        restored = report.restored(),
        skipped = report.skipped(),
        failed = report.failed(),
        bytes = report.bytes_restored(),
        "headless restore finished"
    );
    Ok(report)
}

/// Restore ONE `file_state` row, returning its outcome. Never returns `Err`:
/// every per-file problem is a [`FileOutcome::Failed`] so the run continues.
#[allow(clippy::too_many_arguments)]
async fn restore_one(
    state: &dyn StateRepo,
    remote: &dyn RemoteStore,
    source: &SourceRow,
    crypto: Option<&dyn SourceCryptoSuite>,
    dest_dir: &Path,
    opts: &RestoreOptions,
    relative_path: &RelativePath,
    row: &FileStateRow,
    bundle_cache: &mut BundleCache,
) -> FileOutcome {
    // A bundled member legitimately has a NULL `drive_file_id` (its bytes live
    // inside the `.tar.gz`), so consult `bundle_members` ONLY when there is no
    // standalone object. Order matters: a member promoted back to a standalone
    // object keeps a stale membership row until the next commit, and the
    // standalone id is the live pointer.
    let bundle = if row.drive_file_id.is_none() {
        match state
            .get_bundle_ref_for_member(source.id, relative_path)
            .await
        {
            Ok(b) => b,
            Err(e) => return FileOutcome::Failed(format!("resolve bundle membership: {e}")),
        }
    } else {
        None
    };

    if row.drive_file_id.is_none() && bundle.is_none() {
        return FileOutcome::Skipped(SkipReason::NotUploaded);
    }
    if row.status != FileStateStatus::Synced {
        return FileOutcome::Skipped(SkipReason::NotSynced(row.status));
    }

    let dest_path = match join_relative(dest_dir, relative_path) {
        Some(p) => p,
        None => return FileOutcome::Failed("relative path has no usable components".to_string()),
    };
    if !opts.overwrite && dest_path.exists() {
        return FileOutcome::Failed(format!(
            "{} already exists (pass overwrite to replace it)",
            dest_path.display()
        ));
    }
    if let Some(parent) = dest_path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return FileOutcome::Failed(format!("create {}: {e}", parent.display()));
        }
    }

    match bundle {
        Some(bundle_ref) => {
            restore_bundled_member(
                remote,
                crypto,
                &bundle_ref.drive_file_id,
                relative_path,
                row,
                &dest_path,
                bundle_cache,
            )
            .await
        }
        None => {
            // Unreachable for a None `drive_file_id` (the skip above caught it),
            // but expressed as a match so the invariant is local.
            let Some(file_id) = row.drive_file_id.as_deref() else {
                return FileOutcome::Skipped(SkipReason::NotUploaded);
            };
            restore_standalone(remote, crypto, file_id, row, &dest_path).await
        }
    }
}

/// Download one standalone object, streaming it (decrypting frame by frame when
/// `crypto` is set) into a temp file beside the destination, verifying the
/// plaintext BLAKE3 + length, then renaming it into place.
///
/// Streaming rather than buffering is the point: a multi-GiB file must never sit
/// whole in memory, so at most ~2 ciphertext frames (~128 KiB) are held at once.
async fn restore_standalone(
    remote: &dyn RemoteStore,
    crypto: Option<&dyn SourceCryptoSuite>,
    file_id: &str,
    row: &FileStateRow,
    dest_path: &Path,
) -> FileOutcome {
    let mut reader = match remote.download(file_id).await {
        Ok(stream) => stream.0,
        Err(e) => return FileOutcome::Failed(format!("download {file_id}: {e}")),
    };

    let temp_path = temp_path_for(dest_path);
    let temp = match tokio::fs::File::create(&temp_path).await {
        Ok(f) => f,
        Err(e) => return FileOutcome::Failed(format!("create {}: {e}", temp_path.display())),
    };
    let mut writer = tokio::io::BufWriter::new(temp);
    let mut hasher = Blake3::new();

    let written = match decode_into(&mut reader, &mut writer, crypto, Some(&mut hasher), None).await
    {
        Ok(n) => n,
        Err(e) => return fail_and_clean(&temp_path, format!("{e}")).await,
    };
    if let Err(e) = writer.flush().await {
        return fail_and_clean(&temp_path, format!("flush {}: {e}", temp_path.display())).await;
    }
    // fsync before the rename so a crash cannot leave a renamed-but-empty file.
    if let Err(e) = writer.get_ref().sync_all().await {
        return fail_and_clean(&temp_path, format!("fsync {}: {e}", temp_path.display())).await;
    }
    drop(writer);

    if let Err(reason) = verify_plaintext(hasher.finalize().as_bytes(), written, row) {
        return fail_and_clean(&temp_path, reason).await;
    }
    if let Err(e) = tokio::fs::rename(&temp_path, dest_path).await {
        return fail_and_clean(
            &temp_path,
            format!("rename into {}: {e}", dest_path.display()),
        )
        .await;
    }
    FileOutcome::Restored {
        bytes: written,
        from_bundle: false,
    }
}

/// Restore one file whose bytes live inside a `.tar.gz` bundle (issue #35):
/// download + decrypt the whole (size-capped) bundle object once per run, then
/// extract this member and verify it exactly as the standalone path does.
///
/// In-memory by necessity - a `.tar.gz` member cannot be located without reading
/// the archive - which is why both caps are enforced rather than advisory.
async fn restore_bundled_member(
    remote: &dyn RemoteStore,
    crypto: Option<&dyn SourceCryptoSuite>,
    bundle_file_id: &str,
    relative_path: &RelativePath,
    row: &FileStateRow,
    dest_path: &Path,
    bundle_cache: &mut BundleCache,
) -> FileOutcome {
    let tar_gz = match bundle_cache.get(bundle_file_id) {
        Some(cached) => cached.clone(),
        None => {
            let mut reader = match remote.download(bundle_file_id).await {
                Ok(stream) => stream.0,
                Err(e) => {
                    return FileOutcome::Failed(format!("download bundle {bundle_file_id}: {e}"))
                }
            };
            let mut buf: Vec<u8> = Vec::new();
            if let Err(e) = decode_into(
                &mut reader,
                &mut buf,
                crypto,
                None,
                Some(MAX_BUNDLE_OBJECT_BYTES),
            )
            .await
            {
                return FileOutcome::Failed(format!("read bundle {bundle_file_id}: {e}"));
            }
            let bytes = Arc::new(buf);
            bundle_cache.put(bundle_file_id.to_string(), bytes.clone());
            bytes
        }
    };

    // `extract_member` is synchronous, gzip-decompresses, and is bounded by
    // MAX_BUNDLE_DECOMPRESSED; run it off the reactor. The Arc is cloned in
    // because a blocking task cannot borrow from this one.
    let entry_name = crate::bundle::member_entry_name(relative_path);
    let member = tokio::task::spawn_blocking(move || {
        crate::bundle::extract_member(&tar_gz, &entry_name, MAX_BUNDLE_DECOMPRESSED)
    })
    .await;
    let member_bytes = match member {
        Ok(Ok(Some(bytes))) => bytes,
        Ok(Ok(None)) => {
            return FileOutcome::Failed(format!(
                "bundle {bundle_file_id} has no entry for this file"
            ))
        }
        Ok(Err(e)) => return FileOutcome::Failed(format!("extract from {bundle_file_id}: {e}")),
        Err(e) => return FileOutcome::Failed(format!("bundle extract task failed: {e}")),
    };

    let digest = blake3::hash(&member_bytes);
    if let Err(reason) = verify_plaintext(digest.as_bytes(), member_bytes.len() as u64, row) {
        return FileOutcome::Failed(reason);
    }

    // Same temp-then-rename discipline as the standalone path, so a failed write
    // never leaves a half-file at the destination.
    let temp_path = temp_path_for(dest_path);
    let temp = match tokio::fs::File::create(&temp_path).await {
        Ok(f) => f,
        Err(e) => return FileOutcome::Failed(format!("create {}: {e}", temp_path.display())),
    };
    let mut writer = tokio::io::BufWriter::new(temp);
    if let Err(e) = writer.write_all(&member_bytes).await {
        return fail_and_clean(&temp_path, format!("write {}: {e}", temp_path.display())).await;
    }
    if let Err(e) = writer.flush().await {
        return fail_and_clean(&temp_path, format!("flush {}: {e}", temp_path.display())).await;
    }
    if let Err(e) = writer.get_ref().sync_all().await {
        return fail_and_clean(&temp_path, format!("fsync {}: {e}", temp_path.display())).await;
    }
    drop(writer);
    if let Err(e) = tokio::fs::rename(&temp_path, dest_path).await {
        return fail_and_clean(
            &temp_path,
            format!("rename into {}: {e}", dest_path.display()),
        )
        .await;
    }
    FileOutcome::Restored {
        bytes: member_bytes.len() as u64,
        from_bundle: true,
    }
}

/// Copy `reader` into `writer`, decrypting frame by frame when `suite` is set,
/// feeding every plaintext byte to `hasher` when supplied, and refusing to write
/// past `max_bytes` when supplied. Returns the plaintext byte count.
///
/// The encrypted arm reproduces the executor's framing exactly. The "is this the
/// LAST frame?" question has no in-band answer, so the loop keeps a rolling
/// buffer and only `decrypt_chunk`s a leading frame once STRICTLY MORE than one
/// frame is buffered (proving a later frame exists); whatever is left at EOF - at
/// most one frame, possibly empty - is the `decrypt_last` trailer. That bounds
/// the buffer to ~2 frames regardless of object size.
async fn decode_into<R, W>(
    reader: &mut R,
    writer: &mut W,
    suite: Option<&dyn SourceCryptoSuite>,
    mut hasher: Option<&mut Blake3>,
    max_bytes: Option<u64>,
) -> Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut written: u64 = 0;
    let mut emit = |written: &mut u64, bytes: &[u8]| -> Result<()> {
        if let Some(cap) = max_bytes {
            if written.saturating_add(bytes.len() as u64) > cap {
                anyhow::bail!("object exceeds the {cap}-byte cap");
            }
        }
        if let Some(h) = hasher.as_deref_mut() {
            h.update(bytes);
        }
        *written = written.saturating_add(bytes.len() as u64);
        Ok(())
    };

    match suite {
        None => {
            let mut buf = vec![0u8; CIPHERTEXT_FRAME];
            loop {
                let n = reader
                    .read(&mut buf)
                    .await
                    .context("read download stream")?;
                if n == 0 {
                    break;
                }
                emit(&mut written, &buf[..n])?;
                writer
                    .write_all(&buf[..n])
                    .await
                    .context("write restored bytes")?;
            }
        }
        Some(suite) => {
            // The fixed header opens the decryptor. A short read here means a
            // truncated or non-Driven object, which is a decrypt failure rather
            // than a transport one.
            let mut header = [0u8; HEADER_LEN];
            reader
                .read_exact(&mut header)
                .await
                .context("read the encrypted object header (truncated object?)")?;
            let mut dec: Box<dyn ContentDecryptor> = suite
                .content_decryptor(&header)
                .map_err(|e| anyhow::anyhow!("open decryptor: {e}"))?;

            let mut buf: Vec<u8> = Vec::with_capacity(CIPHERTEXT_FRAME * 2);
            let mut read_chunk = vec![0u8; CIPHERTEXT_FRAME];
            let mut eof = false;
            while !eof {
                while buf.len() <= CIPHERTEXT_FRAME && !eof {
                    let n = reader
                        .read(&mut read_chunk)
                        .await
                        .context("read download stream")?;
                    if n == 0 {
                        eof = true;
                    } else {
                        buf.extend_from_slice(&read_chunk[..n]);
                    }
                }
                while buf.len() > CIPHERTEXT_FRAME {
                    let frame: Vec<u8> = buf.drain(..CIPHERTEXT_FRAME).collect();
                    let pt = dec
                        .decrypt_chunk(&frame)
                        .map_err(|e| anyhow::anyhow!("decrypt frame: {e}"))?;
                    emit(&mut written, &pt)?;
                    writer
                        .write_all(&pt)
                        .await
                        .context("write restored bytes")?;
                }
            }
            let pt = dec
                .decrypt_last(&buf)
                .map_err(|e| anyhow::anyhow!("decrypt final frame: {e}"))?;
            emit(&mut written, &pt)?;
            writer
                .write_all(&pt)
                .await
                .context("write restored bytes")?;
        }
    }
    Ok(written)
}

/// DATA-SAFETY: the restored plaintext must match BOTH the recorded BLAKE3 and
/// the recorded length before it is presented as the user's data. The hash alone
/// is not enough on its own terms - a length check catches a state row that
/// drifted from its object even when the digest comparison is skipped by a
/// future refactor - and the production path checks both, so this does too.
fn verify_plaintext(digest: &[u8; 32], written: u64, row: &FileStateRow) -> Result<(), String> {
    if digest != &row.hash_blake3 {
        return Err(format!(
            "plaintext blake3 mismatch (expected {}, got {})",
            hex::encode(row.hash_blake3),
            hex::encode(digest)
        ));
    }
    if written != row.size {
        return Err(format!(
            "plaintext length mismatch (expected {}, got {written})",
            row.size
        ));
    }
    Ok(())
}

/// Remove the temp file (best effort) and return the failure outcome, so a
/// failed restore leaves no partial artefact next to the destination.
async fn fail_and_clean(temp_path: &Path, reason: String) -> FileOutcome {
    tokio::fs::remove_file(temp_path).await.ok();
    FileOutcome::Failed(reason)
}

/// The temp file a restore writes before verification: a sibling of the
/// destination, so the final step is a same-directory rename (atomic on every
/// platform Driven targets) rather than a cross-device copy.
fn temp_path_for(dest_path: &Path) -> PathBuf {
    let mut name = dest_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "restore".to_string());
    name.push_str(TEMP_SUFFIX);
    match dest_path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// Join a `/`-separated [`RelativePath`] onto `root` one component at a time.
///
/// Not `root.join(rel.as_str())`: on Windows a literal `a/b/c` would be handed
/// to the OS as a single component name. [`RelativePath`] has already rejected
/// absolute, drive-relative, UNC and `..` forms at construction, so the
/// components here are plain names.
fn join_relative(root: &Path, rel: &RelativePath) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    let mut any = false;
    for component in rel.as_str().split('/') {
        if component.is_empty() {
            continue;
        }
        out.push(component);
        any = true;
    }
    any.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_crypto::key::SourceKey;
    use driven_crypto::DrivenCryptoSuite;

    fn rel(s: &str) -> RelativePath {
        RelativePath::try_from(s.to_string()).expect("valid relative path")
    }

    fn row_for(bytes: &[u8]) -> FileStateRow {
        FileStateRow {
            source_id: crate::types::SourceId::new_v4(),
            relative_path: rel("f.bin"),
            size: bytes.len() as u64,
            mtime_ns: 0,
            hash_blake3: *blake3::hash(bytes).as_bytes(),
            drive_file_id: None,
            drive_md5: None,
            encrypted_remote_path: None,
            status: FileStateStatus::Synced,
            last_uploaded_at: None,
            last_verified_at: None,
        }
    }

    /// The single constant that could silently desync this decoder from the
    /// encryptor is the frame size, so pin it against what a REAL encryptor
    /// emits rather than trusting two hand-written arithmetic expressions to
    /// agree. If `driven-crypto` ever changes its chunk size or AEAD tag, this
    /// fails here instead of corrupting every restored file over 64 KiB.
    #[test]
    fn frame_len_matches_the_encryptor() {
        let suite = DrivenCryptoSuite::new(SourceKey::generate());
        let mut enc = suite.content_encryptor();
        assert_eq!(enc.header().len(), HEADER_LEN);
        let full = vec![0x5Au8; PLAINTEXT_CHUNK_LEN];
        let frame = enc.encrypt_chunk(&full).expect("encrypt a full chunk");
        assert_eq!(
            frame.len(),
            CIPHERTEXT_FRAME,
            "the on-wire frame size drifted from driven-crypto"
        );
    }

    /// Encrypt `plaintext` exactly as the executor does (header, then full
    /// chunks, then a final chunk) so the decoder is fed genuine on-wire bytes.
    fn encrypt_object(suite: &DrivenCryptoSuite, plaintext: &[u8]) -> Vec<u8> {
        let mut enc = suite.content_encryptor();
        let mut out = enc.header().to_vec();
        let mut chunks = plaintext.chunks(PLAINTEXT_CHUNK_LEN).peekable();
        // An empty plaintext yields no chunks at all; it is a single empty
        // `finalize_last`.
        let mut last: &[u8] = &[];
        while let Some(chunk) = chunks.next() {
            if chunks.peek().is_none() {
                last = chunk;
                break;
            }
            out.extend_from_slice(&enc.encrypt_chunk(chunk).expect("encrypt chunk"));
        }
        let (tail, _md5) = enc.finalize_last(last).expect("finalize");
        out.extend_from_slice(&tail);
        out
    }

    async fn decode_all(blob: &[u8], suite: Option<&dyn SourceCryptoSuite>) -> Vec<u8> {
        let mut reader = std::io::Cursor::new(blob.to_vec());
        let mut out: Vec<u8> = Vec::new();
        decode_into(&mut reader, &mut out, suite, None, None)
            .await
            .expect("decode");
        out
    }

    #[tokio::test]
    async fn decode_plaintext_passes_bytes_through() {
        let body = b"the quick brown fox".to_vec();
        assert_eq!(decode_all(&body, None).await, body);
    }

    /// A plaintext larger than one buffer read, so the passthrough arm's loop
    /// runs more than once.
    #[tokio::test]
    async fn decode_plaintext_handles_multiple_reads() {
        let body: Vec<u8> = (0..(PLAINTEXT_CHUNK_LEN * 3 + 17))
            .map(|i| (i % 251) as u8)
            .collect();
        assert_eq!(decode_all(&body, None).await, body);
    }

    /// The boundary case the framing loop exists for: an object spanning SEVERAL
    /// ciphertext frames plus a partial trailer, so `decrypt_chunk` runs more
    /// than once and `decrypt_last` gets a short final frame. A single-frame test
    /// passes with an off-by-one in the `> CIPHERTEXT_FRAME` comparison; this one
    /// does not.
    #[tokio::test]
    async fn decode_encrypted_round_trips_a_multi_frame_object() {
        let suite = DrivenCryptoSuite::new(SourceKey::generate());
        let plaintext: Vec<u8> = (0..(PLAINTEXT_CHUNK_LEN * 3 + 1_234))
            .map(|i| (i % 253) as u8)
            .collect();
        let blob = encrypt_object(&suite, &plaintext);
        let got = decode_all(&blob, Some(&suite as &dyn SourceCryptoSuite)).await;
        assert_eq!(got, plaintext);
    }

    /// Exactly one full frame plus nothing: the trailer is EMPTY, which is the
    /// other side of the same off-by-one.
    #[tokio::test]
    async fn decode_encrypted_round_trips_an_exact_frame_multiple() {
        let suite = DrivenCryptoSuite::new(SourceKey::generate());
        let plaintext: Vec<u8> = (0..(PLAINTEXT_CHUNK_LEN * 2))
            .map(|i| (i % 199) as u8)
            .collect();
        let blob = encrypt_object(&suite, &plaintext);
        assert_eq!(
            decode_all(&blob, Some(&suite as &dyn SourceCryptoSuite)).await,
            plaintext
        );
    }

    #[tokio::test]
    async fn decode_encrypted_round_trips_an_empty_object() {
        let suite = DrivenCryptoSuite::new(SourceKey::generate());
        let blob = encrypt_object(&suite, &[]);
        assert!(decode_all(&blob, Some(&suite as &dyn SourceCryptoSuite))
            .await
            .is_empty());
    }

    /// A DIFFERENT source key must not decrypt: the AEAD tag fails rather than
    /// yielding garbage plaintext that would then be hash-checked.
    #[tokio::test]
    async fn decode_encrypted_rejects_the_wrong_key() {
        let writer_suite = DrivenCryptoSuite::new(SourceKey::generate());
        let reader_suite = DrivenCryptoSuite::new(SourceKey::generate());
        let blob = encrypt_object(&writer_suite, b"secret bytes");
        let mut reader = std::io::Cursor::new(blob);
        let mut out: Vec<u8> = Vec::new();
        let err = decode_into(
            &mut reader,
            &mut out,
            Some(&reader_suite as &dyn SourceCryptoSuite),
            None,
            None,
        )
        .await
        .expect_err("a foreign key must not decrypt");
        assert!(
            err.to_string().contains("decrypt"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn decode_enforces_the_max_bytes_cap() {
        let body = vec![7u8; 4096];
        let mut reader = std::io::Cursor::new(body);
        let mut out: Vec<u8> = Vec::new();
        let err = decode_into(&mut reader, &mut out, None, None, Some(1024))
            .await
            .expect_err("the cap must be enforced");
        assert!(err.to_string().contains("cap"), "unexpected error: {err}");
    }

    #[test]
    fn verify_rejects_a_hash_mismatch_and_a_length_mismatch() {
        let row = row_for(b"hello");
        assert!(verify_plaintext(blake3::hash(b"hello").as_bytes(), 5, &row).is_ok());
        // Right length, wrong bytes.
        let wrong = verify_plaintext(blake3::hash(b"world").as_bytes(), 5, &row)
            .expect_err("hash mismatch must fail");
        assert!(wrong.contains("blake3"), "{wrong}");
        // Right bytes, wrong recorded length (a drifted state row).
        let short = verify_plaintext(blake3::hash(b"hello").as_bytes(), 4, &row)
            .expect_err("length mismatch must fail");
        assert!(short.contains("length"), "{short}");
    }

    #[test]
    fn join_relative_builds_nested_paths_component_by_component() {
        let root = Path::new("/tmp/dest");
        let joined = join_relative(root, &rel("a/b/c.txt")).expect("joined");
        assert_eq!(joined, root.join("a").join("b").join("c.txt"));
        assert_eq!(
            joined.components().count(),
            root.components().count() + 3,
            "each segment must be its own path component"
        );
    }

    #[test]
    fn temp_path_is_a_sibling_of_the_destination() {
        let temp = temp_path_for(Path::new("/tmp/dest/a/b.txt"));
        assert_eq!(temp.parent(), Some(Path::new("/tmp/dest/a")));
        assert!(temp
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("b.txt") && n.ends_with(TEMP_SUFFIX)));
    }

    #[test]
    fn report_ok_ignores_skips_but_not_failures() {
        let mut report = RestoreReport::default();
        report.files.push(FileReport {
            relative_path: rel("a.txt"),
            outcome: FileOutcome::Restored {
                bytes: 10,
                from_bundle: false,
            },
        });
        report.files.push(FileReport {
            relative_path: rel("b.txt"),
            outcome: FileOutcome::Skipped(SkipReason::NotUploaded),
        });
        assert!(report.ok(), "a skip alone is not a failure");
        assert_eq!(
            (report.restored(), report.skipped(), report.failed()),
            (1, 1, 0)
        );
        assert_eq!(report.bytes_restored(), 10);

        report.files.push(FileReport {
            relative_path: rel("c.txt"),
            outcome: FileOutcome::Failed("dead object".to_string()),
        });
        assert!(!report.ok(), "a failure sinks the run");
        assert_eq!(
            report
                .failures()
                .map(|(p, _)| p.to_string())
                .collect::<Vec<_>>(),
            vec!["c.txt".to_string()]
        );
    }
}
