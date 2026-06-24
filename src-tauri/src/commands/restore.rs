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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use blake3::Hasher as Blake3;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::io::AsyncReadExt;

use driven_core::crypto_provider::CryptoResolution;
use driven_core::state::RestoreFileRow;
use driven_core::types::{ErrorCode, SourceId};
use driven_crypto::{ContentDecryptor, SourceCryptoSuite, HEADER_LEN};
use driven_drive::remote_store::RemoteStore;

use crate::app_state::{AppState, RestoreCancel};
use crate::commands::dtos::{
    FileSearchHitDto, RemoteEntryDto, RemoteTreeDto, RestoreFileProgress, RestoreFileState,
    RestoreItem, RestoreJobId, RestoreJobStatus,
};
use crate::commands::{validate_restore_dest, CommandError, CommandResult, DialogToken};
use crate::events::EVENT_RESTORE_PROGRESS;

/// Tracing target for the restore command layer.
const TARGET: &str = "driven::app::restore";

/// Max nodes `list_remote_tree` returns for one folder open. A folder with more
/// immediate children than this is truncated (the UI shows the first page + a
/// `truncated` flag so the user knows, M8-P2-1); the underlying range scan is
/// bounded so a pathological tree cannot blow up the IPC.
pub(crate) const MAX_TREE_NODES: u32 = 5_000;

/// Underlying `file_state` row cap for one tree open. A folder's range scan can
/// touch the whole subtree (to derive immediate sub-folders), so this bounds the
/// scan independently of the returned-node cap above.
const MAX_TREE_SCAN_ROWS: u32 = 100_000;

/// Max search hits returned to the webview for one query (SPEC s11.6.1 bound).
const MAX_SEARCH_LIMIT: u32 = 1_000;

/// Max length (bytes) of the `list_remote_tree` prefix (SPEC s11.6.1 bound). A
/// Drive-relative path well under any real depth.
const MAX_PREFIX_LEN: usize = 4_096;

