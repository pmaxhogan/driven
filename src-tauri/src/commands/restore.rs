//! Restore IPC commands (SPEC s11.5; DESIGN s8.4): `list_remote_tree`,
//! `search_files`, `restore_files`, `get_restore_job`.
//!
//! These back the Restore browser (DESIGN s8.4): browse what is backed up, search
//! by filename / glob, and restore selected files - INCLUDING the encrypted path
//! (decrypt the filename for display + STREAM-decrypt content to disk).
//!
//! ## Navigation reads `file_state`, never Drive (ROADMAP M8)
//!
//! `list_remote_tree` + `search_files` read the LOCAL authoritative metadata
//! (`file_state`, SPEC s2) - they never call Drive. `file_state.relative_path`
//! stores the PLAINTEXT path even for encrypted sources, so the browser shows
//! decrypted names without touching the keystore for display (DESIGN s8.4
//! "decrypts filenames inline"). The tree is a single indexed range scan over the
//! `(source_id, relative_path)` primary key per folder open (10k-file tree
//! < 100ms); search uses FTS5 for prefix terms (< 50ms) and SQLite `GLOB` for
//! wildcard patterns (< 200ms).
//!
//! ## Restore decrypts while STREAMING (DATA-SAFETY + PERF)
//!
//! `restore_files` spawns a BACKGROUND job (it does not block the IPC) and streams
//! `restore:progress` events (SPEC s11.7). For each selected file it downloads the
//! object from Drive by its `file_state.drive_file_id` and, for an encrypted
//! source, DECRYPTS the content while streaming to disk: it reads the 40-byte
//! header, opens a [`ContentDecryptor`](driven_crypto::ContentDecryptor), then
//! decrypts one 64-KiB+tag ciphertext frame at a time into a bounded buffer - so a
//! 1 GiB file never sits whole in RAM (RSS stays well under 200 MB). The plaintext
//! is verified against the stored BLAKE3 and written atomically (temp + fsync +
//! rename) into the dialog-approved destination directory.
//!
//! ## Untrusted-webview path safety (SPEC s11.6.1)
//!
//! The restore destination is the security-critical untrusted input. It is NEVER
//! a raw webview path: the webview calls `pick_folder_dialog` (backend-owned
//! native picker) which mints a one-shot dialog TOKEN bound to the chosen folder;
//! `restore_files` takes that token, resolves it to the approved root, and confines
//! every write under it via [`validate_restore_dest`] (no `..`, no absolute, no
//! symlink-at-leaf, atomic write). The `prefix` to `list_remote_tree` is a
//! Drive-relative PLAINTEXT path, validated as a printable, `/`-separated,
//! length-bounded string (NOT a local path).

use std::sync::Arc;

use blake3::Hasher as Blake3;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::io::AsyncReadExt;

use driven_core::crypto_provider::CryptoResolution;
use driven_core::state::RestoreFileRow;
use driven_core::types::{ErrorCode, SourceId};
use driven_crypto::{ContentDecryptor, SourceCryptoSuite, HEADER_LEN};
use driven_drive::remote_store::RemoteStore;

use crate::app_state::AppState;
use crate::commands::dtos::{
    FileSearchHitDto, RemoteEntryDto, RestoreFileProgress, RestoreFileState, RestoreItem,
    RestoreJobId, RestoreJobStatus,
};
use crate::commands::{validate_restore_dest, CommandError, CommandResult, DialogToken};
use crate::events::EVENT_RESTORE_PROGRESS;

/// Tracing target for the restore command layer.
const TARGET: &str = "driven::app::restore";

/// Max nodes `list_remote_tree` returns for one folder open. A folder with more
/// immediate children than this is truncated (the UI shows the first page); the
/// underlying range scan is bounded so a pathological tree cannot blow up the IPC.
const MAX_TREE_NODES: u32 = 5_000;

/// Underlying `file_state` row cap for one tree open. A folder's range scan can
/// touch the whole subtree (to derive immediate sub-folders), so this bounds the
/// scan independently of the returned-node cap above.
const MAX_TREE_SCAN_ROWS: u32 = 100_000;

/// Max search hits returned to the webview for one query (SPEC s11.6.1 bound).
const MAX_SEARCH_LIMIT: u32 = 1_000;

/// Max length (bytes) of the `list_remote_tree` prefix (SPEC s11.6.1 bound). A
/// Drive-relative path well under any real depth.
const MAX_PREFIX_LEN: usize = 4_096;

/// Max length (bytes) of a `search_files` query (SPEC s11.6.1 bound).
const MAX_QUERY_LEN: usize = 1_024;

/// Max files one `restore_files` call may select (SPEC s11.6.1 bound), so a
/// hostile / buggy renderer cannot queue an unbounded job.
const MAX_RESTORE_ITEMS: usize = 100_000;

/// The plaintext chunk size the executor encrypts at (DESIGN s7.1: 64 KiB), so
/// the ciphertext frame is this + the 16-byte Poly1305 tag. The restore decrypt
/// MUST read at the SAME frame boundary the encryptor used (STREAM BE32 is
/// chunk-boundary sensitive).
const PLAINTEXT_CHUNK_LEN: usize = 64 * 1024;
/// XChaCha20-Poly1305 tag length appended to each encrypted chunk.
const TAG_LEN: usize = 16;
/// One ciphertext frame on disk: a full plaintext chunk plus its tag.
const CIPHERTEXT_FRAME: usize = PLAINTEXT_CHUNK_LEN + TAG_LEN;

/// `list_remote_tree(source_id, prefix)` - the immediate children (sub-folders +
/// files) of `prefix` in the backed-up tree (SPEC s11.5; DESIGN s8.4).
///
/// Reads `file_state` (LOCAL metadata), never Drive (ROADMAP M8). For an encrypted
/// source the names are already plaintext (file_state stores the plaintext path).
/// `prefix` is a Drive-relative PLAINTEXT path (empty = source root), validated as
/// a printable, `/`-separated, length-bounded string (SPEC s11.6.1) - NOT a local
/// path.
#[tauri::command]
pub async fn list_remote_tree(
    state: State<'_, AppState>,
    source_id: SourceId,
    prefix: String,
) -> CommandResult<Vec<RemoteEntryDto>> {
    let prefix = validate_prefix(&prefix)?;

    let rows = state
        .state()
        .list_file_state_under_prefix(source_id, &prefix, MAX_TREE_SCAN_ROWS)
        .await
        .map_err(CommandError::from)?;

    let nodes = derive_immediate_children(&prefix, &rows);
    let truncated = nodes.len() as u32 > MAX_TREE_NODES;
    let nodes: Vec<RemoteEntryDto> = nodes.into_iter().take(MAX_TREE_NODES as usize).collect();

    tracing::debug!(
        target: TARGET,
        %source_id,
        prefix = %prefix,
        scanned = rows.len(),
        returned = nodes.len(),
        truncated,
        "list_remote_tree served from file_state"
    );
    Ok(nodes)
}

/// `search_files(source_id?, query, limit)` - search backed-up files by filename /
/// glob (SPEC s11.5; DESIGN s8.4).
///
/// A query containing a glob metacharacter (`*`, `?`, `[`) routes to the SQLite
/// `GLOB` path (wildcard match, < 200ms); otherwise it routes to FTS5 (prefix /
/// term match, < 50ms). When `source_id` is `Some` the search is scoped to that
/// source. Reads `file_state` / its FTS index, never Drive; the returned paths are
/// plaintext (decrypted display for encrypted sources, per SPEC s2).
#[tauri::command]
pub async fn search_files(
    state: State<'_, AppState>,
    source_id: Option<SourceId>,
    query: String,
    limit: u32,
) -> CommandResult<Vec<FileSearchHitDto>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    if query.len() > MAX_QUERY_LEN {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "search query is too long",
        ));
    }
    let limit = limit.clamp(1, MAX_SEARCH_LIMIT);

    // Route: a glob metacharacter -> the GLOB path (wildcard); else FTS5 (terms).
    let hits = if is_glob_query(query) {
        state
            .state()
            .search_files_glob(source_id, query, limit)
            .await
            .map_err(CommandError::from)?
    } else {
        state
            .state()
            .search_files(source_id, query, limit)
            .await
            .map_err(CommandError::from)?
    };

    let dtos: Vec<FileSearchHitDto> = hits
        .into_iter()
        .map(|h| FileSearchHitDto {
            source_id: h.source_id.to_string(),
            relative_path: h.relative_path.as_str().to_string(),
            status: file_state_status_str(h.status).to_string(),
            restorable: h.drive_file_id.is_some(),
        })
        .collect();

    tracing::debug!(target: TARGET, glob = is_glob_query(query), returned = dtos.len(), "search_files served");
    Ok(dtos)
}