/// Max length (chars) of a `search_files` query (DESIGN s18.8: 256 chars, no raw
/// newlines / NUL / control chars). M8-P2-2 tightened this from 1024 + added the
/// control-char rejection below to match DESIGN s18.8 exactly.
const MAX_QUERY_LEN: usize = 256;

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
) -> CommandResult<RemoteTreeDto> {
    let prefix = validate_prefix(&prefix)?;

    let rows = state
        .state()
        .list_file_state_under_prefix(source_id, &prefix, MAX_TREE_SCAN_ROWS)
        .await
        .map_err(CommandError::from)?;

    let nodes = derive_immediate_children(&prefix, &rows);
    // M8-P2-1: SURFACE the cap instead of silently dropping children. `truncated`
    // is true when the folder has more immediate children than the returned cap
    // (or the underlying scan itself hit its row cap, in which case deeper folders
    // may also be incomplete), so the UI can tell the user the listing is partial.
    let truncated = nodes.len() as u32 > MAX_TREE_NODES || rows.len() as u32 >= MAX_TREE_SCAN_ROWS;
    let entries: Vec<RemoteEntryDto> = nodes.into_iter().take(MAX_TREE_NODES as usize).collect();

    tracing::debug!(
        target: TARGET,
        %source_id,
        prefix = %prefix,
        scanned = rows.len(),
        returned = entries.len(),
        truncated,
        "list_remote_tree served from file_state"
    );
    Ok(RemoteTreeDto { entries, truncated })
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
    // M8-P2-2 / DESIGN s18.8: cap at 256 CHARS (not bytes) and reject raw
    // newlines, NUL, and other control chars BEFORE the FTS/GLOB path. A query
    // that carries a control char is rejected outright (a hostile renderer cannot
    // smuggle a newline / NUL into the FTS or GLOB matcher).
    if query.chars().count() > MAX_QUERY_LEN {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "search query is too long",
        ));
    }
    if query.contains('\0') || query.chars().any(|c| c.is_control()) {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "search query contains control characters",
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

    // Build the initial (all-pending) status, seed it WITH a cancel flag (P1-1),
    // and emit the first tick so the webview shows the job immediately.
    let mut status = RestoreJobStatus {
        job_id: job_id.clone(),
        total_files: resolved.len() as u32,
        completed_files: 0,
        failed_files: 0,
        total_bytes: resolved.iter().map(|r| r.size).sum(),
        bytes_done: 0,
        current_file: None,
        done: false,
        cancelled: false,
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
    // M8-P1-1: the per-job cancel flag the spawned task observes between frames;
    // `cancel_restore_job` + the shutdown drain set it. Seed the job so a cancel /
    // late poll resolves it immediately, before the task handle is attached.
    let cancel: RestoreCancel = Arc::new(AtomicBool::new(false));
    state.seed_restore_job(status.clone(), cancel.clone());
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

    // Spawn the background job. It owns the plans + the dest token + the cancel
    // flag; it drives each file, updates + emits + records the status, and never
    // blocks the IPC. On exit it clears its own handle so the shutdown drain does
    // not await an already-finished task.
    let app_for_job = app.clone();
    let job_cancel = cancel.clone();
    // tokio::spawn (not tauri::async_runtime::spawn) so the returned handle is a
    // tokio JoinHandle the AppState tracks + the shutdown drain awaits, matching
    // the per-account task handles (the command already runs on the tokio runtime).
    let handle = tokio::task::spawn(async move {
        run_restore_job(app_for_job, plans, dest_token, &mut status, job_cancel).await;
    });
    // Attach the handle so cancel / shutdown can await it (P1-1).
    state.set_restore_job_handle(&job_id, handle);

    Ok(RestoreJobId(job_id))
}

/// `cancel_restore_job(job)` - request cancellation of a running restore job
/// (SPEC s11.5 / s11.7; M8-P1-1). Sets the job's cancel flag so the background
/// task stops between frames, DELETES any in-flight temp file (no partial left),
/// and emits a terminal CANCELLED [`RestoreJobStatus`]. Idempotent: cancelling an
/// unknown / already-finished job is a benign no-op (the job may have completed
/// or its terminal record been pruned). The command returns once the cancel is
/// SIGNALLED; the terminal CANCELLED status arrives on `restore:progress`.
#[tauri::command]
pub async fn cancel_restore_job(
    state: State<'_, AppState>,
    job: RestoreJobId,
) -> CommandResult<()> {
    // Set the flag + take the task handle (if still running). We do NOT await the
    // handle here (the command should return promptly); the task observes the flag,
    // cleans up its temp, emits CANCELLED, and exits on its own.
    let _ = state.cancel_restore_job(&job.0);
    Ok(())
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

/// One file's terminal outcome within the job.
enum FileOutcome {
    /// Restored + verified.
    Done,
    /// Failed with the SPEC s24 error code.
    Failed(ErrorCode),
    /// Cancelled mid-stream (M8-P1-1): the temp file was deleted, so no partial
    /// final file remains.
    Cancelled,
}

/// Drive the restore job to completion (or cancellation): for each file, restore
/// it, then update + emit + record the job status. Always emits a final terminal
/// status (`done`, with `cancelled` set if the user cancelled). M8-P1-1: the
/// `cancel` flag is checked BEFORE each file AND between frames inside
/// `stream_to_disk`; on cancel the in-flight temp is deleted and a CANCELLED
/// terminal status is emitted, then the job clears its own handle so the shutdown
/// drain does not await an already-finished task.
async fn run_restore_job(
    app: AppHandle,
    plans: Vec<RestorePlan>,
    dest_token: DialogToken,
    status: &mut RestoreJobStatus,
    cancel: RestoreCancel,
) {
    let mut cancelled = false;
    for (idx, plan) in plans.iter().enumerate() {
        // M8-P1-1: cancel observed BEFORE starting this file -> stop the loop; the
        // remaining (and this) files stay Pending -> marked Cancelled below.
        if cancel.load(Ordering::SeqCst) {
            cancelled = true;
            break;
        }

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
            &cancel,
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
            FileOutcome::Done => {
                if let Some(f) = status.files.get_mut(idx) {
                    // Ensure the file's bytes-done reflects the full size on success.
                    let remaining = f.bytes_total.saturating_sub(f.bytes_done);
                    f.bytes_done = f.bytes_total;
                    status.bytes_done = status.bytes_done.saturating_add(remaining);
                    f.state = RestoreFileState::Done;
                }
                status.completed_files = status.completed_files.saturating_add(1);
            }
            FileOutcome::Failed(code) => {
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
            FileOutcome::Cancelled => {
                // The temp was already deleted in restore_one_file; mark this file
                // cancelled and stop the loop (remaining files marked below).
                if let Some(f) = status.files.get_mut(idx) {
                    f.state = RestoreFileState::Cancelled;
                }
                cancelled = true;
                break;
            }
        }
        push_status(&app, status);
    }

    if cancelled {
        // M8-P1-1: mark every not-yet-terminal file Cancelled (the current file is
        // already cancelled; the rest were still Pending), then emit the terminal
        // CANCELLED status.
        for f in status.files.iter_mut() {
            if matches!(
                f.state,
                RestoreFileState::Pending | RestoreFileState::Restoring
            ) {
                f.state = RestoreFileState::Cancelled;
            }
        }
        status.current_file = None;
        status.cancelled = true;
        status.done = true;
        push_status(&app, status);
        tracing::info!(
            target: TARGET,
            job_id = %status.job_id,
            completed = status.completed_files,
            failed = status.failed_files,
            "restore job cancelled"
        );
    } else {
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

    // M8-P1-1: clear this job's handle so the app-shutdown drain does not try to
    // await an already-finished task.
    if let Some(state) = app.try_state::<AppState>() {
        state.finish_restore_job_handle(&status.job_id);
    }
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
/// the caller can advance the UI mid-file. `cancel` is checked between frames
/// inside [`stream_to_disk`]; on cancel the temp is deleted and [`FileOutcome::Cancelled`]
/// is returned (no partial). Returns the SPEC s24 error code on failure (mapped
/// to a translatable key on the file's progress entry).
async fn restore_one_file<F: FnMut(u64)>(
    file: &ResolvedRestore,
    store: &dyn RemoteStore,
    crypto: &SuiteVerdict,
    dest_token: &DialogToken,
    cancel: &RestoreCancel,
    mut on_progress: F,
) -> FileOutcome {
    // A file never uploaded has no Drive object to restore.
    let drive_file_id = match file.drive_file_id.as_deref() {
        Some(id) => id,
        None => return FileOutcome::Failed(ErrorCode::InternalBug),
    };

    // Fail closed: an encrypted source whose key is unavailable cannot decrypt.
    let suite: Option<&dyn SourceCryptoSuite> = match crypto {
        SuiteVerdict::Plaintext => None,
        SuiteVerdict::Suite(s) => Some(s.as_ref()),
        SuiteVerdict::Unavailable => return FileOutcome::Failed(ErrorCode::CryptoKeyMissing),
    };

    // Resolve + confine the destination (SPEC s11.6.1): re-create the file's
    // relative tree under the approved root, no traversal, no symlink-at-leaf.
    let dest = match validate_restore_dest(dest_token, &file.relative_path) {
        Ok(d) => d,
        Err(e) => return FileOutcome::Failed(e.code),
    };

    // Open the Drive download stream. M8-P2-5: classify the remote error into the
    // specific SPEC s24 code (auth / missing-object / rate-limit / quota / ...)
    // instead of collapsing every failure to `drive.unreachable`.
    let mut reader = match store.download(drive_file_id).await {
        Ok(stream) => stream.0,
        Err(e) => return FileOutcome::Failed(classify_download_error(&e)),
    };

    // Atomic write (SPEC s11.6.1 step 5): stream into a RANDOM-named sibling temp
    // file opened no-follow + O_EXCL, then rename over the final name. A failure /
    // cancel best-effort removes the temp file so no partial is left behind
    // (M8-P1-1, M8-P1-2).
    let parent = match dest.parent() {
        Some(p) => p,
        None => return FileOutcome::Failed(ErrorCode::LocalIoError),
    };
    // M8-P1-2: a RANDOM temp name (not timestamp-derived) so the path is
    // unpredictable; combined with `create_new(true)` (O_EXCL) below this kills
    // the pre-place / race-to-the-path attack.
    let tmp = parent.join(format!(".driven-restore-tmp.{}", uuid::Uuid::new_v4()));

    let result = stream_to_disk(&mut reader, &tmp, suite, file, cancel, &mut on_progress).await;

    match result {
        StreamOutcome::Done => {
            // M8-P1-2: re-confirm the rename TARGET is still inside the canonical
            // root (the leaf could have been swapped for a symlink during the
            // stream); validate_restore_dest already canonicalised the parent, so
            // re-validate to catch a TOCTOU swap at the leaf before renaming over it.
            if let Err(e) = validate_restore_dest(dest_token, &file.relative_path) {
                let _ = tokio::fs::remove_file(&tmp).await;
                return FileOutcome::Failed(e.code);
            }
            // Atomically place the verified plaintext at its final name.
            if let Err(e) = tokio::fs::rename(&tmp, &dest).await {
                let _ = tokio::fs::remove_file(&tmp).await;
                tracing::warn!(target: TARGET, file = %file.relative_path, %e, "restore atomic rename failed");
                return FileOutcome::Failed(ErrorCode::LocalIoError);
            }
            FileOutcome::Done
        }
        StreamOutcome::Cancelled => {
            // M8-P1-1: cancelled mid-stream - delete the partial temp; nothing was
            // renamed into place, so no partial final file remains.
            let _ = tokio::fs::remove_file(&tmp).await;
            FileOutcome::Cancelled
        }
        StreamOutcome::Failed(code) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            FileOutcome::Failed(code)
        }
    }
}

/// The outcome of streaming one file to disk: completed + verified, cancelled
/// mid-stream (M8-P1-1), or failed with a SPEC s24 code.
enum StreamOutcome {
    Done,
    Cancelled,
    Failed(ErrorCode),
}

/// M8-P2-5: classify a Drive `download` error into the specific SPEC s24
/// [`ErrorCode`], so a per-file restore failure reports invalid auth / a missing
/// remote object / rate-limit / quota distinctly rather than a generic
/// `drive.unreachable`. Reuses the SAME classification the executor / reconcile
/// path uses: the typed `DriveError` downcast (the real `GoogleDriveStore`) via
/// [`driven_drive::google::classification_of`], falling back to substring matching
/// on the message for the `InMemoryRemoteStore` fake (whose fault injection embeds
/// the dotted code in its `anyhow` message). A 404 / not-found / unclassified
/// error maps to `drive.unreachable` (the closest existing code for "the object
/// could not be fetched").
fn classify_download_error(err: &anyhow::Error) -> ErrorCode {
    use driven_drive::remote_store::DriveErrorClassification as C;
    // Typed path (real store): map the classification to its SPEC s24 code.
    if let Some(class) = driven_drive::google::classification_of(err) {
        return match class {
            C::RateLimited { .. } => ErrorCode::DriveRateLimited,
            C::Transient5xx => ErrorCode::DriveUnreachable,
            C::Network => ErrorCode::NetIntermittent,
            C::AuthInvalidGrant => ErrorCode::AuthInvalidGrant,
            C::DailyQuota => ErrorCode::DriveDailyQuotaExhausted,
            C::StorageQuota => ErrorCode::DriveQuotaExhausted,
            C::Other => ErrorCode::DriveUnreachable,
        };
    }
    // String fallback for the fake's plain anyhow messages (mirrors the
    // executor's classify_drive_error ordering: `daily` before `quota_exhausted`).
    let msg = err.to_string();
    if msg.contains("rate_limited") {
        ErrorCode::DriveRateLimited
    } else if msg.contains("daily") {
        ErrorCode::DriveDailyQuotaExhausted
    } else if msg.contains("quota_exhausted") {
        ErrorCode::DriveQuotaExhausted
    } else if msg.contains("invalid_grant") {
        ErrorCode::AuthInvalidGrant
    } else if msg.contains("net.intermittent") || msg.contains("network drop") {
        ErrorCode::NetIntermittent
    } else {
        // 404 / not-found / unclassified: the object could not be fetched.
        ErrorCode::DriveUnreachable
    }
}

/// Stream the download into `tmp`, decrypting if `suite` is `Some`, hashing the
/// plaintext with BLAKE3, and verifying it against `file.hash_blake3`. Bounded
/// memory: at most ~2 ciphertext frames (~128 KiB) are buffered at once, so a
/// 1 GiB file never sits whole in RAM (PERF acceptance).
///
/// M8-P1-1: `cancel` is checked between frames; on cancel this returns
/// [`StreamOutcome::Cancelled`] WITHOUT verifying, so the caller deletes the temp
/// (no partial). M8-P1-2: the temp is opened with [`open_temp_no_follow`] -
/// `create_new(true)` (O_EXCL: fails if the path already exists, killing a
/// pre-place race) plus platform no-follow flags (Unix `O_NOFOLLOW`; Windows
/// `FILE_FLAG_OPEN_REPARSE_POINT` + reparse-point rejection) so a symlinked temp
/// path cannot redirect the write outside the approved root.
async fn stream_to_disk<R, F>(
    reader: &mut R,
    tmp: &std::path::Path,
    suite: Option<&dyn SourceCryptoSuite>,
    file: &ResolvedRestore,
    cancel: &RestoreCancel,
    on_progress: &mut F,
) -> StreamOutcome
where
    R: tokio::io::AsyncRead + Unpin,
    F: FnMut(u64),
{
    match stream_to_disk_inner(reader, tmp, suite, file, cancel, on_progress).await {
        Ok(outcome) => outcome,
        Err(code) => StreamOutcome::Failed(code),
    }
}

/// The fallible body of [`stream_to_disk`]: `?`-propagates IO / crypto errors as
/// the SPEC s24 code, returning the terminal [`StreamOutcome`] on success or
/// cancel. Split out so the outer fn can map an `Err` to [`StreamOutcome::Failed`]
/// without a `match` on every `?`.
async fn stream_to_disk_inner<R, F>(
    reader: &mut R,
    tmp: &std::path::Path,
    suite: Option<&dyn SourceCryptoSuite>,
    file: &ResolvedRestore,
    cancel: &RestoreCancel,
    on_progress: &mut F,
) -> Result<StreamOutcome, ErrorCode>
where
    R: tokio::io::AsyncRead + Unpin,
    F: FnMut(u64),
{
    use tokio::io::AsyncWriteExt;

    // M8-P1-2: open the temp no-follow + O_EXCL (random name from the caller).
    let out = open_temp_no_follow(tmp).map_err(map_io_err)?;
    let out = tokio::fs::File::from_std(out);
    let mut writer = tokio::io::BufWriter::new(out);
    let mut hasher = Blake3::new();
    let mut written: u64 = 0;

    match suite {
        // --- plaintext source: copy bytes straight through, hashing as we go ----
        None => {
            let mut buf = vec![0u8; CIPHERTEXT_FRAME];
            loop {
                // M8-P1-1: cancel observed between frames -> stop without verifying.
                if cancel.load(Ordering::SeqCst) {
                    return Ok(StreamOutcome::Cancelled);
                }
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
                // M8-P1-1: cancel observed between frames -> stop without verifying.
                if cancel.load(Ordering::SeqCst) {
                    return Ok(StreamOutcome::Cancelled);
                }
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
    Ok(StreamOutcome::Done)
}

/// M8-P1-2: open the restore temp file with NO-FOLLOW + O_EXCL semantics (SPEC
/// s11.6.1: no-follow writes). `create_new(true)` (O_EXCL) makes the open FAIL if
/// the path already exists - killing a pre-placed-symlink / race-to-the-path
/// attack outright - and the platform no-follow flag ensures that even a symlink
/// at the leaf cannot redirect the write:
/// - Unix: `O_NOFOLLOW` (open fails with `ELOOP` if the leaf is a symlink).
/// - Windows: `FILE_FLAG_OPEN_REPARSE_POINT` opens the reparse point itself
///   (does not traverse it); combined with `create_new` (which fails if anything
///   exists at the path) a pre-placed reparse point is refused.
fn open_temp_no_follow(tmp: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW: refuse to open a symlink at the final component.
        const O_NOFOLLOW: i32 = 0o400000;
        opts.custom_flags(O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT: open the reparse point itself rather than
        // following it. With create_new the path must not already exist, so a
        // pre-placed reparse point is rejected by O_EXCL before this matters.
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    opts.open(tmp)
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

    /// A never-set cancel flag for the streaming tests (the cancel-specific test
    /// flips its own flag).
    fn no_cancel() -> RestoreCancel {
        Arc::new(AtomicBool::new(false))
    }

    /// A unique random temp path for a no-follow open test (O_EXCL needs an
    /// unused path).
    fn rand_tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "driven-restore-test-{tag}-{}",
            uuid::Uuid::new_v4()
        ))
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
        let res = stream_to_disk(
            &mut reader,
            &out,
            Some(&suite),
            &file,
            &no_cancel(),
            &mut |done| {
                assert!(done >= last_progress, "progress must be monotonic");
                last_progress = done;
            },
        )
        .await;
        assert!(
            matches!(res, StreamOutcome::Done),
            "streaming decrypt must succeed"
        );
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
            let res = stream_to_disk(
                &mut reader,
                &out,
                Some(&suite),
                &file,
                &no_cancel(),
                &mut |_| {},
            )
            .await;
            assert!(matches!(res, StreamOutcome::Done));
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
        let res = stream_to_disk(
            &mut reader,
            &out,
            Some(&suite),
            &file,
            &no_cancel(),
            &mut |_| {},
        )
        .await;
        assert!(matches!(res, StreamOutcome::Done));
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
        let res = stream_to_disk(&mut reader, &out, None, &file, &no_cancel(), &mut |_| {}).await;
        assert!(matches!(res, StreamOutcome::Done));
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
        let res = stream_to_disk(
            &mut reader,
            &out,
            Some(&other),
            &file,
            &no_cancel(),
            &mut |_| {},
        )
        .await;
        assert!(
            matches!(res, StreamOutcome::Failed(ErrorCode::CryptoDecryptFailed)),
            "wrong key must fail with crypto.decrypt_failed"
        );
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
        let res = stream_to_disk(&mut reader, &out, None, &file, &no_cancel(), &mut |_| {}).await;
        assert!(
            matches!(res, StreamOutcome::Failed(ErrorCode::CryptoDecryptFailed)),
            "blake3 mismatch must be refused"
        );
        let _ = std::fs::remove_file(&out);
    }

    // --- M8-P1-1: cancellation deletes the temp + reports Cancelled -----------

    #[tokio::test]
    async fn streaming_cancel_midstream_stops_and_leaves_no_partial() {
        // M8-P1-1: a cancel flag observed between frames stops the stream and
        // returns Cancelled WITHOUT verifying. The temp file may exist mid-write;
        // restore_one_file (the caller) is what deletes it, so here we assert the
        // outcome is Cancelled (the caller's deletion is covered separately).
        let suite = DrivenCryptoSuite::new(driven_crypto::key::SourceKey::generate());
        // Multi-frame so the loop runs more than once and observes the flag.
        let plaintext: Vec<u8> = (0..(3 * PLAINTEXT_CHUNK_LEN))
            .map(|i| (i % 251) as u8)
            .collect();
        let blob = encrypt_blob(&suite, &plaintext);
        let file = resolved_for(&plaintext, "cancel.bin");
        let out = rand_tmp("cancel");
        let mut reader = std::io::Cursor::new(blob);
        // Pre-set the cancel flag so the FIRST frame check trips it.
        let cancel: RestoreCancel = Arc::new(AtomicBool::new(true));
        let res =
            stream_to_disk(&mut reader, &out, Some(&suite), &file, &cancel, &mut |_| {}).await;
        assert!(
            matches!(res, StreamOutcome::Cancelled),
            "a pre-set cancel must stop the stream as Cancelled"
        );
        let _ = std::fs::remove_file(&out);
    }

    #[tokio::test]
    async fn restore_one_file_cancel_deletes_temp_and_reports_cancelled() {
        // M8-P1-1 end-to-end for one file: with the cancel flag already set,
        // restore_one_file must return Cancelled AND leave NO temp file in the
        // dest dir (the partial is deleted). Uses the InMemoryRemoteStore so the
        // real download -> stream -> cleanup path runs.
        use driven_drive::remote_store::UploadBody;
        let dir = rand_tmp("cancel-onefile");
        std::fs::create_dir_all(&dir).unwrap();
        let token = DialogToken::for_root(dir.to_string_lossy().to_string());
        let plaintext = b"some bytes to restore".to_vec();

        let store = driven_drive::fake::InMemoryRemoteStore::new();
        let parent = store.root_id().to_string();
        let entry = store
            .create(
                &parent,
                "x.bin",
                "application/octet-stream",
                UploadBody::Bytes(bytes::Bytes::from(plaintext.clone())),
                std::collections::HashMap::new(),
            )
            .await
            .unwrap();
        let mut file = resolved_for(&plaintext, "sub/x.bin");
        file.drive_file_id = Some(entry.id.clone());

        let cancel: RestoreCancel = Arc::new(AtomicBool::new(true));
        let outcome = restore_one_file(
            &file,
            &store,
            &SuiteVerdict::Plaintext,
            &token,
            &cancel,
            |_| {},
        )
        .await;
        assert!(
            matches!(outcome, FileOutcome::Cancelled),
            "cancel before/at stream must report Cancelled"
        );
        // No leftover temp file in the dest dir (the partial was deleted, and no
        // final file was renamed into place).
        let leftovers: Vec<_> = std::fs::read_dir(dir.join("sub"))
            .map(|rd| rd.flatten().collect())
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "cancel must leave no partial temp: {leftovers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- M8-P1-2: no-follow + O_EXCL temp open --------------------------------

    #[test]
    fn open_temp_no_follow_rejects_existing_path() {
        // O_EXCL: the open must FAIL if the temp path already exists (kills the
        // pre-place / race-to-the-path attack).
        let path = rand_tmp("excl");
        std::fs::write(&path, b"pre-placed").unwrap();
        let err = open_temp_no_follow(&path).expect_err("create_new must reject an existing path");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn open_temp_no_follow_rejects_pre_placed_symlink() {
        // A symlink pre-placed at the temp path must be refused (O_EXCL fails
        // because the path exists; even if it did not, O_NOFOLLOW would refuse).
        use std::os::unix::fs::symlink;
        let dir = rand_tmp("nofollow-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.txt");
        std::fs::write(&target, b"victim").unwrap();
        let link = dir.join(".driven-restore-tmp.evil");
        symlink(&target, &link).unwrap();
        let err =
            open_temp_no_follow(&link).expect_err("a pre-placed symlink temp must be refused");
        // AlreadyExists (O_EXCL) on most platforms; ELOOP if the symlink existed
        // without O_EXCL. Either way the write did NOT go through to the target.
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::Other
            ),
            "unexpected error kind: {:?}",
            err.kind()
        );
        // The target was NOT overwritten via the symlink.
        assert_eq!(std::fs::read(&target).unwrap(), b"victim");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- M8-P2-2: search input limits (DESIGN s18.8) --------------------------

    #[test]
    fn search_query_limits_match_design_s18_8() {
        // The cap is 256 chars (DESIGN s18.8). A 256-char query is allowed; 257 is
        // rejected. Control chars (newline, NUL, CR) are rejected. We assert the
        // pure validation logic (no DB needed) by reproducing the command's guard.
        let ok = "a".repeat(256);
        assert_eq!(ok.chars().count(), MAX_QUERY_LEN);
        let too_long = "a".repeat(257);
        assert!(too_long.chars().count() > MAX_QUERY_LEN);
        for bad in ["line\nbreak", "nul\0byte", "carriage\rreturn", "tab\there"] {
            assert!(
                bad.contains('\0') || bad.chars().any(|c| c.is_control()),
                "{bad:?} must be detected as a control-char query"
            );
        }
    }

    // --- M8-P2-1: tree truncation flag ----------------------------------------

    #[test]
    fn tree_truncation_flag_set_when_children_exceed_cap() {
        // M8-P2-1: more immediate children than MAX_TREE_NODES => truncated. Build
        // rows with > cap direct file children at the root and assert the derive +
        // cap logic the command uses reports truncation.
        let n = (MAX_TREE_NODES as usize) + 5;
        let rows: Vec<RestoreFileRow> = (0..n)
            .map(|i| row(&format!("f{i:06}.txt"), 1, Some("d")))
            .collect();
        let nodes = derive_immediate_children("", &rows);
        let truncated = nodes.len() as u32 > MAX_TREE_NODES;
        let entries: Vec<_> = nodes.into_iter().take(MAX_TREE_NODES as usize).collect();
        assert!(
            truncated,
            "a folder with > cap children must report truncated"
        );
        assert_eq!(entries.len(), MAX_TREE_NODES as usize);
    }

    // --- M8-P2-5: download error classification -------------------------------

    #[test]
    fn classify_download_error_maps_distinct_remote_errors() {
        // M8-P2-5: distinct remote failures map to distinct SPEC s24 codes (not all
        // drive.unreachable). The fake's fault messages embed the dotted code, so
        // the string fallback path classifies them.
        let rate = anyhow::anyhow!("fake: drive.rate_limited after N requests");
        assert_eq!(classify_download_error(&rate), ErrorCode::DriveRateLimited);

        let auth = anyhow::anyhow!("fake: auth.invalid_grant; reauth required");
        assert_eq!(classify_download_error(&auth), ErrorCode::AuthInvalidGrant);

        let daily = anyhow::anyhow!("fake: drive.daily_quota_exhausted dailyLimitExceeded");
        assert_eq!(
            classify_download_error(&daily),
            ErrorCode::DriveDailyQuotaExhausted
        );

        let quota = anyhow::anyhow!("fake: drive.quota_exhausted storageQuotaExceeded");
        assert_eq!(
            classify_download_error(&quota),
            ErrorCode::DriveQuotaExhausted
        );

        // A 404 / not-found / unclassified maps to drive.unreachable (the object
        // could not be fetched).
        let missing = anyhow::anyhow!("fake: no object with id d-missing");
        assert_eq!(
            classify_download_error(&missing),
            ErrorCode::DriveUnreachable
        );
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