/// `restore_files(items, dest_token)` - restore selected files to a local folder
/// (SPEC s11.5; DESIGN s8.4).
///
/// Spawns a BACKGROUND job and returns its id immediately (the IPC does not block
/// on the download/decrypt). The job streams `restore:progress`
/// ([`RestoreJobStatus`]) and records each snapshot on [`AppState`] so
/// `get_restore_job` can serve a late subscriber.
///
/// SPEC s11.6.1: `dest_token` is a one-shot dialog token from `pick_folder_dialog`
/// (the user-approved destination DIRECTORY); the webview never supplies a raw
/// path. Every per-file write is confined under the approved root via
/// [`validate_restore_dest`] and written atomically.
#[tauri::command]
pub async fn restore_files(
    app: AppHandle,
    state: State<'_, AppState>,
    items: Vec<RestoreItem>,
    dest_token: String,
) -> CommandResult<RestoreJobId> {
    if items.is_empty() {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "no files selected to restore",
        ));
    }
    if items.len() > MAX_RESTORE_ITEMS {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "too many files selected to restore",
        ));
    }

    // SPEC s11.6.1: resolve + CONSUME the one-shot dialog token to the approved
    // destination directory. A missing / replayed token is rejected.
    let dest_dir = state.take_dialog_token(&dest_token).ok_or_else(|| {
        CommandError::with_code(
            ErrorCode::LocalIoError,
            "no matching destination folder; pick a restore folder first",
        )
    })?;
    // The destination directory must exist + be a directory (the user picked it).
    let dest_meta = std::fs::metadata(&dest_dir).map_err(|e| {
        CommandError::with_code(
            ErrorCode::LocalIoError,
            format!("restore destination is unreadable: {e}"),
        )
    })?;
    if !dest_meta.is_dir() {
        return Err(CommandError::with_code(
            ErrorCode::LocalIoError,
            "restore destination is not a directory",
        ));
    }
    let dest_token = DialogToken::for_root(dest_dir.to_string_lossy().to_string());

    // Resolve each selected item to its authoritative file_state row UP FRONT (so
    // a bad selection fails the command rather than the background job): the
    // (source_id, relative_path) pair is the file_state PK; the backend reads the
    // Drive id + size + status from SQLite, never trusting webview-supplied ids.
    let mut resolved: Vec<ResolvedRestore> = Vec::with_capacity(items.len());
    for item in &items {
        let source_id: SourceId = item.source_id.parse().map_err(|e| {
            CommandError::with_code(
                ErrorCode::InvalidInput,
                format!("invalid source id in restore selection: {e}"),
            )
        })?;
        let relative_path = driven_core::types::RelativePath::try_from(item.relative_path.clone())
            .map_err(|e| {
                CommandError::with_code(
                    ErrorCode::InvalidInput,
                    format!("invalid relative path in restore selection: {e}"),
                )
            })?;
        let row = state
            .state()
            .get_file_state(source_id, &relative_path)
            .await
            .map_err(CommandError::from)?
            .ok_or_else(|| {
                CommandError::with_code(
                    ErrorCode::InternalBug,
                    format!("unknown file to restore: {}", item.relative_path),
                )
            })?;
        resolved.push(ResolvedRestore {
            source_id,
            relative_path: row.relative_path.as_str().to_string(),
            size: row.size,
            drive_file_id: row.drive_file_id.clone(),
            hash_blake3: row.hash_blake3,
        });
    }

    let job_id = uuid::Uuid::new_v4().to_string();

    // Build the initial (all-pending) status, seed it, and emit the first tick so
    // the webview shows the job immediately.
    let mut status = RestoreJobStatus {
        job_id: job_id.clone(),
        total_files: resolved.len() as u32,
        completed_files: 0,
        failed_files: 0,
        total_bytes: resolved.iter().map(|r| r.size).sum(),
        bytes_done: 0,
        current_file: None,
        done: false,
        files: resolved
            .iter()
            .map(|r| RestoreFileProgress {
                relative_path: r.relative_path.clone(),
                state: RestoreFileState::Pending,
                bytes_done: 0,
                bytes_total: r.size,
                error_code: None,
            })
            .collect(),
    };
    state.put_restore_job(status.clone());
    emit_progress(&app, &status);

    // Build the per-account remote stores + crypto suites the job will use. We
    // build them here (on the command, with `State` access) and MOVE them into the
    // spawned task, since the spawned task cannot hold `State<'_, AppState>`.
    let mut plans: Vec<RestorePlan> = Vec::with_capacity(resolved.len());
    // Cache one remote store per account (multiple files often share an account).
    let mut store_cache: std::collections::HashMap<
        driven_core::types::AccountId,
        Arc<dyn RemoteStore>,
    > = std::collections::HashMap::new();

    // Resolve the source -> account map once.
    let sources = state
        .state()
        .list_sources()
        .await
        .map_err(CommandError::from)?;
    for r in resolved {
        let source = sources
            .iter()
            .find(|s| s.id == r.source_id)
            .ok_or_else(|| {
                CommandError::with_code(
                    ErrorCode::InternalBug,
                    format!("source not found for restore: {}", r.source_id),
                )
            })?;
        let account_id = source.account_id;
        let store = match store_cache.get(&account_id) {
            Some(s) => s.clone(),
            None => {
                let s = crate::commands::sources::build_restore_store(state.inner(), account_id)?;
                store_cache.insert(account_id, s.clone());
                s
            }
        };
        // Resolve the per-source crypto suite (fail-closed) via the account's live
        // provider, so an encrypted file decrypts and an unencrypted one streams
        // raw. An account with no running handle (e.g. needs_reauth) cannot restore
        // an encrypted file - the resolution is recorded and the file fails closed.
        let crypto = resolve_suite(
            state.inner(),
            account_id,
            r.source_id,
            source.encryption_enabled,
        );
        plans.push(RestorePlan {
            file: r,
            store,
            crypto,
        });
    }

    // Spawn the background job. It owns the plans + the dest token; it drives each
    // file, updates + emits + records the status, and never blocks the IPC.
    let app_for_job = app.clone();
    tauri::async_runtime::spawn(async move {
        run_restore_job(app_for_job, plans, dest_token, &mut status).await;
    });

    Ok(RestoreJobId(job_id))
}

/// `get_restore_job(job)` - the current status snapshot of a restore job (SPEC
/// s11.5). Serves a late / reconnected subscriber that missed the live
/// `restore:progress` stream. An unknown id surfaces an error.
#[tauri::command]
pub async fn get_restore_job(
    state: State<'_, AppState>,
    job: RestoreJobId,
) -> CommandResult<RestoreJobStatus> {
    state.restore_job(&job.0).ok_or_else(|| {
        CommandError::with_code(
            ErrorCode::InternalBug,
            format!("unknown restore job: {}", job.0),
        )
    })
}

// -----------------------------------------------------------------------------
// Background job
// -----------------------------------------------------------------------------

/// One file resolved to its authoritative `file_state` fields (M8).
struct ResolvedRestore {
    source_id: SourceId,
    relative_path: String,
    size: u64,
    drive_file_id: Option<String>,
    hash_blake3: [u8; 32],
}

/// One file's restore plan: the resolved file plus the store to download from and
/// the crypto verdict for its source.
struct RestorePlan {
    file: ResolvedRestore,
    store: Arc<dyn RemoteStore>,
    crypto: SuiteVerdict,
}

/// The per-source crypto verdict captured for a restore (mirrors
/// [`CryptoResolution`] but owns the suite so it can move into the spawned task).
enum SuiteVerdict {
    /// Unencrypted source: stream the downloaded bytes straight to disk.
    Plaintext,
    /// Encrypted source: decrypt the downloaded ciphertext while streaming.
    Suite(Arc<dyn SourceCryptoSuite>),
    /// Encrypted source whose key is unavailable: fail closed (cannot decrypt).
    Unavailable,
}

/// Drive the restore job to completion: for each file, restore it, then update +
/// emit + record the job status. Always emits a final `done` status.
async fn run_restore_job(
    app: AppHandle,
    plans: Vec<RestorePlan>,
    dest_token: DialogToken,
    status: &mut RestoreJobStatus,
) {
    for (idx, plan) in plans.iter().enumerate() {
        // Mark this file as restoring + set it as the current file.
        status.current_file = Some(plan.file.relative_path.clone());
        if let Some(f) = status.files.get_mut(idx) {
            f.state = RestoreFileState::Restoring;
        }
        push_status(&app, status);

        let outcome = restore_one_file(
            &plan.file,
            plan.store.as_ref(),
            &plan.crypto,
            &dest_token,
            |bytes_done| {
                // Per-file streamed progress: update this file's bytes + the overall
                // bytes-done, then emit so the UI advances the bar mid-file.
                if let Some(f) = status_file_mut(status, idx) {
                    let prev = f.bytes_done;
                    f.bytes_done = bytes_done;
                    let delta = bytes_done.saturating_sub(prev);
                    status.bytes_done = status.bytes_done.saturating_add(delta);
                }
                push_status(&app, status);
            },
        )
        .await;

        match outcome {
            Ok(()) => {
                if let Some(f) = status.files.get_mut(idx) {
                    // Ensure the file's bytes-done reflects the full size on success.
                    let remaining = f.bytes_total.saturating_sub(f.bytes_done);
                    f.bytes_done = f.bytes_total;
                    status.bytes_done = status.bytes_done.saturating_add(remaining);
                    f.state = RestoreFileState::Done;
                }
                status.completed_files = status.completed_files.saturating_add(1);
            }
            Err(code) => {
                if let Some(f) = status.files.get_mut(idx) {
                    f.state = RestoreFileState::Failed;
                    f.error_code = Some(code.code().to_string());
                }
                status.failed_files = status.failed_files.saturating_add(1);
                tracing::warn!(
                    target: TARGET,
                    file = %plan.file.relative_path,
                    code = %code.code(),
                    "restore file failed"
                );
            }
        }
        push_status(&app, status);
    }

    status.current_file = None;
    status.done = true;
    push_status(&app, status);
    tracing::info!(
        target: TARGET,
        job_id = %status.job_id,
        completed = status.completed_files,
        failed = status.failed_files,
        "restore job done"
    );
}

/// Record + emit a status snapshot (records on AppState via the app-managed state
/// so `get_restore_job` stays current, then emits `restore:progress`).
fn push_status(app: &AppHandle, status: &RestoreJobStatus) {
    if let Some(state) = app.try_state::<AppState>() {
        state.put_restore_job(status.clone());
    }
    emit_progress(app, status);
}

/// Mutable access to file `idx`'s progress, if present.
fn status_file_mut(status: &mut RestoreJobStatus, idx: usize) -> Option<&mut RestoreFileProgress> {
    status.files.get_mut(idx)
}

/// Emit one `restore:progress` event (SPEC s11.7). A failed emit is logged, never
/// fatal (the snapshot is still recorded for `get_restore_job`).
fn emit_progress(app: &AppHandle, status: &RestoreJobStatus) {
    if let Err(err) = app.emit(EVENT_RESTORE_PROGRESS, status) {
        tracing::debug!(target: TARGET, %err, "emit restore:progress failed");
    }
}

/// Restore ONE file: download from Drive, (for encrypted sources) STREAM-decrypt,
/// verify the plaintext BLAKE3, and write atomically into the dest dir.
/// `on_progress` is called with the cumulative plaintext bytes written so far so
/// the caller can advance the UI mid-file. Returns the SPEC s24 error code on
/// failure (mapped to a translatable key on the file's progress entry).
async fn restore_one_file<F: FnMut(u64)>(
    file: &ResolvedRestore,
    store: &dyn RemoteStore,
    crypto: &SuiteVerdict,
    dest_token: &DialogToken,
    mut on_progress: F,
) -> Result<(), ErrorCode> {
    // A file never uploaded has no Drive object to restore.
    let drive_file_id = file
        .drive_file_id
        .as_deref()
        .ok_or(ErrorCode::InternalBug)?;

    // Fail closed: an encrypted source whose key is unavailable cannot decrypt.
    let suite: Option<&dyn SourceCryptoSuite> = match crypto {
        SuiteVerdict::Plaintext => None,
        SuiteVerdict::Suite(s) => Some(s.as_ref()),
        SuiteVerdict::Unavailable => return Err(ErrorCode::CryptoKeyMissing),
    };

    // Resolve + confine the destination (SPEC s11.6.1): re-create the file's
    // relative tree under the approved root, no traversal, no symlink-at-leaf.
    let dest = validate_restore_dest(dest_token, &file.relative_path).map_err(|e| e.code)?;

    // Open the Drive download stream.
    let mut reader = store
        .download(drive_file_id)
        .await
        .map_err(|_| ErrorCode::DriveUnreachable)?
        .0;

    // Atomic write: stream into a sibling temp file, then rename over the final
    // name. A failure best-effort removes the temp file (SPEC s11.6.1 step 5).
    let parent = dest.parent().ok_or(ErrorCode::LocalIoError)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let leaf = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("restore");
    let tmp = parent.join(format!(".driven-restore-tmp.{leaf}.{nonce}"));

    let result = stream_to_disk(&mut reader, &tmp, suite, file, &mut on_progress).await;

    match result {
        Ok(()) => {
            // Atomically place the verified plaintext at its final name.
            if let Err(e) = tokio::fs::rename(&tmp, &dest).await {
                let _ = tokio::fs::remove_file(&tmp).await;
                tracing::warn!(target: TARGET, file = %file.relative_path, %e, "restore atomic rename failed");
                return Err(ErrorCode::LocalIoError);
            }
            Ok(())
        }
        Err(code) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(code)
        }
    }
}

/// Stream the download into `tmp`, decrypting if `suite` is `Some`, hashing the
/// plaintext with BLAKE3, and verifying it against `file.hash_blake3`. Bounded
/// memory: at most ~2 ciphertext frames (~128 KiB) are buffered at once, so a
/// 1 GiB file never sits whole in RAM (PERF acceptance).
async fn stream_to_disk<R, F>(
    reader: &mut R,
    tmp: &std::path::Path,
    suite: Option<&dyn SourceCryptoSuite>,
    file: &ResolvedRestore,
    on_progress: &mut F,
) -> Result<(), ErrorCode>
where
    R: tokio::io::AsyncRead + Unpin,
    F: FnMut(u64),
{
    use tokio::io::AsyncWriteExt;

    let out = tokio::fs::File::create(tmp).await.map_err(map_io_err)?;
    let mut writer = tokio::io::BufWriter::new(out);
    let mut hasher = Blake3::new();
    let mut written: u64 = 0;

    match suite {
        // --- plaintext source: copy bytes straight through, hashing as we go ----
        None => {
            let mut buf = vec![0u8; CIPHERTEXT_FRAME];
            loop {
                let n = reader.read(&mut buf).await.map_err(map_io_err)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                writer.write_all(&buf[..n]).await.map_err(map_io_err)?;
                written = written.saturating_add(n as u64);
                on_progress(written);
            }
        }
        // --- encrypted source: STREAM-decrypt frame by frame -------------------
        Some(suite) => {
            // 1) Read the fixed 40-byte header (magic || nonce) to open the
            //    decryptor. A short read here means a truncated / non-Driven object.
            let mut header = [0u8; HEADER_LEN];
            reader
                .read_exact(&mut header)
                .await
                .map_err(|_| ErrorCode::CryptoDecryptFailed)?;
            let mut dec: Box<dyn ContentDecryptor> = suite
                .content_decryptor(&header)
                .map_err(|_| ErrorCode::CryptoDecryptFailed)?;

            // 2) Decrypt one ciphertext frame at a time. We MUST decrypt at the
            //    SAME 64-KiB+tag boundary the encryptor used (STREAM BE32 is
            //    boundary-sensitive). The trick for "is this the LAST frame?": a
            //    frame is non-final iff there are MORE bytes after it. So we keep a
            //    rolling buffer and only `decrypt_chunk` a full frame once we have
            //    read STRICTLY MORE than one frame; whatever remains at EOF
            //    (<= one frame) is the `decrypt_last` chunk. This bounds the buffer
            //    to ~2 frames regardless of file size.
            let mut buf: Vec<u8> = Vec::with_capacity(CIPHERTEXT_FRAME * 2);
            let mut read_chunk = vec![0u8; CIPHERTEXT_FRAME];
            let mut eof = false;
            while !eof {
                // Fill the buffer until it holds > one frame or we hit EOF.
                while buf.len() <= CIPHERTEXT_FRAME && !eof {
                    let n = reader.read(&mut read_chunk).await.map_err(map_io_err)?;
                    if n == 0 {
                        eof = true;
                    } else {
                        buf.extend_from_slice(&read_chunk[..n]);
                    }
                }
                // While we have strictly more than a frame buffered, the leading
                // frame is definitely NOT the last -> decrypt_chunk it.
                while buf.len() > CIPHERTEXT_FRAME {
                    let frame: Vec<u8> = buf.drain(..CIPHERTEXT_FRAME).collect();
                    let pt = dec
                        .decrypt_chunk(&frame)
                        .map_err(|_| ErrorCode::CryptoDecryptFailed)?;
                    hasher.update(&pt);
                    writer.write_all(&pt).await.map_err(map_io_err)?;
                    written = written.saturating_add(pt.len() as u64);
                    on_progress(written);
                    // A non-final decryptor cannot be re-borrowed after the last
                    // chunk; we only reach `decrypt_last` once the loop exits.
                }
            }
            // 3) The remaining <= one frame is the final chunk. (For a single-frame
            //    or empty file this is the only chunk.)
            let pt = dec
                .decrypt_last(&buf)
                .map_err(|_| ErrorCode::CryptoDecryptFailed)?;
            hasher.update(&pt);
            writer.write_all(&pt).await.map_err(map_io_err)?;
            written = written.saturating_add(pt.len() as u64);
            on_progress(written);
        }
    }

    writer.flush().await.map_err(map_io_err)?;
    // fsync the file data before the caller renames it into place.
    writer.get_ref().sync_all().await.map_err(map_io_err)?;

    // DATA-SAFETY: verify the restored plaintext matches the stored BLAKE3. A
    // mismatch means a wrong decrypt / corrupted object - refuse to present it as
    // the user's data.
    let digest = hasher.finalize();
    if digest.as_bytes() != &file.hash_blake3 {
        tracing::warn!(
            target: TARGET,
            file = %file.relative_path,
            "restored plaintext blake3 mismatch; refusing the file"
        );
        return Err(ErrorCode::CryptoDecryptFailed);
    }
    // Sanity: the plaintext length should match the recorded size.
    if written != file.size {
        tracing::warn!(
            target: TARGET,
            file = %file.relative_path,
            written,
            expected = file.size,
            "restored plaintext length mismatch"
        );
        return Err(ErrorCode::CryptoDecryptFailed);
    }
    Ok(())
}

/// Map a disk write error to the SPEC s24 code: out-of-space -> `local.disk_full`,
/// everything else -> `local.io_error`. Disk-full is detected via the raw OS error
/// code (ENOSPC == 28 on Unix; ERROR_DISK_FULL == 112 / ERROR_HANDLE_DISK_FULL ==
/// 39 on Windows), since `std::io::ErrorKind::StorageFull` is not yet stable.
fn map_io_err(e: std::io::Error) -> ErrorCode {
    if let Some(raw) = e.raw_os_error() {
        #[cfg(unix)]
        const ENOSPC: i32 = 28;
        #[cfg(unix)]
        if raw == ENOSPC {
            return ErrorCode::LocalDiskFull;
        }
        #[cfg(windows)]
        if raw == 112 || raw == 39 {
            return ErrorCode::LocalDiskFull;
        }
        let _ = raw;
    }
    ErrorCode::LocalIoError
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

/// Resolve the per-source crypto verdict for a restore via the account's live
/// crypto provider (fail-closed). An account with no running handle (never
/// spawned / needs_reauth) yields `Unavailable` for an ENCRYPTED source so its
/// files fail closed with `crypto.key_missing` rather than streaming ciphertext
/// to disk; an unencrypted source resolves `Plaintext` (no handle needed since
/// there is nothing to decrypt). `encryption_enabled` is the source row's flag, so
/// the no-handle path can distinguish the two without touching the keystore.
fn resolve_suite(
    state: &AppState,
    account_id: driven_core::types::AccountId,
    source_id: SourceId,
    encryption_enabled: bool,
) -> SuiteVerdict {
    use driven_core::crypto_provider::CryptoProvider;
    match state.account(account_id) {
        Some(handle) => match handle.crypto.resolve(&source_id) {
            CryptoResolution::Plaintext => SuiteVerdict::Plaintext,
            CryptoResolution::Suite(s) => SuiteVerdict::Suite(s),
            CryptoResolution::Unavailable => SuiteVerdict::Unavailable,
        },
        // No running handle: we cannot resolve a per-source key. Fail an ENCRYPTED
        // source closed (Unavailable -> crypto.key_missing); an UNENCRYPTED source
        // has nothing to decrypt, so stream its plaintext (the per-file blake3
        // verify still guards against any corruption).
        None => {
            if encryption_enabled {
                SuiteVerdict::Unavailable
            } else {
                SuiteVerdict::Plaintext
            }
        }
    }
}

/// Validate a `list_remote_tree` prefix (SPEC s11.6.1): a Drive-relative,
/// `/`-separated, printable, length-bounded PLAINTEXT path - NOT a local path. An
/// empty prefix (the source root) is allowed. Returns the normalized prefix (no
/// leading / trailing slash). Rejects `..`, NUL, control chars, and a too-long
/// input.
fn validate_prefix(prefix: &str) -> CommandResult<String> {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.len() > MAX_PREFIX_LEN {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "tree prefix is too long",
        ));
    }
    if trimmed.contains('\0') || trimmed.chars().any(|c| c.is_control()) {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "tree prefix contains control characters",
        ));
    }
    if trimmed.split('/').any(|seg| seg == ".." || seg == ".") {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "tree prefix must not contain relative segments",
        ));
    }
    Ok(trimmed.to_string())
}

/// `true` if `query` carries a SQLite GLOB metacharacter (`*`, `?`, `[`), routing
/// it to the wildcard search path instead of FTS5.
fn is_glob_query(query: &str) -> bool {
    query.contains('*') || query.contains('?') || query.contains('[')
}

/// Derive the IMMEDIATE children (sub-folders + files) of `prefix` from the
/// subtree rows. A row whose path equals `prefix/<name>` is a direct FILE child; a
/// row deeper than that (`prefix/<dir>/...`) contributes its first segment as a
/// direct FOLDER child (deduped). Folders sort before files, each alphabetically.
fn derive_immediate_children(prefix: &str, rows: &[RestoreFileRow]) -> Vec<RemoteEntryDto> {
    use std::collections::BTreeMap;

    // The path segment that follows the prefix (the "local" portion under it).
    let strip = |path: &str| -> Option<String> {
        if prefix.is_empty() {
            return Some(path.to_string());
        }
        let with_slash = format!("{prefix}/");
        path.strip_prefix(&with_slash).map(|s| s.to_string())
    };

    // Folders: name -> () ; Files: name -> the row (so we carry size/status/id).
    let mut folders: BTreeMap<String, ()> = BTreeMap::new();
    let mut files: BTreeMap<String, &RestoreFileRow> = BTreeMap::new();

    for row in rows {
        let Some(local) = strip(row.relative_path.as_str()) else {
            continue;
        };
        if local.is_empty() {
            continue;
        }
        match local.split_once('/') {
            // Deeper than one level -> the first segment is a sub-folder.
            Some((dir, _rest)) => {
                folders.insert(dir.to_string(), ());
            }
            // Exactly one level -> a direct file child.
            None => {
                files.insert(local, row);
            }
        }
    }

    let prefix_join = |name: &str| -> String {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        }
    };

    let mut out: Vec<RemoteEntryDto> = Vec::with_capacity(folders.len() + files.len());
    for (name, _) in folders {
        out.push(RemoteEntryDto {
            relative_path: prefix_join(&name),
            name,
            is_dir: true,
            size: 0,
            status: None,
            restorable: false,
        });
    }
    for (name, row) in files {
        out.push(RemoteEntryDto {
            relative_path: prefix_join(&name),
            name,
            is_dir: false,
            size: row.size,
            status: Some(file_state_status_str(row.status).to_string()),
            restorable: row.drive_file_id.is_some(),
        });
    }
    out
}

/// The serialized (snake_case) discriminant of a [`FileStateStatus`], matching the
/// `file_state.status` column + the TS `FileStateStatus` union.
fn file_state_status_str(status: driven_core::types::FileStateStatus) -> &'static str {
    use driven_core::types::FileStateStatus as S;
    match status {
        S::Synced => "synced",
        S::Pending => "pending",
        S::Corrupt => "corrupt",
        S::Locked => "locked",
        S::Error => "error",
        S::ExcludedOrphan => "excluded_orphan",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_core::types::{FileStateStatus, SourceId};

    fn row(path: &str, size: u64, drive: Option<&str>) -> RestoreFileRow {
        RestoreFileRow {
            source_id: SourceId::new_v4(),
            relative_path: driven_core::types::RelativePath::try_from(path.to_string()).unwrap(),
            size,
            status: FileStateStatus::Synced,
            drive_file_id: drive.map(|s| s.to_string()),
        }
    }

    #[test]
    fn derive_children_at_root_separates_folders_and_files() {
        let rows = vec![
            row("a.txt", 10, Some("d1")),
            row("src/main.rs", 20, Some("d2")),
            row("src/lib.rs", 30, Some("d3")),
            row("docs/readme.md", 40, Some("d4")),
        ];
        let nodes = derive_immediate_children("", &rows);
        // Folders first (docs, src), then files (a.txt).
        let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["docs", "src", "a.txt"]);
        assert!(nodes[0].is_dir && nodes[1].is_dir);
        assert!(!nodes[2].is_dir);
        // The file node carries size + restorable + the full relative path.
        assert_eq!(nodes[2].relative_path, "a.txt");
        assert_eq!(nodes[2].size, 10);
        assert!(nodes[2].restorable);
    }

    #[test]
    fn derive_children_under_prefix_lists_immediate_only() {
        let rows = vec![
            row("src/main.rs", 20, Some("d2")),
            row("src/nested/deep.rs", 50, Some("d5")),
        ];
        let nodes = derive_immediate_children("src", &rows);
        // "nested" (folder) before "main.rs" (file). "deep.rs" is NOT a direct
        // child (it is under src/nested), so it does not appear.
        let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["nested", "main.rs"]);
        assert!(nodes[0].is_dir);
        assert_eq!(nodes[0].relative_path, "src/nested");
        assert_eq!(nodes[1].relative_path, "src/main.rs");
    }

    #[test]
    fn derive_children_marks_unuploaded_file_not_restorable() {
        let rows = vec![row("pending.bin", 99, None)];
        let nodes = derive_immediate_children("", &rows);
        assert_eq!(nodes.len(), 1);
        assert!(
            !nodes[0].restorable,
            "a file with no drive id is not restorable"
        );
    }

    #[test]
    fn validate_prefix_normalizes_and_rejects_traversal() {
        assert_eq!(validate_prefix("").unwrap(), "");
        assert_eq!(validate_prefix("/").unwrap(), "");
        assert_eq!(validate_prefix("src/").unwrap(), "src");
        assert_eq!(validate_prefix("/src/nested/").unwrap(), "src/nested");
        assert!(validate_prefix("../escape").is_err());
        assert!(validate_prefix("a/../b").is_err());
        assert!(validate_prefix("a/./b").is_err());
        assert!(validate_prefix("with\0nul").is_err());
    }

    #[test]
    fn glob_routing_detects_metacharacters() {
        assert!(is_glob_query("*.rs"));
        assert!(is_glob_query("src/?.txt"));
        assert!(is_glob_query("[abc].md"));
        assert!(!is_glob_query("plain"));
        assert!(!is_glob_query("foo-bar"));
    }

    // --- streaming decrypt round-trip (DATA-SAFETY + PERF acceptance) ---------

    use driven_crypto::DrivenCryptoSuite;

    /// Encrypt `plaintext` through the SAME `ContentEncryptor` the executor uses
    /// (header || 64-KiB chunks || finalize_last), returning the on-disk ciphertext
    /// blob (exactly what `download` would hand back for an encrypted object).
    fn encrypt_blob(suite: &DrivenCryptoSuite, plaintext: &[u8]) -> Vec<u8> {
        let mut enc = suite.content_encryptor();
        let mut blob = Vec::new();
        blob.extend_from_slice(&enc.header());
        // Chunk at exactly PLAINTEXT_CHUNK_LEN (the executor's READ_BUF), final
        // chunk via finalize_last - the boundary the streaming decrypt expects.
        let mut off = 0;
        while plaintext.len() - off > PLAINTEXT_CHUNK_LEN {
            let ct = enc
                .encrypt_chunk(&plaintext[off..off + PLAINTEXT_CHUNK_LEN])
                .unwrap();
            blob.extend_from_slice(&ct);
            off += PLAINTEXT_CHUNK_LEN;
        }
        let (last, _md5) = enc.finalize_last(&plaintext[off..]).unwrap();
        blob.extend_from_slice(&last);
        blob
    }

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("driven-restore-test-{tag}-{nonce}"))
    }

    fn resolved_for(plaintext: &[u8], rel: &str) -> ResolvedRestore {
        ResolvedRestore {
            source_id: SourceId::new_v4(),
            relative_path: rel.to_string(),
            size: plaintext.len() as u64,
            drive_file_id: Some("d-test".to_string()),
            hash_blake3: *blake3::hash(plaintext).as_bytes(),
        }
    }

    #[tokio::test]
    async fn streaming_decrypt_round_trips_multichunk_encrypted_file() {
        // DATA-SAFETY: an encrypted multi-chunk file must decrypt back to the
        // EXACT plaintext via the streaming sink, with the blake3 verifying. This
        // spans several 64-KiB ciphertext frames so the frame-boundary streaming
        // path (decrypt_chunk loop + decrypt_last) is exercised, NOT a single
        // buffered decrypt.
        let suite = DrivenCryptoSuite::new(driven_crypto::key::SourceKey::generate());
        // ~5.3 MiB: > 80 frames, with a non-frame-aligned tail.
        let plaintext: Vec<u8> = (0..(5 * 1024 * 1024 + 777usize))
            .map(|i| (i % 251) as u8)
            .collect();
        let blob = encrypt_blob(&suite, &plaintext);
        assert_ne!(blob, plaintext, "stored blob must be ciphertext");

        let file = resolved_for(&plaintext, "secret/big.bin");
        let out = tmp_path("multichunk");
        let mut reader = std::io::Cursor::new(blob);
        let mut last_progress = 0u64;
        let res = stream_to_disk(&mut reader, &out, Some(&suite), &file, &mut |done| {
            assert!(done >= last_progress, "progress must be monotonic");
            last_progress = done;
        })
        .await;
        assert!(res.is_ok(), "streaming decrypt must succeed: {res:?}");
        let restored = std::fs::read(&out).unwrap();
        assert_eq!(
            restored, plaintext,
            "decrypted bytes must match the original"
        );
        assert_eq!(
            last_progress,
            plaintext.len() as u64,
            "final progress == size"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[tokio::test]
    async fn streaming_decrypt_round_trips_empty_and_small_files() {
        let suite = DrivenCryptoSuite::new(driven_crypto::key::SourceKey::generate());
        for plaintext in [Vec::new(), b"hello driven restore".to_vec()] {
            let blob = encrypt_blob(&suite, &plaintext);
            let file = resolved_for(&plaintext, "small.txt");
            let out = tmp_path("small");
            let mut reader = std::io::Cursor::new(blob);
            stream_to_disk(&mut reader, &out, Some(&suite), &file, &mut |_| {})
                .await
                .unwrap();
            assert_eq!(std::fs::read(&out).unwrap(), plaintext);
            let _ = std::fs::remove_file(&out);
        }
    }

    #[tokio::test]
    async fn streaming_decrypt_round_trips_exact_frame_multiple() {
        // Edge case: a plaintext that is an EXACT multiple of the chunk size, so
        // the last frame is a FULL frame (the encryptor still finalize_last's it).
        // The streaming decrypt's "> CIPHERTEXT_FRAME" boundary must treat the
        // final full frame as decrypt_last, not decrypt_chunk.
        let suite = DrivenCryptoSuite::new(driven_crypto::key::SourceKey::generate());
        let plaintext: Vec<u8> = (0..(2 * PLAINTEXT_CHUNK_LEN))
            .map(|i| (i % 97) as u8)
            .collect();
        let blob = encrypt_blob(&suite, &plaintext);
        let file = resolved_for(&plaintext, "aligned.bin");
        let out = tmp_path("aligned");
        let mut reader = std::io::Cursor::new(blob);
        stream_to_disk(&mut reader, &out, Some(&suite), &file, &mut |_| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), plaintext);
        let _ = std::fs::remove_file(&out);
    }

    #[tokio::test]
    async fn streaming_plaintext_source_copies_and_verifies() {
        // An unencrypted source streams the downloaded bytes straight through and
        // still verifies blake3.
        let plaintext = b"plain source bytes, no encryption".to_vec();
        let file = resolved_for(&plaintext, "plain.txt");
        let out = tmp_path("plain");
        let mut reader = std::io::Cursor::new(plaintext.clone());
        stream_to_disk(&mut reader, &out, None, &file, &mut |_| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), plaintext);
        let _ = std::fs::remove_file(&out);
    }

    #[tokio::test]
    async fn streaming_decrypt_wrong_key_fails_and_blake3_guard_catches_tamper() {
        // A wrong key must fail the AEAD (crypto.decrypt_failed), NOT silently
        // write garbage.
        let suite = DrivenCryptoSuite::new(driven_crypto::key::SourceKey::generate());
        let other = DrivenCryptoSuite::new(driven_crypto::key::SourceKey::generate());
        let plaintext = b"top secret".to_vec();
        let blob = encrypt_blob(&suite, &plaintext);
        let file = resolved_for(&plaintext, "x.bin");
        let out = tmp_path("wrongkey");
        let mut reader = std::io::Cursor::new(blob);
        let err = stream_to_disk(&mut reader, &out, Some(&other), &file, &mut |_| {})
            .await
            .expect_err("wrong key must fail");
        assert_eq!(err, ErrorCode::CryptoDecryptFailed);
        let _ = std::fs::remove_file(&out);
    }

    #[tokio::test]
    async fn streaming_plaintext_blake3_mismatch_is_refused() {
        // If the downloaded plaintext does not match the recorded blake3 (a
        // corrupted object), the sink refuses the file rather than presenting bad
        // data as the user's file.
        let actual = b"actual bytes".to_vec();
        let mut file = resolved_for(&actual, "c.txt");
        // Record a DIFFERENT hash so the verify fails.
        file.hash_blake3 = *blake3::hash(b"different").as_bytes();
        let out = tmp_path("mismatch");
        let mut reader = std::io::Cursor::new(actual);
        let err = stream_to_disk(&mut reader, &out, None, &file, &mut |_| {})
            .await
            .expect_err("blake3 mismatch must be refused");
        assert_eq!(err, ErrorCode::CryptoDecryptFailed);
        let _ = std::fs::remove_file(&out);
    }

    #[tokio::test]
    async fn decrypt_filename_round_trips_for_display() {
        // DESIGN s8.4: an encrypted source's filename decrypts for display. This
        // exercises the same suite the restore browser would use to show plaintext
        // names (file_state stores plaintext, but the round-trip is the load-
        // bearing correctness property for an encrypted backup).
        let suite = DrivenCryptoSuite::new(driven_crypto::key::SourceKey::generate());
        let dir_ct = suite.encrypt_filename("Documents", &[]).unwrap();
        let file_ct = suite
            .encrypt_filename("taxes-2025.pdf", dir_ct.as_bytes())
            .unwrap();
        assert_ne!(
            file_ct, "taxes-2025.pdf",
            "the Drive name must be ciphertext"
        );
        assert_eq!(
            suite.decrypt_filename(&file_ct, dir_ct.as_bytes()).unwrap(),
            "taxes-2025.pdf",
            "the restore browser decrypts the filename back to plaintext"
        );
    }
}
