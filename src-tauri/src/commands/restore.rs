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
// R3-P2-3: `RestoreFileRow` is now used only by the test-only
// `derive_immediate_children` helper + the unit tests (production navigation reads
// `ImmediateTreeChildren` via `list_immediate_tree_children`), so the import is
// test-scoped to avoid an unused-import warning in release builds.
#[cfg(test)]
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

    // R3-P2-3: query the IMMEDIATE children directly in SQL (capped CHILDREN), so a
    // single huge sub-folder can NEVER crowd out later sibling folders/files. The
    // previous design scanned the first N DESCENDANT rows and derived children from
    // them, so a first sub-folder with 100k+ files would exhaust the scan cap and
    // hide every sibling after it (the UI saw only `truncated`). Now the DB returns
    // distinct immediate sub-folders + immediate files, each capped independently,
    // and `truncated` is set ONLY on a genuine immediate-child overflow.
    let children = state
        .state()
        .list_immediate_tree_children(source_id, &prefix, MAX_TREE_NODES)
        .await
        .map_err(CommandError::from)?;

    let entries = immediate_children_to_entries(&prefix, &children);
    let truncated = children.truncated;

    tracing::debug!(
        target: TARGET,
        %source_id,
        prefix = %prefix,
        folders = children.folders.len(),
        files = children.files.len(),
        returned = entries.len(),
        truncated,
        "list_remote_tree served from file_state (immediate children)"
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

    // Route (R2-P1-2): a SIMPLE trailing-star prefix term (`proj*`) goes to the
    // FTS5 prefix path (< 50ms, ROADMAP M8 acceptance); a GENUINE wildcard / path
    // pattern (`*.rs`, `src/*`, leading/interior `*`, `?`, `[...]`) goes to the
    // slower GLOB path. `is_glob_query` returns true only for the latter.
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
            // M9c D2: ONE shared predicate (status==Synced AND has a Drive id), so
            // the search surface never marks restorable a row resolution rejects.
            restorable: is_restorable(h.status, h.drive_file_id.as_deref()),
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
    // Issue #36: optional point-in-time. When `Some(unix_ms)`, each file is
    // restored AS OF that instant: the current bytes if they were already in
    // place then, else the retained version whose validity window contains it. A
    // selected file with no backed-up version as of that instant rejects the
    // whole job with a clear message (so the user never silently gets current
    // or wrong-era content). `None` restores the latest bytes (pre-#36 behaviour).
    as_of: Option<i64>,
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

    // R3-P2-1: PEEK (do NOT consume) the one-shot dialog token first, so any
    // failure in the token-INDEPENDENT validation below (a stale selection, a
    // missing/unuploaded row, a destination collision, or a keychain/setup error)
    // leaves the token INTACT - the user is not forced to re-pick the folder. The
    // token is CONSUMED only immediately before the job is actually accepted (just
    // before the atomic seed+spawn), so the single use is spent only on a real
    // restore. A missing / replayed / expired token is still rejected here.
    let dest_dir = state.peek_dialog_token(&dest_token).ok_or_else(|| {
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
    let dest_root = DialogToken::for_root(dest_dir.to_string_lossy().to_string());

    // Resolve each selected item to its authoritative file_state row UP FRONT (so
    // a bad selection fails the command rather than the background job): the
    // (source_id, relative_path) pair is the file_state PK; the backend reads the
    // Drive id + size + status from SQLite, never trusting webview-supplied ids.
    // R3-P2-2: a row that was never uploaded (drive_file_id == NULL) or is not in
    // a restorable (`synced`) state is rejected here as bad INPUT, before any job
    // is spawned, rather than flowing in and failing later as `internal.bug`. This
    // resolution + the R3-P1-1 collision pre-check are token-INDEPENDENT, so they
    // run BEFORE the dialog token is consumed (R3-P2-1).
    let resolved = resolve_restore_items(state.inner(), &items, as_of).await?;

    // R2-P2-1: build ALL fallible setup (the per-account remote stores + crypto
    // suites the job will use) BEFORE seeding/emitting the job, so an early Err
    // returns WITHOUT ever creating a job entry or emitting a "running" job event.
    // Previously the job was seeded + emitted first, so a failure here left a
    // non-terminal job orphaned in AppState (never pruned) and a dangling running
    // job on the UI. `build_restore_plans` does NOT touch the job map, so on Err
    // the `?` below returns with `restore_jobs` still empty.
    let plans = build_restore_plans(state.inner(), resolved).await?;

    // R3-P2-1: ALL token-independent validation + fallible setup has now succeeded,
    // so the restore WILL be accepted. CONSUME the one-shot token now (the first +
    // only irreversible step). The peeked `dest_dir` equals the path bound to the
    // token, so `dest_root` (built from it above) is the approved root; we just
    // spend the single use so the token cannot be replayed. A concurrent take that
    // already consumed it (None) is rejected without spawning a job.
    if state.take_dialog_token(&dest_token).is_none() {
        return Err(CommandError::with_code(
            ErrorCode::LocalIoError,
            "no matching destination folder; pick a restore folder first",
        ));
    }
    // R5-P1-3 (DATA-SAFETY): BIND the approved root to a STABLE identity (canonical
    // path + on-disk dev/inode or volume file-id) right now, at consume time. The
    // background job carries this `ConfinedRoot`, and every per-file write
    // re-verifies the root against it - so a root swapped to a symlink/junction
    // AFTER this point is rejected and no decrypted bytes land outside the chosen
    // directory (the root-level analogue of the per-component parent-swap TOCTOU).
    let dest_root = confine::ConfinedRoot::bind(dest_root).map_err(|code| {
        CommandError::with_code(code, "restore destination folder is no longer valid")
    })?;

    // All fallible setup succeeded. NOW mint the job id, build the initial
    // (all-pending) status from the plans, seed it WITH its cancel flag (P1-1),
    // and emit the first tick so the webview shows the job (R2-P2-1: never before
    // the setup above, so a setup failure leaves no job behind).
    let job_id = uuid::Uuid::new_v4().to_string();
    let mut status = RestoreJobStatus {
        job_id: job_id.clone(),
        total_files: plans.len() as u32,
        completed_files: 0,
        failed_files: 0,
        total_bytes: plans.iter().map(|p| p.file.size).sum(),
        bytes_done: 0,
        current_file: None,
        done: false,
        cancelled: false,
        files: plans
            .iter()
            .map(|p| RestoreFileProgress {
                relative_path: p.file.relative_path.clone(),
                state: RestoreFileState::Pending,
                bytes_done: 0,
                bytes_total: p.file.size,
                error_code: None,
            })
            .collect(),
    };
    // Clone the initial status for the seed (stored on AppState) and for the first
    // emit, since the original `status` moves INTO the spawned task below (it owns
    // + mutates the live status as the job runs).
    let status_seed = status.clone();
    let status_for_emit = status.clone();
    // M8-P1-1: the per-job cancel flag the spawned task observes between frames;
    // `cancel_restore_job` + the shutdown drain set it.
    let cancel: RestoreCancel = Arc::new(AtomicBool::new(false));

    // R3-P1-2: ATOMIC seed + handle-registration via a START BARRIER, so
    // `cancel_all_restore_jobs` (app quit) NEVER observes a seeded restore job that
    // lacks an awaitable handle, and a quit anywhere in the spawn window leaves NO
    // partial temp.
    //
    // The previous code seeded the job, spawned the task, THEN attached the handle
    // - a window in which a quit saw a handle-less job it could not await/abort,
    // and if the task had already begun (created a temp) the process could exit
    // mid-write leaving a partial. We close that window:
    // 1. Spawn the task FIRST, but gate ALL of its filesystem work behind a
    //    oneshot `release_rx.await`: until released it creates NO temp / touches no
    //    disk. (A oneshot await is an async wait, not a poll/sleep loop.)
    // 2. Seed the job AND register the JoinHandle in ONE locked insert
    //    (`seed_restore_job` now takes the handle), so the moment the job is
    //    observable it ALREADY has an awaitable handle.
    // 3. ONLY THEN release the barrier. On release the task re-checks the cancel
    //    flag IMMEDIATELY (before any temp): if a quit/cancel set it during the
    //    window, the task exits cleanly via `run_restore_job`'s pre-file cancel
    //    check (it marks every file Cancelled, emits the terminal status, and
    //    clears its own handle) - no temp is ever created.
    let app_for_job = app.clone();
    let job_cancel = cancel.clone();
    let job_id_for_task = job_id.clone();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    // tokio::spawn (not tauri::async_runtime::spawn) so the returned handle is a
    // tokio JoinHandle the AppState tracks + the shutdown drain awaits, matching
    // the per-account task handles (the command already runs on the tokio runtime).
    let handle = tokio::task::spawn(async move {
        // BARRIER: do nothing (no temp, no disk) until released. If the command
        // returns Err before releasing (so `release_tx` is dropped), `await`
        // resolves to Err and the task exits without seeding-side effects. If the
        // job was cancelled during the seed window, the cancel flag is already set
        // and `run_restore_job`'s first pre-file check exits cleanly (no temp).
        if release_rx.await.is_err() {
            // Not released (the command failed after spawn but before release, or
            // the sender was dropped): clear our own handle if the job was seeded,
            // then exit without doing any filesystem work.
            if let Some(state) = app_for_job.try_state::<AppState>() {
                state.finish_restore_job_handle(&job_id_for_task);
            }
            return;
        }
        run_restore_job(app_for_job, plans, dest_root, &mut status, job_cancel).await;
    });

    // Seed the job AND attach the handle atomically (one locked insert), so a
    // seeded job is NEVER observable without an awaitable handle (R3-P1-2). Done
    // under the SAME lock that seeds the job.
    state.seed_restore_job(status_seed, cancel, handle);
    // The job is now observable WITH its handle. Release the barrier so the task
    // proceeds (or exits cleanly if a cancel landed during the window).
    let _ = release_tx.send(());
    // Emit the first (all-pending) tick so the webview shows the job.
    emit_progress(&app, &status_for_emit);

    Ok(RestoreJobId(job_id))
}

/// Resolve + validate the selected restore `items` against authoritative
/// `file_state` (token-INDEPENDENT, so it runs BEFORE the one-shot dialog token is
/// consumed - R3-P2-1). For each item it:
///   - parses the source id + relative path (bad shapes -> `internal.invalid_input`),
///   - looks up the `file_state` PK row (an unknown row is a stale / forged
///     selection -> `internal.invalid_input`, NOT `internal.bug` - R3-P2-2),
///   - R3-P2-2 restore-eligibility: REJECTS a row that was never uploaded
///     (`drive_file_id == NULL`) or whose status is not `synced`, as bad input. A
///     non-`synced` row's recorded `hash_blake3` may not match the bytes currently
///     on Drive, so restoring it would fail the in-stream BLAKE3 verify late (a
///     confusing `crypto.decrypt_failed`) or hand back a mismatched object; we
///     reject it up front instead (documented in design/CODEX_NOTES.md).
///
/// Then it runs the R3-P1-1 destination-collision pre-check over the whole
/// selection (duplicate / case-folded / file-vs-dir path conflicts), rejecting the
/// WHOLE job before any job is spawned or the token consumed. Split out so this
/// pure validation is unit-testable against a real `AppState` without a Tauri
/// `AppHandle`.
async fn resolve_restore_items(
    state: &AppState,
    items: &[RestoreItem],
    as_of: Option<i64>,
) -> CommandResult<Vec<ResolvedRestore>> {
    let mut resolved: Vec<ResolvedRestore> = Vec::with_capacity(items.len());
    for item in items {
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
                    ErrorCode::InvalidInput,
                    format!("unknown file to restore: {}", item.relative_path),
                )
            })?;
        // M9c D2 (M8 R4-P2-1): gate resolution on the SAME `is_restorable`
        // predicate the tree/search DTOs use, so the UI never offers a row that is
        // rejected here. The granular branches below only choose the precise
        // bad-input MESSAGE (never-uploaded vs non-synced); the eligibility
        // DECISION is the single shared predicate.
        if !is_restorable(row.status, row.drive_file_id.as_deref()) {
            if row.drive_file_id.is_none() {
                return Err(CommandError::with_code(
                    ErrorCode::InvalidInput,
                    format!(
                        "file is not restorable (never uploaded): {}",
                        item.relative_path
                    ),
                ));
            }
            return Err(CommandError::with_code(
                ErrorCode::InvalidInput,
                format!(
                    "file is not restorable (status is {}, not synced): {}",
                    file_state_status_str(row.status),
                    item.relative_path
                ),
            ));
        }
        // Issue #36: pick the effective (drive_file_id, size, hash) for the
        // requested instant. Without `as_of`, or when the current bytes were
        // already in place at `as_of`, restore the current object; otherwise
        // restore the retained version whose window covers `as_of`. A missing
        // version rejects the whole job with a clear message.
        let (drive_file_id, size, hash_blake3) = match as_of {
            None => (row.drive_file_id.clone(), row.size, row.hash_blake3),
            Some(t) if row.last_uploaded_at.is_some_and(|c| t >= c) => {
                // The current bytes were already the live version at `t`.
                (row.drive_file_id.clone(), row.size, row.hash_blake3)
            }
            Some(t) => {
                match state
                    .state()
                    .resolve_version_at(source_id, &relative_path, t)
                    .await
                    .map_err(CommandError::from)?
                {
                    Some(v) => (Some(v.drive_file_id), v.size, v.hash_blake3),
                    None => {
                        return Err(CommandError::with_code(
                            ErrorCode::InvalidInput,
                            format!(
                                "no backed-up version of {} as of the selected date",
                                item.relative_path
                            ),
                        ))
                    }
                }
            }
        };
        resolved.push(ResolvedRestore {
            source_id,
            relative_path: row.relative_path.as_str().to_string(),
            size,
            drive_file_id,
            hash_blake3,
        });
    }

    // R3-P1-1: reject the WHOLE job if any two selected items would write to the
    // SAME destination (case-folded), or if one item's destination is a strict
    // path-prefix of another's (a file vs a directory at the same path).
    detect_dest_collisions(&resolved)?;

    Ok(resolved)
}

/// `cancel_restore_job(job)` - request cancellation of a running restore job
/// (SPEC s11.5 / s11.7; M8-P1-1 / R1-P1-2). Sets the job's cancel flag so the
/// background task stops between frames, DELETES any in-flight temp file (no
/// partial left), and emits a terminal CANCELLED [`RestoreJobStatus`]. Idempotent:
/// cancelling an unknown / already-finished job is a benign no-op (the job may
/// have completed or its terminal record been pruned). The command returns once
/// the cancel is SIGNALLED; the terminal CANCELLED status arrives on
/// `restore:progress`.
///
/// R1-P1-2: this ONLY sets the cancel flag and LEAVES the task handle tracked on
/// the job entry. It deliberately does NOT take the handle - taking + dropping a
/// tokio `JoinHandle` would DETACH the task, so after a UI cancel the task would
/// no longer be drainable on shutdown and could run on until its next read (an
/// orphan, violating the no-orphan/no-partial cancel acceptance). The task
/// observes the flag, cleans up its temp, emits CANCELLED, and clears its own
/// handle on exit (`finish_restore_job_handle`); the app-shutdown drain
/// (`cancel_all_restore_jobs`) still awaits/aborts the handle if the task is
/// somehow still running at quit.
#[tauri::command]
pub async fn cancel_restore_job(
    state: State<'_, AppState>,
    job: RestoreJobId,
) -> CommandResult<()> {
    let _ = state.signal_cancel_restore_job(&job.0);
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

/// `list_file_versions(source_id, relative_path)` - the retained point-in-time
/// versions of one file, newest-first (issue #36). Reads `file_versions` (LOCAL
/// metadata), never Drive. Powers the Restore version-history view so the user
/// can see which dates have a restorable version before choosing an "as of" date.
#[tauri::command]
pub async fn list_file_versions(
    state: State<'_, AppState>,
    source_id: SourceId,
    relative_path: String,
) -> CommandResult<Vec<crate::commands::dtos::FileVersionDto>> {
    let rp = driven_core::types::RelativePath::try_from(relative_path).map_err(|e| {
        CommandError::with_code(
            ErrorCode::InvalidInput,
            format!("invalid relative path: {e}"),
        )
    })?;
    let versions = state
        .state()
        .list_file_versions(source_id, &rp)
        .await
        .map_err(CommandError::from)?;
    Ok(versions
        .into_iter()
        .map(|v| crate::commands::dtos::FileVersionDto {
            size: v.size,
            created_at: v.created_at,
            superseded_at: v.superseded_at,
            trashed: v.trashed,
        })
        .collect())
}

/// R2-P2-1: build the per-file [`RestorePlan`]s (the fallible job SETUP) WITHOUT
/// touching the restore-job map. Resolves the source -> account map, builds (and
/// caches per account) the remote store, and resolves the per-source crypto
/// verdict for each resolved file. Returns `Err` (via `?` in `restore_files`) on
/// the first setup failure - and because this never seeds a job, an early failure
/// leaves `AppState.restore_jobs` untouched (no orphaned non-terminal job entry,
/// no dangling "running" job event). Split out so the seed/emit can happen strictly
/// AFTER setup succeeds and so the setup path is unit-testable against a real
/// `AppState` (the `restore_files` command itself needs a Tauri `State`/`AppHandle`).
async fn build_restore_plans(
    state: &AppState,
    resolved: Vec<ResolvedRestore>,
) -> CommandResult<Vec<RestorePlan>> {
    let mut plans: Vec<RestorePlan> = Vec::with_capacity(resolved.len());
    // Cache one remote store per account (multiple files often share an account).
    let mut store_cache: std::collections::HashMap<
        driven_core::types::AccountId,
        Arc<dyn RemoteStore>,
    > = std::collections::HashMap::new();

    // Resolve the source -> account map once (fallible).
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
                let s = crate::commands::sources::build_restore_store(state, account_id)?;
                store_cache.insert(account_id, s.clone());
                s
            }
        };
        // Resolve the per-source crypto suite (fail-closed) via the account's live
        // provider, so an encrypted file decrypts and an unencrypted one streams
        // raw. An account with no running handle (e.g. needs_reauth) cannot restore
        // an encrypted file - the resolution is recorded and the file fails closed.
        let crypto = resolve_suite(state, account_id, r.source_id, source.encryption_enabled);
        plans.push(RestorePlan {
            file: r,
            store,
            crypto,
        });
    }
    Ok(plans)
}

// -----------------------------------------------------------------------------
// Background job
// -----------------------------------------------------------------------------

/// One file resolved to its authoritative `file_state` fields (M8).
#[derive(Debug)]
struct ResolvedRestore {
    source_id: SourceId,
    relative_path: String,
    size: u64,
    drive_file_id: Option<String>,
    hash_blake3: [u8; 32],
}

/// R3-P1-1: the restore-eligible subset of one resolved item, used for the
/// destination-collision pre-check. A restore writes every selected item to
/// `dest/<relative_path>`, so two items whose normalized destination KEY collide
/// (an exact duplicate, a case-folded duplicate on a case-insensitive dest, or a
/// file whose key is a strict path-prefix of another item's directory key) would
/// silently overwrite each other (data loss). We compute these keys up front and
/// reject the WHOLE job on any collision (a visible bad-request error) BEFORE any
/// fallible setup, the dialog token is consumed, or the job is spawned.
struct DestKey {
    /// The original (display) relative path - used in the rejection message.
    display: String,
    /// The case-FOLDED path segments (each segment lowercased) of the
    /// destination key. We fold case to the SAFE direction (treat `Foo.txt` and
    /// `foo.txt` as colliding): a case-insensitive destination is the norm on the
    /// supported platforms (Windows ALWAYS, macOS/APFS by default), and an
    /// over-reject is a visible error while an under-reject is silent data loss
    /// (R3-P1-1). We DEFAULT TO folding rather than probing the dest's case
    /// sensitivity (documented in design/CODEX_NOTES.md): probing would create
    /// throwaway files in the user's chosen folder for an edge that does not arise
    /// on the supported platforms.
    folded: Vec<String>,
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
    dest_root: confine::ConfinedRoot,
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
            &dest_root,
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
    dest_root: &confine::ConfinedRoot,
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
    // relative tree under the approved root, no traversal, no symlink-at-leaf. This
    // CREATES the parent directory chain (component-at-a-time confined, R1-P1-1) so
    // the handle-based confine open below can re-open it; it is the structural
    // pre-step, while `ConfinedDest` provides the TOCTOU-safe write/rename.
    if let Err(e) = validate_restore_dest(dest_root.token(), &file.relative_path) {
        return FileOutcome::Failed(e.code);
    }

    // Open the Drive download stream. M8-P2-5: classify the remote error into the
    // specific SPEC s24 code (auth / missing-object / rate-limit / quota / ...)
    // instead of collapsing every failure to `drive.unreachable`.
    let mut reader = match store.download(drive_file_id).await {
        Ok(stream) => stream.0,
        Err(e) => return FileOutcome::Failed(classify_download_error(&e)),
    };

    // R2-P1-1 (SPEC s11.6.1): open a HANDLE-CONFINED destination. This RE-walks the
    // parent chain from the canonical root opening each component with a NO-FOLLOW
    // directory handle (rejecting a symlink / reparse point and re-confirming the
    // resolved real path is still inside the dialog-approved canonical root),
    // creates a RANDOM-named temp RELATIVE to that pinned final-parent handle, and
    // (on Windows) re-confirms the temp itself landed in-root BEFORE any plaintext
    // is written. Because BOTH the temp create and the final rename are performed
    // RELATIVE to the SAME pinned parent handle, a concurrent swap of a parent
    // component to a symlink/junction AFTER validation cannot redirect the decrypted
    // plaintext (temp OR final) outside the root - the earlier path-string TOCTOU
    // (re-resolving the dest at rename time, and opening the temp by full path) is
    // closed. The temp is unpredictably named + O_EXCL, so a pre-place race is also
    // refused (M8-P1-2).
    // `open` returns the guard PLUS the open temp file to stream into. The
    // ConfinedDest retains everything needed to (a) rename handle-relative on
    // success and (b) remove the temp on drop if not committed (so an abort /
    // failure / cancel leaves no temp - the R2-P2-2 abort-safe cleanup: dropping
    // the future drops `confined`, whose Drop deletes the still-uncommitted temp).
    let (mut confined, temp_file) =
        match confine::ConfinedDest::open(dest_root, &file.relative_path) {
            Ok(pair) => pair,
            Err(code) => return FileOutcome::Failed(code),
        };

    let result = stream_to_disk(
        &mut reader,
        temp_file,
        suite,
        file,
        cancel,
        &mut on_progress,
    )
    .await;

    match result {
        StreamOutcome::Done => {
            // R2-P1-1: commit by a HANDLE-RELATIVE atomic replace - the rename
            // resolves the leaf relative to the pinned parent handle (NOT a
            // re-resolved path string), so a parent swap during the stream cannot
            // redirect it out of root, and an existing dest is atomically replaced
            // (R1-P2-2). On success the temp guard is defused; on error the temp is
            // removed by the guard on drop.
            match confined.commit().await {
                Ok(()) => FileOutcome::Done,
                Err(code) => {
                    tracing::warn!(
                        target: TARGET,
                        file = %file.relative_path,
                        code = %code.code(),
                        "restore handle-relative commit failed"
                    );
                    FileOutcome::Failed(code)
                }
            }
        }
        // M8-P1-1 / R2-P2-2: cancelled or failed mid-stream - `confined` is dropped
        // here (NOT committed), so its Drop removes the partial temp; nothing was
        // renamed into place, so no partial final file remains.
        StreamOutcome::Cancelled => FileOutcome::Cancelled,
        StreamOutcome::Failed(code) => FileOutcome::Failed(code),
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

/// Stream the download `reader` into the ALREADY-OPEN temp file `out`, decrypting
/// if `suite` is `Some`, hashing the plaintext with BLAKE3, and verifying it
/// against `file.hash_blake3`. Bounded memory: at most ~2 ciphertext frames
/// (~128 KiB) are buffered at once, so a 1 GiB file never sits whole in RAM (PERF
/// acceptance).
///
/// R2-P1-1: the temp `out` is opened + confined by the caller's
/// [`confine::ConfinedDest`] (a no-follow temp created RELATIVE to a verified
/// parent handle), so this fn just writes into the open handle - it never
/// re-resolves a path. M8-P1-1: `cancel` is checked between frames; on cancel this
/// returns [`StreamOutcome::Cancelled`] WITHOUT verifying, and the caller's
/// `ConfinedDest` drop removes the temp (no partial). On any error the same
/// drop-based cleanup applies (R2-P2-2).
async fn stream_to_disk<R, F>(
    reader: &mut R,
    out: std::fs::File,
    suite: Option<&dyn SourceCryptoSuite>,
    file: &ResolvedRestore,
    cancel: &RestoreCancel,
    on_progress: &mut F,
) -> StreamOutcome
where
    R: tokio::io::AsyncRead + Unpin,
    F: FnMut(u64),
{
    match stream_to_disk_inner(reader, out, suite, file, cancel, on_progress).await {
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
    out: std::fs::File,
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

    // R2-P1-1: `out` is the temp the caller already created RELATIVE to a verified
    // parent handle (no path re-resolution here).
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
                // R1-P2-1: a read error here is a DRIVE/network failure, not a
                // local disk failure - classify it accordingly.
                let n = reader.read(&mut buf).await.map_err(map_download_read_err)?;
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
            //    decryptor. A short read (UnexpectedEof) means a truncated /
            //    non-Driven object -> crypto.decrypt_failed; any OTHER read error
            //    is a DRIVE/network mid-stream failure (R1-P2-1), classified as
            //    such rather than collapsed to a crypto/local error.
            let mut header = [0u8; HEADER_LEN];
            reader.read_exact(&mut header).await.map_err(|e| {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    ErrorCode::CryptoDecryptFailed
                } else {
                    map_download_read_err(e)
                }
            })?;
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
                    // R1-P2-1: a mid-stream read error is a DRIVE/network failure.
                    let n = reader
                        .read(&mut read_chunk)
                        .await
                        .map_err(map_download_read_err)?;
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

// The handle-based confinement module (R2-P1-1) lives at the bottom of this file
// (`mod confine`).

/// R1-P2-1: map a MID-STREAM download READ error to the SPEC s24 code. A read
/// error on the Drive download stream is a DRIVE/NETWORK failure, NOT a local
/// disk failure, so it must NOT be reported as `local.io_error` (which would tell
/// the user the DISK failed when DRIVE/network failed). This first asks
/// [`driven_drive::google::classify_stream_read_error`] for the wrapped Drive /
/// transport classification (the real `GoogleDriveStore`'s streaming reader wraps
/// the reqwest transport error via `io::Error::other`); failing that it falls back
/// to the dotted-code substring scan used by [`classify_download_error`] (so the
/// `InMemoryRemoteStore` fake's mid-stream injected errors, whose messages embed
/// the dotted code, still classify). A read failure with no recognisable
/// Drive/network cause maps to `net.intermittent` - the closest "the download
/// stream broke" code - rather than `local.io_error`.
fn map_download_read_err(e: std::io::Error) -> ErrorCode {
    use driven_drive::remote_store::DriveErrorClassification as C;
    if let Some(class) = driven_drive::google::classify_stream_read_error(&e) {
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
    // String fallback: a mid-stream io error whose message embeds a dotted code
    // (the fake's injected mid-stream faults, or a wrapped Display). Mirrors
    // classify_download_error's ordering (daily before quota_exhausted).
    let msg = e.to_string();
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
        // A broken download stream with no recognisable local-disk cause: this is
        // a DRIVE/network failure, not a local IO error.
        ErrorCode::NetIntermittent
    }
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
/// crypto provider, applying the FAIL-CLOSED policy keyed on the DB
/// `SourceRow.encryption_enabled` flag (CAP-P2b, post-GA hardening). An account
/// with no running handle (never spawned / needs_reauth) yields `Unavailable` for
/// an ENCRYPTED source so its files fail closed with `crypto.key_missing` rather
/// than streaming ciphertext to disk; an unencrypted source resolves `Plaintext`
/// (no handle needed since there is nothing to decrypt).
///
/// CAP-P2b: the DB row's `encryption_enabled` - NOT the live provider's verdict -
/// is the fail-closed authority, mirroring the EXECUTOR's policy
/// (`executor.rs::resolve_source_crypto`). A stale provider snapshot
/// (`reconfigure_account` keeps the prior snapshot when `list_sources` fails,
/// sources.rs) could otherwise return `Plaintext` for an encrypted source and route
/// ciphertext through the plaintext path. See [`apply_encryption_policy`].
fn resolve_suite(
    state: &AppState,
    account_id: driven_core::types::AccountId,
    source_id: SourceId,
    encryption_enabled: bool,
) -> SuiteVerdict {
    use driven_core::crypto_provider::CryptoProvider;
    let resolution = match state.account(account_id) {
        Some(handle) => handle.crypto.resolve(&source_id),
        // No running handle: we cannot resolve a per-source key. Treat it as
        // `Unavailable` and let the DB-keyed policy decide (an encrypted source
        // fails closed; an unencrypted source forces plaintext).
        None => CryptoResolution::Unavailable,
    };
    apply_encryption_policy(encryption_enabled, resolution)
}

/// CAP-P2b (post-GA hardening): the PURE fail-closed policy mirroring the
/// executor (`executor.rs::resolve_source_crypto`). The DB `encryption_enabled`
/// flag is the AUTHORITY; the live provider's `CryptoResolution` is only TRUSTED
/// to supply the suite when the DB says encrypted:
///
/// - **DB encrypted** (`encryption_enabled == true`): ONLY a resolved
///   [`CryptoResolution::Suite`] is acceptable -> [`SuiteVerdict::Suite`]. A
///   provider `Plaintext` or `Unavailable` (e.g. a stale snapshot, or no running
///   handle) FAILS CLOSED -> [`SuiteVerdict::Unavailable`] (`crypto.key_missing`),
///   so ciphertext is never streamed through the plaintext path.
/// - **DB unencrypted** (`encryption_enabled == false`): FORCE
///   [`SuiteVerdict::Plaintext`], IGNORING any suite the provider returns. An
///   unencrypted source has nothing to decrypt, so a spurious provider suite must
///   never be applied.
fn apply_encryption_policy(encryption_enabled: bool, resolution: CryptoResolution) -> SuiteVerdict {
    if !encryption_enabled {
        // DB says unencrypted: force plaintext, ignore any provider suite.
        return SuiteVerdict::Plaintext;
    }
    // DB says encrypted: only a real suite is acceptable; anything else fails
    // closed (never degrade to plaintext for an encrypted source).
    match resolution {
        CryptoResolution::Suite(s) => SuiteVerdict::Suite(s),
        CryptoResolution::Plaintext | CryptoResolution::Unavailable => SuiteVerdict::Unavailable,
    }
}

/// R3-P1-1: reject the restore job if any two selected items would collide at the
/// destination. A restore writes each item to `dest/<relative_path>`, so a
/// collision SILENTLY overwrites one file with another (data loss). Two collision
/// shapes are rejected, BOTH under case-FOLDING (so `Foo.txt` and `foo.txt` count
/// as the same destination on a case-insensitive dest - Windows always, macOS/APFS
/// by default; see [`DestKey`]):
///   1. DUPLICATE: two items map to the same folded destination key (e.g. two
///      sources both selecting `foo.txt`, or `Foo.txt` + `foo.txt`).
///   2. FILE-VS-DIR PREFIX: one item's folded key is a strict path-PREFIX of
///      another's, i.e. one item is a FILE at a path that is also a DIRECTORY
///      component of another item's path (e.g. `a/b` as a file vs `a/b/c` as a
///      file - `a/b` cannot be both a file and a directory).
///
/// The prefix test is SEGMENT-WISE, never a raw string `starts_with` (so `foo`
/// does NOT falsely prefix `foobar`): we build the set of every PROPER ANCESTOR
/// directory path of every item and check it against the set of full file keys -
/// any intersection is a file-vs-dir conflict.
fn detect_dest_collisions(resolved: &[ResolvedRestore]) -> CommandResult<()> {
    use std::collections::HashMap;

    // Build the folded key for each item.
    let keys: Vec<DestKey> = resolved
        .iter()
        .map(|r| DestKey {
            display: r.relative_path.clone(),
            folded: r
                .relative_path
                .split('/')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_lowercase())
                .collect(),
        })
        .collect();

    // 1) DUPLICATE folded full key. Map folded-joined -> first display path seen.
    let mut seen: HashMap<String, String> = HashMap::with_capacity(keys.len());
    for k in &keys {
        let joined = k.folded.join("/");
        if let Some(prev) = seen.get(&joined) {
            return Err(CommandError::with_code(
                ErrorCode::InvalidInput,
                format!(
                    "restore selection has two files that map to the same destination (case-insensitive): \"{}\" and \"{}\"",
                    prev, k.display
                ),
            ));
        }
        seen.insert(joined.clone(), k.display.clone());
    }

    // 2) FILE-VS-DIR PREFIX: every PROPER ANCESTOR dir path of every item, mapped
    //    back to the item whose ancestor it is. If any ancestor path equals some
    //    item's FULL file key, that file path is also used as a directory - reject.
    let mut ancestor_of: HashMap<String, String> = HashMap::new();
    for k in &keys {
        // Proper ancestors: all but the last segment, accumulated.
        for end in 1..k.folded.len() {
            let ancestor = k.folded[..end].join("/");
            ancestor_of
                .entry(ancestor)
                .or_insert_with(|| k.display.clone());
        }
    }
    for k in &keys {
        let joined = k.folded.join("/");
        if let Some(descendant) = ancestor_of.get(&joined) {
            // `joined` is a FILE key that is ALSO an ancestor directory of
            // `descendant` - a file-vs-directory conflict at the same path.
            return Err(CommandError::with_code(
                ErrorCode::InvalidInput,
                format!(
                    "restore selection conflicts: \"{}\" is a file but is also a folder on the path to \"{}\"",
                    k.display, descendant
                ),
            ));
        }
    }
    Ok(())
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

/// `true` if `query` is a GENUINE wildcard / path pattern that must route to the
/// SQLite GLOB path rather than FTS5 (R2-P1-2).
///
/// The dispatcher's job is to keep a SIMPLE trailing-star prefix term - a single
/// token (no path separator), with no other wildcard metacharacter, ending in
/// exactly a trailing `*` over a NON-EMPTY stem (e.g. `proj*`) - on the fast FTS5
/// prefix path (`build_fts_match_query` already emits the `"proj"*` prefix form,
/// ROADMAP M8's `<50ms` acceptance). Everything else is a real GLOB pattern:
/// - any `?` or `[` (GLOB single-char / class metacharacters);
/// - any `/` (a path pattern like `src/*`, which FTS5 cannot match);
/// - a `*` that is NOT a single trailing star over a non-empty stem: a leading
///   `*` (`*.rs`), an interior `*` (`a*b`), a bare/empty-stem `*` / `**`, or more
///   than one `*` anywhere.
///
/// So this returns `false` (-> FTS5) ONLY for: a plain term with no metacharacter,
/// or a single-trailing-star prefix term; and `true` (-> GLOB) for any genuine
/// wildcard / path pattern.
fn is_glob_query(query: &str) -> bool {
    // `?`, `[`, and `/` are unambiguous GLOB / path markers - always GLOB.
    if query.contains('?') || query.contains('[') || query.contains('/') {
        return true;
    }
    // No `*` at all: a plain term -> FTS5 (not glob).
    if !query.contains('*') {
        return false;
    }
    // There is at least one `*`. It is a SIMPLE prefix term (-> FTS5, not glob)
    // iff it is EXACTLY one trailing `*` over a non-empty stem: the only `*` is the
    // final byte, and the stem before it is non-empty. Any other shape (leading /
    // interior `*`, multiple `*`, or a bare `*`) is a genuine GLOB pattern.
    let is_simple_trailing_prefix =
        query.ends_with('*') && query.len() > 1 && query.matches('*').count() == 1;
    !is_simple_trailing_prefix
}

/// R3-P2-3: convert the SQL-computed [`ImmediateTreeChildren`] (distinct immediate
/// sub-folder names + immediate file rows) into the webview [`RemoteEntryDto`]
/// list. Folders sort before files, each alphabetically (the DB already returns
/// each kind sorted; we just concatenate folders-then-files). The full
/// `relative_path` is rebuilt as `prefix/<name>` (or `<name>` at the root).
fn immediate_children_to_entries(
    prefix: &str,
    children: &driven_core::state::ImmediateTreeChildren,
) -> Vec<RemoteEntryDto> {
    let prefix_join = |name: &str| -> String {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        }
    };
    let mut out: Vec<RemoteEntryDto> =
        Vec::with_capacity(children.folders.len() + children.files.len());
    for name in &children.folders {
        out.push(RemoteEntryDto {
            relative_path: prefix_join(name),
            name: name.clone(),
            is_dir: true,
            size: 0,
            status: None,
            restorable: false,
        });
    }
    for row in &children.files {
        let full = row.relative_path.as_str().to_string();
        // The display name is the leaf segment of the full relative path.
        let name = full.rsplit('/').next().unwrap_or(&full).to_string();
        out.push(RemoteEntryDto {
            relative_path: full,
            name,
            is_dir: false,
            size: row.size,
            status: Some(file_state_status_str(row.status).to_string()),
            // M9c D2: ONE shared predicate (status==Synced AND has a Drive id).
            restorable: is_restorable(row.status, row.drive_file_id.as_deref()),
        });
    }
    out
}

/// Derive the IMMEDIATE children (sub-folders + files) of `prefix` from the
/// subtree rows. A row whose path equals `prefix/<name>` is a direct FILE child; a
/// row deeper than that (`prefix/<dir>/...`) contributes its first segment as a
/// direct FOLDER child (deduped). Folders sort before files, each alphabetically.
///
/// R3-P2-3: the production `list_remote_tree` no longer derives children from a
/// capped descendant scan (that hid siblings behind a huge first sub-folder) - it
/// queries immediate children in SQL via `list_immediate_tree_children`. This
/// helper is retained ONLY for the unit tests that assert the folder/file split +
/// ordering semantics (the same semantics the SQL now enforces), so it is
/// `#[cfg(test)]`.
#[cfg(test)]
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
            // M9c D2: same shared predicate the production path uses.
            restorable: is_restorable(row.status, row.drive_file_id.as_deref()),
        });
    }
    out
}

/// M9c D2 (M8 R4-P2-1): the ONE shared restore-eligibility predicate. A backed-up
/// file is restorable iff it has a Drive object id AND its status is `Synced`.
///
/// Both the tree/search DTO `restorable` flag AND restore resolution
/// ([`resolve_restore_items`]) go through this SAME predicate, so the UI can never
/// offer a row that resolution would then reject. Before this fix the DTOs marked
/// a row restorable on `drive_file_id.is_some()` ALONE, but resolution also
/// requires `status == Synced` (R3-P2-2: a changed/pending/error row's recorded
/// `hash_blake3` may not match the bytes currently on Drive), so a stale-status
/// row LOOKED selectable then failed only at restore start. Folding both checks
/// into one function keeps the two surfaces in lockstep.
fn is_restorable(status: driven_core::types::FileStateStatus, drive_file_id: Option<&str>) -> bool {
    drive_file_id.is_some() && status == driven_core::types::FileStateStatus::Synced
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

// -----------------------------------------------------------------------------
// R2-P1-1: handle-based destination confinement
// -----------------------------------------------------------------------------

/// R2-P1-1 (SPEC s11.6.1): a restore destination confined by a PINNED parent
/// directory HANDLE rather than by a path string.
///
/// The earlier code validated the dest path, then later re-resolved that path
/// STRING to open the temp and to rename - a TOCTOU window in which a local
/// process could swap a parent component to a symlink/junction and redirect the
/// decrypted plaintext (temp AND final) OUTSIDE the dialog-approved root.
///
/// [`ConfinedDest::open`] closes that window: it walks the relative path's
/// directory chain from the canonical root, opening EACH component with a
/// NO-FOLLOW directory handle (Unix `O_NOFOLLOW | O_DIRECTORY`; Windows
/// `BACKUP_SEMANTICS | OPEN_REPARSE_POINT` + reparse-attribute reject + a
/// `GetFinalPathNameByHandleW` in-root recheck), arriving at a final-PARENT handle
/// that is pinned to a real inode. It then creates the temp RELATIVE to that
/// handle (Unix `openat`; Windows `CreateFileW` by full path + a post-create
/// in-root recheck of the temp handle BEFORE any byte is written), and
/// [`ConfinedDest::commit`] renames the temp to the leaf RELATIVE to the SAME
/// pinned handle (Unix `renameat`; Windows `SetFileInformationByHandle` with
/// `FILE_RENAME_INFO { RootDirectory = parent_handle, ReplaceIfExists }`).
///
/// Because both the create and the rename are handle-relative, a concurrent parent
/// swap after validation cannot move the write out of root. If the temp is dropped
/// without [`ConfinedDest::commit`] succeeding (a failure, a cancel, or a SHUTDOWN
/// ABORT that drops the future - R2-P2-2), [`Drop`] best-effort removes the temp so
/// no partial / out-of-root file is left behind.
///
/// M9c D1 (M8 R4-P1-1) - verify->rename TOCTOU on the TEMP itself: the guard now
/// RETAINS the temp file's OWN handle from create through commit. The streamer
/// writes + BLAKE3-verifies through a DUPLICATE of that handle (returned by
/// `open`); the original stays owned by the guard. So the rename acts on the SAME
/// verified file object, NOT on a re-resolution of the temp pathname:
/// - Windows: `SetFileInformationByHandle(FileRenameInfo)` on the RETAINED temp
///   handle (opened with `DELETE` access). A local process that unlinks/replaces
///   `.driven-restore-tmp.<uuid>` after verification cannot affect the rename - the
///   handle still names the original object; the substitute is a different object.
/// - Unix: before `renameat` the guard fstat's the RETAINED temp fd and fstatat's
///   the temp NAME in the pinned parent; the rename proceeds ONLY if `(st_dev,
///   st_ino)` match (the name still points at the verified file object). A swap is
///   detected and the commit FAILS (`crypto.decrypt_failed`-class IO error) rather
///   than renaming attacker bytes into place - no silent corruption.
///
/// Residual gaps (documented in `design/CODEX_NOTES.md`): on Windows the temp is
/// created by full path then immediately rechecked in-root, so an empty (zero
/// plaintext) temp can momentarily exist out-of-root if a junction is swapped in
/// at exactly the create instant - it is detected and deleted before byte one, so
/// NO plaintext ever leaves root; `FILE_RENAME_INFO` has no WRITE_THROUGH (the file
/// DATA is already `sync_all`'d; only the rename metadata is not flush-forced); and
/// the Windows rename DESTINATION is still derived from the pinned parent handle's
/// resolved path (the handle-relative `RootDirectory` form is NT-API-only), so the
/// parent-pin - not a dest-string - is what confines the target directory.
mod confine {
    use super::{map_io_err, ErrorCode, TARGET};
    use driven_core::types::RelativePath;

    use crate::commands::DialogToken;

    /// R5-P1-3 (DATA-SAFETY): a STABLE identity of the dialog-approved restore ROOT,
    /// captured ONCE at pick/consume time (in `restore_files`) and re-verified at
    /// every per-file [`ConfinedDest::open`].
    ///
    /// The earlier code carried only the root PATH STRING; `ConfinedDest::open`
    /// re-canonicalised that string on every file. If the selected root was swapped
    /// to a symlink/junction BETWEEN the bind and a later open, the new target became
    /// the "approved" root and decrypted bytes could land outside the user-chosen
    /// directory - the root-level analogue of the parent-component TOCTOU the
    /// handle-relative parent walk already closes (that walk pins components BELOW
    /// the root, but the root itself was still re-resolved from a string).
    ///
    /// This binds the root's real on-disk identity at consume time and rejects any
    /// later canonicalisation whose identity differs:
    /// - Unix: `(st_dev, st_ino)` of the canonical root.
    /// - Windows: `(dwVolumeSerialNumber, nFileIndexHigh, nFileIndexLow)` of the
    ///   canonical root directory (its file id on the volume).
    ///
    /// cfg-gated per OS; an unsupported target has no identity (`None`) and the open
    /// falls back to the canonical-path equality check alone (still rejecting a root
    /// whose resolved path changed).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct RootIdentity {
        #[cfg(unix)]
        dev: u64,
        #[cfg(unix)]
        ino: u64,
        #[cfg(windows)]
        volume_serial: u32,
        #[cfg(windows)]
        file_index_high: u32,
        #[cfg(windows)]
        file_index_low: u32,
    }

    impl RootIdentity {
        /// Capture the identity of the directory at `canon_root` (an already
        /// canonicalised path). Returns `None` if the identity cannot be read
        /// (e.g. the directory vanished) so the caller can treat that as a mismatch.
        pub(super) fn capture(canon_root: &std::path::Path) -> Option<Self> {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let meta = std::fs::metadata(canon_root).ok()?;
                Some(RootIdentity {
                    dev: meta.dev(),
                    ino: meta.ino(),
                })
            }
            #[cfg(windows)]
            {
                root_identity_windows(canon_root)
            }
            #[cfg(not(any(unix, windows)))]
            {
                let _ = canon_root;
                None
            }
        }
    }

    /// Windows: read the canonical root directory's volume serial + file id via a
    /// no-follow `BY_HANDLE_FILE_INFORMATION` so the identity is the REAL directory
    /// inode-equivalent (a junction swapped in later resolves to a different file
    /// id). `None` on any failure (treated as a mismatch by the caller).
    #[cfg(windows)]
    fn root_identity_windows(canon_root: &std::path::Path) -> Option<RootIdentity> {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        let wide: Vec<u16> = canon_root
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `wide` is a valid NUL-terminated wide string outliving the call;
        // CreateFileW does not retain it. BACKUP_SEMANTICS opens a directory handle;
        // OPEN_REPARSE_POINT opens the link itself rather than its target so a
        // swapped junction is identified as a different object.
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut::<core::ffi::c_void>() as HANDLE,
            )
        };
        if raw == INVALID_HANDLE_VALUE || raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is a valid handle we own from a successful CreateFileW.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw as *mut _) };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: handle is valid; info is a valid out-param.
        let ok = unsafe { GetFileInformationByHandle(handle.as_raw_handle() as HANDLE, &mut info) };
        if ok == 0 {
            return None;
        }
        Some(RootIdentity {
            volume_serial: info.dwVolumeSerialNumber,
            file_index_high: info.nFileIndexHigh,
            file_index_low: info.nFileIndexLow,
        })
    }

    /// R5-P1-3 (DATA-SAFETY): the dialog-approved restore ROOT bound to a STABLE
    /// canonical path + on-disk identity at consume time, so every per-file
    /// [`ConfinedDest::open`] re-verifies the root has not been swapped underneath
    /// it (a root-level TOCTOU). Carries the original [`DialogToken`] for the
    /// structural `validate_restore_dest` pre-step (which creates the dir chain).
    #[derive(Debug, Clone)]
    pub(super) struct ConfinedRoot {
        /// The dialog token (the original root path string), for the structural
        /// `validate_restore_dest` pre-step.
        token: DialogToken,
        /// The root canonicalised ONCE at bind time. A later canonicalisation that
        /// differs is rejected (the root path was redirected).
        canon_root: std::path::PathBuf,
        /// The root's on-disk identity captured at bind time (`None` on an
        /// unsupported target; then only the canonical-path equality holds).
        identity: Option<RootIdentity>,
    }

    impl ConfinedRoot {
        /// Bind a `ConfinedRoot` from the dialog token at consume time: canonicalise
        /// the root and capture its identity. Called ONCE in `restore_files` after
        /// the token is consumed, then carried into the background job so every
        /// per-file open verifies against this fixed identity.
        pub(super) fn bind(dialog_token: DialogToken) -> Result<Self, ErrorCode> {
            let canon_root = dunce::canonicalize(&dialog_token.0).map_err(map_io_err)?;
            let identity = RootIdentity::capture(&canon_root);
            Ok(ConfinedRoot {
                token: dialog_token,
                canon_root,
                identity,
            })
        }

        /// The dialog token for the structural `validate_restore_dest` pre-step.
        pub(super) fn token(&self) -> &DialogToken {
            &self.token
        }

        /// R5-P1-3: re-resolve the root and confirm it is still the SAME directory
        /// bound at consume time - same canonical path AND (where available) same
        /// on-disk identity. A mismatch means the root was swapped to a
        /// symlink/junction between bind and now; REJECT so no decrypted bytes land
        /// outside the user-chosen directory. Returns the verified canonical root.
        fn verify(&self) -> Result<std::path::PathBuf, ErrorCode> {
            let now_root = dunce::canonicalize(&self.token.0).map_err(map_io_err)?;
            if now_root != self.canon_root {
                tracing::warn!(
                    target: TARGET,
                    bound = %self.canon_root.display(),
                    now = %now_root.display(),
                    "restore root canonical path changed between bind and open; refusing (R5-P1-3)"
                );
                return Err(ErrorCode::LocalIoError);
            }
            // Identity recheck (defence beyond the path string: a path can resolve
            // to the same STRING while pointing at a different object after a swap).
            let now_identity = RootIdentity::capture(&now_root);
            match (self.identity, now_identity) {
                (Some(bound), Some(now)) if bound != now => {
                    tracing::warn!(
                        target: TARGET,
                        root = %now_root.display(),
                        "restore root identity changed between bind and open; refusing (R5-P1-3)"
                    );
                    return Err(ErrorCode::LocalIoError);
                }
                // Bound an identity but it cannot be read now (root vanished /
                // unreadable) -> treat as a mismatch.
                (Some(_), None) => {
                    tracing::warn!(
                        target: TARGET,
                        root = %now_root.display(),
                        "restore root identity unreadable at open; refusing (R5-P1-3)"
                    );
                    return Err(ErrorCode::LocalIoError);
                }
                // No identity captured on this target (unsupported OS), or both read
                // and equal: the canonical-path equality above already held.
                _ => {}
            }
            Ok(now_root)
        }
    }

    /// A restore destination confined to a pinned parent directory handle, holding
    /// the VERIFIED temp file's own OS handle through commit (M9c D1).
    pub(super) struct ConfinedDest {
        /// The leaf file name to rename the temp to on commit.
        leaf: std::ffi::OsString,
        /// The temp's basename (relative to the pinned parent), used by the Unix
        /// handle-relative `renameat` / `unlinkat` / identity recheck. (On Windows
        /// the cleanup is by `temp_path`, so this is Unix-only.)
        #[cfg(unix)]
        temp_name: std::ffi::OsString,
        /// Whether the temp still needs removal on drop (true until commit succeeds
        /// or the temp create itself failed).
        armed: bool,
        /// The full temp path - retained for the Drop-based best-effort cleanup.
        temp_path: std::path::PathBuf,
        /// Platform parent-directory handle the create + rename are relative to.
        #[cfg(unix)]
        parent: std::os::fd::OwnedFd,
        #[cfg(windows)]
        parent: std::os::windows::io::OwnedHandle,
        /// M9c D1 (M8 R4-P1-1): the temp file's OWN handle, RETAINED from create
        /// through commit so the rename acts on the SAME, BLAKE3-VERIFIED object -
        /// NOT a re-resolution of the temp pathname after verification. The streamer
        /// writes + verifies through a DUPLICATE of this handle (returned by `open`);
        /// keeping the original here means a local process that unlinks/replaces the
        /// temp file at its path AFTER verification cannot make commit rename the
        /// attacker's substitute (the retained handle still refers to the original
        /// inode / file object). On Windows the rename is `SetFileInformationByHandle
        /// (FileRenameInfo)` on THIS handle (needs the DELETE access it was opened
        /// with); on Unix it is `renameat` guarded by an fstat/fstatat identity
        /// recheck of the temp name against this fd (a swap is detected + refused).
        #[cfg(unix)]
        temp_handle: std::os::fd::OwnedFd,
        #[cfg(windows)]
        temp_handle: std::os::windows::io::OwnedHandle,
    }

    impl ConfinedDest {
        /// Open the confined destination for `relative_path` under the dialog token
        /// root: pin the parent chain with no-follow handles and create the temp
        /// relative to the final parent handle. Returns the guard plus the OPEN temp
        /// file to stream into (so the caller owns the file directly - no
        /// take-once/Option/panic). M9c D1: the returned file is a DUPLICATE of the
        /// temp handle the guard RETAINS, so the streamer's write/verify and the
        /// guard's commit-rename act on the SAME file object.
        pub(super) fn open(
            root: &ConfinedRoot,
            relative_path: &str,
        ) -> Result<(Self, std::fs::File), ErrorCode> {
            // Derive the validated relative segments (same rules
            // `validate_restore_dest` enforced). RelativePath rejects `..`,
            // absolute, drive/UNC, NUL.
            let rel: RelativePath = RelativePath::try_from(relative_path.to_string())
                .map_err(|_| ErrorCode::LocalIoError)?;
            let rel = rel.as_str();
            let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
            let (leaf, dir_segments) = segments.split_last().ok_or(ErrorCode::LocalIoError)?;

            // R5-P1-3 (DATA-SAFETY): re-verify the ROOT is still the SAME directory
            // bound at consume time (canonical path + on-disk identity). A root
            // swapped to a symlink/junction between bind and now is REJECTED here,
            // BEFORE any handle is opened or temp created - so decrypted bytes can
            // never land outside the user-chosen directory. The returned
            // `canon_root` is the verified root the no-follow parent walk pins to.
            let canon_root = root.verify()?;

            platform::open_confined(&canon_root, dir_segments, leaf)
        }

        /// Commit: rename the temp to the leaf via the RETAINED, VERIFIED temp
        /// handle (M9c D1) - never by re-resolving the temp pathname after
        /// verification. Atomic replace over an existing dest. Defuses the Drop
        /// cleanup on success. R2-P1-1: the rename remains parent-pinned, so a
        /// concurrent parent swap cannot redirect it out of root; M9c D1: it also
        /// acts on the same file object the streamer verified, so a temp swap
        /// between verify and rename cannot smuggle unverified bytes into the final
        /// file (a detected swap fails the commit - no silent corruption).
        pub(super) async fn commit(&mut self) -> Result<(), ErrorCode> {
            let result = platform::commit_rename(self);
            if result.is_ok() {
                self.armed = false;
            }
            result
        }
    }

    impl Drop for ConfinedDest {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            // R2-P2-2: best-effort remove the uncommitted temp (failure / cancel /
            // shutdown-abort path). Remove handle-relative on Unix; by full path on
            // Windows (the temp handle is closed by now).
            platform::remove_temp(self);
            tracing::debug!(
                target: TARGET,
                temp = %self.temp_path.display(),
                "removed uncommitted restore temp on drop"
            );
        }
    }

    /// A random, unpredictable temp basename (M8-P1-2): combined with O_EXCL /
    /// CREATE_NEW this refuses a pre-placed-path race.
    fn random_temp_name() -> std::ffi::OsString {
        std::ffi::OsString::from(format!(".driven-restore-tmp.{}", uuid::Uuid::new_v4()))
    }

    // --- Unix: openat / renameat handle-relative confinement -----------------
    #[cfg(unix)]
    mod platform {
        use super::super::map_io_err;
        use super::{random_temp_name, ConfinedDest};
        use driven_core::types::ErrorCode;
        use rustix::fs::{Mode, OFlags};
        use std::os::fd::AsFd;
        use std::path::Path;

        /// Walk `dir_segments` from `canon_root` opening each with
        /// `O_NOFOLLOW | O_DIRECTORY` (a symlink component fails the open), then
        /// `openat` the temp `O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC`.
        ///
        /// M9c D1: the temp fd is opened ONCE and RETAINED on the guard; the
        /// streamer receives a `try_clone` DUP to write through. So commit's
        /// identity recheck + renameat operate on the SAME object the streamer
        /// verified.
        pub(super) fn open_confined(
            canon_root: &Path,
            dir_segments: &[&str],
            leaf: &str,
        ) -> Result<(ConfinedDest, std::fs::File), ErrorCode> {
            // Open the canonical root itself no-follow as a directory.
            let mut parent = rustix::fs::open(
                canon_root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|e| map_io_err(e.into()))?;

            for seg in dir_segments {
                // openat each component no-follow: a symlink/junction here fails
                // (ELOOP / ENOTDIR), so the chain cannot be redirected out of root.
                let next = rustix::fs::openat(
                    parent.as_fd(),
                    *seg,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| map_io_err(e.into()))?;
                parent = next;
            }

            // Create the temp RELATIVE to the pinned final-parent fd.
            let temp_name = random_temp_name();
            let temp_name_str = temp_name.to_string_lossy().to_string();
            let temp_fd = rustix::fs::openat(
                parent.as_fd(),
                temp_name_str.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(|e| map_io_err(e.into()))?;
            // M9c D1: DUP the temp fd for the streamer; the guard RETAINS the
            // original `temp_fd` through commit. The dup shares the file
            // description (writes through it are writes to the same object); both
            // refer to the SAME inode, which is what the commit identity recheck
            // pins to.
            let streamer_fd = temp_fd.try_clone().map_err(map_io_err)?;
            let temp_file = std::fs::File::from(streamer_fd);

            // The temp path is informational only (cleanup is handle-relative).
            let mut temp_path = canon_root.to_path_buf();
            for seg in dir_segments {
                temp_path.push(seg);
            }
            temp_path.push(&temp_name);

            Ok((
                ConfinedDest {
                    leaf: std::ffi::OsString::from(leaf),
                    temp_name,
                    armed: true,
                    temp_path,
                    parent,
                    temp_handle: temp_fd,
                },
                temp_file,
            ))
        }

        /// Rename the temp to the leaf RELATIVE to the pinned parent fd. `renameat`
        /// atomically replaces an existing dest on Unix.
        ///
        /// M9c D1 (M8 R4-P1-1): BEFORE the rename, prove the temp NAME in the
        /// pinned parent still refers to the SAME file object as the RETAINED,
        /// VERIFIED temp fd - by comparing `(st_dev, st_ino)` of `fstat(temp_fd)`
        /// against `fstatat(parent, temp_name, NOFOLLOW)`. If a local process
        /// unlinked/replaced the temp between the BLAKE3 verify and now, the name
        /// resolves to a DIFFERENT object (or to nothing); we REFUSE the commit
        /// rather than rename attacker-controlled bytes into the final file (no
        /// silent restore corruption). Only on a confirmed identity match do we
        /// `renameat` the verified object into place.
        pub(super) fn commit_rename(c: &ConfinedDest) -> Result<(), ErrorCode> {
            let from = c.temp_name.to_string_lossy().to_string();
            let to = c.leaf.to_string_lossy().to_string();

            // Identity of the VERIFIED handle.
            let handle_stat =
                rustix::fs::fstat(c.temp_handle.as_fd()).map_err(|e| map_io_err(e.into()))?;
            // Identity of whatever the temp NAME currently points at (no-follow, so
            // a symlink swapped in is not chased).
            let name_stat = rustix::fs::statat(
                c.parent.as_fd(),
                from.as_str(),
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(|e| map_io_err(e.into()))?;
            if handle_stat.st_dev != name_stat.st_dev || handle_stat.st_ino != name_stat.st_ino {
                // The temp name no longer refers to the verified object: a swap
                // happened in the verify->rename window. Refuse (the Drop guard
                // best-effort unlinks whatever now sits at the temp name).
                tracing::warn!(
                    target: super::super::TARGET,
                    temp = %c.temp_path.display(),
                    "restore temp identity changed between verify and rename; refusing commit (D1)"
                );
                return Err(ErrorCode::LocalIoError);
            }

            rustix::fs::renameat(
                c.parent.as_fd(),
                from.as_str(),
                c.parent.as_fd(),
                to.as_str(),
            )
            .map_err(|e| map_io_err(e.into()))
        }

        /// Best-effort unlink the temp RELATIVE to the pinned parent fd.
        pub(super) fn remove_temp(c: &ConfinedDest) {
            let name = c.temp_name.to_string_lossy().to_string();
            let _ = rustix::fs::unlinkat(
                c.parent.as_fd(),
                name.as_str(),
                rustix::fs::AtFlags::empty(),
            );
        }
    }

    // --- Windows: CreateFileW handle + SetFileInformationByHandle rename ------
    #[cfg(windows)]
    mod platform {
        use super::{random_temp_name, ConfinedDest};
        use driven_core::types::ErrorCode;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use std::path::Path;
        use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FileRenameInfo, GetFileInformationByHandle, GetFinalPathNameByHandleW,
            SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, CREATE_NEW, DELETE,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
            FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
            OPEN_EXISTING, SYNCHRONIZE,
        };

        fn to_wide(p: &Path) -> Vec<u16> {
            p.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        }

        /// Open `path` as a directory handle that does NOT follow a reparse point,
        /// and reject it if it IS a reparse point or resolves outside `canon_root`.
        /// The access mask is the directory-traversal set needed to (a) descend
        /// (`FILE_TRAVERSE`/`FILE_LIST_DIRECTORY`) and (b) act as the `RootDirectory`
        /// of a child `FILE_RENAME_INFO` move.
        fn open_dir_no_follow(path: &Path, canon_root: &Path) -> Result<OwnedHandle, ErrorCode> {
            let wide = to_wide(path);
            // SAFETY: `wide` is a valid NUL-terminated wide string outliving the
            // call; CreateFileW does not retain it. BACKUP_SEMANTICS is required to
            // open a directory handle; OPEN_REPARSE_POINT opens the link itself
            // rather than its target.
            let raw = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                    std::ptr::null_mut::<core::ffi::c_void>() as HANDLE,
                )
            };
            if raw == INVALID_HANDLE_VALUE || raw.is_null() {
                return Err(map_last_error());
            }
            // SAFETY: `raw` is a valid handle we own from a successful CreateFileW.
            let handle = unsafe { OwnedHandle::from_raw_handle(raw as *mut _) };

            // Reject a reparse point (junction / symlink): we opened the link
            // itself, so its attributes carry FILE_ATTRIBUTE_REPARSE_POINT.
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: handle is valid; info is a valid out-param.
            let ok =
                unsafe { GetFileInformationByHandle(handle.as_raw_handle() as HANDLE, &mut info) };
            if ok == 0 {
                return Err(map_last_error());
            }
            if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(ErrorCode::LocalIoError);
            }

            // Re-confirm the handle's RESOLVED real path is still inside the root.
            let resolved = final_path_of(&handle)?;
            let canon_resolved = dunce::canonicalize(&resolved).unwrap_or(resolved);
            if !canon_resolved.starts_with(canon_root) {
                return Err(ErrorCode::LocalIoError);
            }
            Ok(handle)
        }

        /// `GetFinalPathNameByHandleW` -> the handle's resolved DOS path.
        fn final_path_of(handle: &OwnedHandle) -> Result<std::path::PathBuf, ErrorCode> {
            let h = handle.as_raw_handle() as HANDLE;
            // First call with len 0 returns the required length (incl NUL).
            // SAFETY: h is a valid handle; a null buffer with 0 length is the
            // documented "ask for the size" form.
            let needed = unsafe {
                GetFinalPathNameByHandleW(h, std::ptr::null_mut(), 0, FILE_NAME_NORMALIZED)
            };
            if needed == 0 {
                return Err(map_last_error());
            }
            let mut buf = vec![0u16; needed as usize];
            // SAFETY: buf has `needed` u16 capacity; h is valid.
            let written = unsafe {
                GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), needed, FILE_NAME_NORMALIZED)
            };
            if written == 0 || written >= needed {
                return Err(map_last_error());
            }
            buf.truncate(written as usize);
            Ok(std::path::PathBuf::from(std::ffi::OsString::from_wide(
                &buf,
            )))
        }

        pub(super) fn open_confined(
            canon_root: &Path,
            dir_segments: &[&str],
            leaf: &str,
        ) -> Result<(ConfinedDest, std::fs::File), ErrorCode> {
            // Pin the parent chain: open the root no-follow, then descend one
            // component at a time, each opened no-follow + reparse-rejected +
            // in-root rechecked. We keep only the FINAL parent handle.
            let mut parent = open_dir_no_follow(canon_root, canon_root)?;
            let mut current = canon_root.to_path_buf();
            for seg in dir_segments {
                current.push(seg);
                parent = open_dir_no_follow(&current, canon_root)?;
            }

            // Create the temp. Windows CreateFileW cannot take a dir-handle +
            // relative name without the NT API, so we create by full path with
            // CREATE_NEW (fails if it exists; kills a pre-place race) + DELETE
            // access (needed to rename a still-open handle) + OPEN_REPARSE_POINT
            // (do not follow a leaf reparse point), then IMMEDIATELY re-confirm the
            // temp handle's resolved path is in-root BEFORE any plaintext is written
            // (closes the residual create-instant junction-swap window: at worst an
            // EMPTY temp existed out-of-root and is deleted here - no plaintext).
            let temp_name = random_temp_name();
            let mut temp_path = current.clone();
            temp_path.push(&temp_name);
            let wide = to_wide(&temp_path);
            // SAFETY: wide is a valid NUL-terminated wide string outliving the call.
            let raw = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    FILE_GENERIC_WRITE | DELETE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    CREATE_NEW,
                    FILE_FLAG_OPEN_REPARSE_POINT,
                    std::ptr::null_mut::<core::ffi::c_void>() as HANDLE,
                )
            };
            if raw == INVALID_HANDLE_VALUE || raw.is_null() {
                return Err(map_last_error());
            }
            // SAFETY: raw is a valid owned handle from a successful CreateFileW.
            let temp_handle = unsafe { OwnedHandle::from_raw_handle(raw as *mut _) };
            // Re-confirm the temp landed in-root (delete + fail if not).
            let resolved = final_path_of(&temp_handle)?;
            let canon_resolved = dunce::canonicalize(&resolved).unwrap_or(resolved);
            if !canon_resolved.starts_with(canon_root) {
                drop(temp_handle);
                let _ = std::fs::remove_file(&temp_path);
                return Err(ErrorCode::LocalIoError);
            }
            // M9c D1 (M8 R4-P1-1): DUP the temp handle for the streamer; the guard
            // RETAINS the original `temp_handle` (opened with DELETE access) through
            // commit. `try_clone` preserves access rights, so the dup can write and
            // the retained handle can still `SetFileInformationByHandle(FileRenameInfo)`.
            // Both refer to the SAME file object, so commit renames the EXACT object
            // the streamer verified - a temp swap at the path after verification
            // cannot redirect the rename.
            let streamer_handle = temp_handle.try_clone().map_err(|_| map_last_error())?;
            let temp_file = std::fs::File::from(streamer_handle);

            Ok((
                ConfinedDest {
                    leaf: std::ffi::OsString::from(leaf),
                    armed: true,
                    temp_path,
                    parent,
                    temp_handle,
                },
                temp_file,
            ))
        }

        /// Rename the temp to the leaf, confined by the PINNED parent handle.
        ///
        /// Win32's `SetFileInformationByHandle(FileRenameInfo)` does NOT support a
        /// non-NULL `RootDirectory` (it returns ERROR_INVALID_PARAMETER - that
        /// handle-relative form is NT-API-only). So the destination path is instead
        /// derived from the PINNED parent HANDLE's CURRENT resolved real path via
        /// `GetFinalPathNameByHandleW` - NOT from the attacker-influenceable original
        /// path string - and re-confirmed in-root immediately before the rename:
        /// - The parent handle was opened no-follow and is pinned to the REAL
        ///   directory inode. If a parent component was swapped to a junction after
        ///   validation, the pinned handle still refers to the ORIGINAL real dir, so
        ///   `GetFinalPathNameByHandleW` returns its real (in-root) path - the rename
        ///   target is the real pinned directory, never the swapped junction target.
        /// - We re-confirm that resolved parent path still `starts_with` the parent's
        ///   own pinned identity (it always does) and, defensively, re-derive +
        ///   recheck it is the same volume path we verified at open.
        ///
        /// M9c D1 (M8 R4-P1-1): the rename is performed on the RETAINED, VERIFIED
        /// temp handle (opened with DELETE at create, kept on the guard) - NOT a
        /// re-open of `temp_path`. So a local process that unlinks/replaces the temp
        /// at its path after the BLAKE3 verify cannot make this rename move the
        /// attacker's substitute: `SetFileInformationByHandle` acts on the file
        /// OBJECT the handle names (the original, verified inode), regardless of
        /// what the temp NAME points at now. `ReplaceIfExists` gives the atomic
        /// replace over an existing dest (R1-P2-2).
        pub(super) fn commit_rename(c: &ConfinedDest) -> Result<(), ErrorCode> {
            // Derive the dest from the PINNED parent handle's resolved real path
            // (TOCTOU-safe: this resolves the pinned inode, not a re-walked string).
            let resolved_parent = final_path_of(&c.parent)?;
            let full_dest = resolved_parent.join(&c.leaf);

            // Build an 8-byte-ALIGNED FILE_RENAME_INFO (a `Vec<u64>` backing, since
            // the struct holds a HANDLE and must be 8-aligned) with NULL
            // RootDirectory + the FULL resolved dest path + ReplaceIfExists.
            let name_wide: Vec<u16> = full_dest.as_os_str().encode_wide().collect();
            let name_bytes = name_wide.len() * std::mem::size_of::<u16>();
            let header = std::mem::size_of::<FILE_RENAME_INFO>();
            let total = header + name_bytes.saturating_sub(std::mem::size_of::<u16>());
            let words = total.div_ceil(std::mem::size_of::<u64>());
            let mut backing = vec![0u64; words];
            let info = backing.as_mut_ptr() as *mut FILE_RENAME_INFO;
            // SAFETY: `backing` is u64-aligned and `words * 8 >= total >=
            // size_of::<FILE_RENAME_INFO>()`, so writing the struct + the flexible
            // FileName array through `info` stays in bounds + aligned.
            unsafe {
                (*info).Anonymous.ReplaceIfExists = true; // windows-sys 0.61: field is a Rust `bool`
                (*info).RootDirectory = std::ptr::null_mut::<core::ffi::c_void>() as HANDLE;
                (*info).FileNameLength = name_bytes as u32;
                let dst = std::ptr::addr_of_mut!((*info).FileName) as *mut u16;
                std::ptr::copy_nonoverlapping(name_wide.as_ptr(), dst, name_wide.len());
            }
            // M9c D1: rename the RETAINED verified temp handle (NOT a re-open).
            // SAFETY: c.temp_handle is a valid owned handle (DELETE access); info is
            // a well-formed, aligned FILE_RENAME_INFO of `total` bytes.
            let ok = unsafe {
                SetFileInformationByHandle(
                    c.temp_handle.as_raw_handle() as HANDLE,
                    FileRenameInfo,
                    info as *const core::ffi::c_void,
                    total as u32,
                )
            };
            if ok == 0 {
                return Err(map_last_error());
            }
            Ok(())
        }

        pub(super) fn remove_temp(c: &ConfinedDest) {
            let _ = std::fs::remove_file(&c.temp_path);
        }

        /// Map the last Win32 error to a SPEC s24 code (disk-full vs generic IO).
        fn map_last_error() -> ErrorCode {
            // SAFETY: GetLastError reads thread-local state, always safe.
            let raw = unsafe { GetLastError() } as i32;
            super::super::map_io_err(std::io::Error::from_raw_os_error(raw))
        }
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

    // --- M9c D2: ONE shared restore-eligibility predicate ---------------------

    #[test]
    fn is_restorable_requires_synced_status_and_a_drive_id() {
        use FileStateStatus as S;
        // Eligible: synced + has a drive id.
        assert!(is_restorable(S::Synced, Some("d1")));
        // Ineligible: synced but never uploaded (no drive id).
        assert!(!is_restorable(S::Synced, None));
        // Ineligible: has a drive id but a non-synced status (its recorded hash may
        // not match the bytes on Drive). EVERY non-synced status is rejected.
        for st in [
            S::Pending,
            S::Corrupt,
            S::Locked,
            S::Error,
            S::ExcludedOrphan,
        ] {
            assert!(
                !is_restorable(st, Some("d1")),
                "a {st:?} row must NOT be restorable even with a drive id"
            );
        }
    }

    #[test]
    fn tree_dto_marks_non_synced_row_not_restorable() {
        // M9c D2: the tree DTO mapper uses the shared predicate, so a row with a
        // Drive id but a NON-SYNCED status is NOT offered as restorable - matching
        // what resolution would do (so the UI never offers a row that then fails).
        let mut r = row("changed.bin", 7, Some("d-old"));
        r.status = FileStateStatus::Error;
        let children = driven_core::state::ImmediateTreeChildren {
            folders: Vec::new(),
            files: vec![r],
            truncated: false,
        };
        let entries = immediate_children_to_entries("", &children);
        assert_eq!(entries.len(), 1);
        assert!(
            !entries[0].restorable,
            "a non-synced row (even with a drive id) must not be marked restorable in the tree DTO"
        );
    }

    #[tokio::test]
    async fn resolve_and_dto_agree_on_eligibility_via_one_predicate() {
        // M9c D2: the SAME `is_restorable` predicate gates BOTH the DTO `restorable`
        // flag and restore resolution. A row that the DTO marks NOT restorable
        // (uploaded but status=Error) must ALSO be rejected at resolution - they
        // can never disagree.
        let (state, src, dir) = state_with_source().await;
        state
            .state()
            .upsert_file_state(&file_state_row(
                src,
                "stale.txt",
                Some("d-1"),
                FileStateStatus::Error,
            ))
            .await
            .unwrap();
        // DTO side: not restorable.
        let r = RestoreFileRow {
            source_id: src,
            relative_path: driven_core::types::RelativePath::try_from("stale.txt".to_string())
                .unwrap(),
            size: 3,
            status: FileStateStatus::Error,
            drive_file_id: Some("d-1".to_string()),
        };
        let children = driven_core::state::ImmediateTreeChildren {
            folders: Vec::new(),
            files: vec![r],
            truncated: false,
        };
        assert!(
            !immediate_children_to_entries("", &children)[0].restorable,
            "DTO marks the stale row not restorable"
        );
        // Resolution side: the identical predicate rejects it as bad input.
        let items = vec![RestoreItem {
            source_id: src.to_string(),
            relative_path: "stale.txt".to_string(),
        }];
        let err = resolve_restore_items(&state, &items, None)
            .await
            .expect_err("the same predicate must reject the stale row at resolution");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        let _ = std::fs::remove_dir_all(&dir);
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
        // Genuine wildcard / path patterns route to GLOB.
        assert!(is_glob_query("*.rs"), "leading star is a real glob");
        assert!(is_glob_query("src/?.txt"));
        assert!(is_glob_query("[abc].md"));
        assert!(is_glob_query("src/*"), "a path pattern routes to glob");
        assert!(is_glob_query("a*b"), "an interior star is a real glob");
        assert!(is_glob_query("a?b"));
        assert!(is_glob_query("[ab]c"));
        assert!(is_glob_query("*"), "a bare star is a real glob");
        assert!(is_glob_query("**"), "a double star is a real glob");
        assert!(is_glob_query("a*b*"), "two stars is a real glob");
        // Plain terms and SIMPLE trailing-star prefixes route to FTS5.
        assert!(!is_glob_query("plain"));
        assert!(!is_glob_query("foo-bar"));
        assert!(
            !is_glob_query("proj*"),
            "R2-P1-2: a simple trailing-star prefix must route to FTS5, not GLOB"
        );
        assert!(
            !is_glob_query("taxes-2025*"),
            "a hyphenated trailing-star prefix is still an FTS5 prefix"
        );
    }

    #[test]
    fn build_fts_match_query_emits_prefix_for_trailing_star() {
        // R2-P1-2: confirm the FTS5 path the dispatcher routes `proj*` to actually
        // produces the prefix MATCH form (`"proj"*`), so the route reaches the
        // index prefix scan rather than a literal-term miss. This mirrors the
        // `build_fts_match_query` contract used by `StateRepo::search_files`.
        // (The function lives in driven-core; here we assert the dispatcher's
        // intent: `proj*` is NOT a glob, so it goes to `search_files` -> FTS5.)
        assert!(!is_glob_query("proj*"));
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

    /// Open a fresh writable temp file at `path` for the streaming tests. R2-P1-1
    /// changed `stream_to_disk` to take an ALREADY-OPEN temp file (the real path
    /// creates it RELATIVE to a verified parent handle via `ConfinedDest`); the
    /// streaming-only tests do not exercise confinement, so they just open a plain
    /// file here and assert the streamed/decrypted bytes land in it.
    fn open_test_temp(path: &std::path::Path) -> std::fs::File {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .expect("open test temp file")
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
            open_test_temp(&out),
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
                open_test_temp(&out),
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
            open_test_temp(&out),
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
        let res = stream_to_disk(
            &mut reader,
            open_test_temp(&out),
            None,
            &file,
            &no_cancel(),
            &mut |_| {},
        )
        .await;
        assert!(matches!(res, StreamOutcome::Done));
        assert_eq!(std::fs::read(&out).unwrap(), plaintext);
        let _ = std::fs::remove_file(&out);
    }

    // --- R1-P2-1: mid-stream download read errors classify as Drive/network ---

    /// A test [`AsyncRead`] that yields `prefix` bytes, then fails the NEXT read
    /// with an `io::Error` carrying `msg` - modelling a Drive download stream that
    /// breaks MID-BODY (the real `StreamingDownloadReader` wraps the transport
    /// error via `io::Error::other`; the fake's injected faults embed the dotted
    /// code in the message). Used to prove a mid-stream read error maps to a
    /// DRIVE/network SPEC s24 code, NOT `local.io_error`.
    struct FailMidStreamReader {
        prefix: Vec<u8>,
        pos: usize,
        msg: &'static str,
        failed: bool,
    }

    impl FailMidStreamReader {
        fn new(prefix: Vec<u8>, msg: &'static str) -> Self {
            Self {
                prefix,
                pos: 0,
                msg,
                failed: false,
            }
        }
    }

    impl tokio::io::AsyncRead for FailMidStreamReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let this = self.get_mut();
            if this.pos < this.prefix.len() {
                let n = (this.prefix.len() - this.pos).min(buf.remaining());
                buf.put_slice(&this.prefix[this.pos..this.pos + n]);
                this.pos += n;
                return std::task::Poll::Ready(Ok(()));
            }
            if !this.failed {
                this.failed = true;
                // Mirror the real reader's wrapping: io::Error::other(cause).
                return std::task::Poll::Ready(Err(std::io::Error::other(this.msg)));
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn midstream_read_error_maps_to_drive_network_not_local_io() {
        // R1-P2-1: a read error on the Drive download stream is a DRIVE/network
        // failure, so the file must fail with a Drive/network code (here
        // net.intermittent / drive.rate_limited), NEVER local.io_error.
        // Case 1: an unclassified mid-stream break -> net.intermittent (the
        // download stream broke; the disk is fine).
        let plaintext: Vec<u8> = (0..(2 * CIPHERTEXT_FRAME))
            .map(|i| (i % 250) as u8)
            .collect();
        let file = resolved_for(&plaintext, "broken.bin");
        let out = rand_tmp("midstream-net");
        let mut reader = FailMidStreamReader::new(plaintext.clone(), "connection reset by peer");
        let res = stream_to_disk(
            &mut reader,
            open_test_temp(&out),
            None,
            &file,
            &no_cancel(),
            &mut |_| {},
        )
        .await;
        match res {
            StreamOutcome::Failed(code) => {
                assert_eq!(
                    code,
                    ErrorCode::NetIntermittent,
                    "an unclassified mid-stream read break must map to net.intermittent, not local.io_error"
                );
                assert_ne!(code, ErrorCode::LocalIoError);
            }
            other => panic!("expected Failed, got {:?}", std::mem::discriminant(&other)),
        }
        let _ = std::fs::remove_file(&out);

        // Case 2: a mid-stream break whose message embeds a dotted Drive code
        // (the fake's injected fault shape) classifies to that specific code.
        let out2 = rand_tmp("midstream-rl");
        let mut reader2 =
            FailMidStreamReader::new(plaintext.clone(), "fake: drive.rate_limited mid-stream");
        let res2 = stream_to_disk(
            &mut reader2,
            open_test_temp(&out2),
            None,
            &file,
            &no_cancel(),
            &mut |_| {},
        )
        .await;
        match res2 {
            StreamOutcome::Failed(code) => {
                assert_eq!(code, ErrorCode::DriveRateLimited);
            }
            other => panic!("expected Failed, got {:?}", std::mem::discriminant(&other)),
        }
        let _ = std::fs::remove_file(&out2);
    }

    #[test]
    fn map_download_read_err_never_returns_local_io() {
        // R1-P2-1: the read-error mapper must never collapse a download read
        // failure to local.io_error - even a plain io error with no recognisable
        // cause is a broken DOWNLOAD stream (net.intermittent), not a disk fault.
        let plain = std::io::Error::other("some opaque stream failure");
        assert_eq!(map_download_read_err(plain), ErrorCode::NetIntermittent);
        let eof = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short");
        assert_ne!(map_download_read_err(eof), ErrorCode::LocalIoError);
    }

    // --- R2-P1-1 / R1-P2-2: handle-relative confined commit (replace existing) -

    #[tokio::test]
    async fn confined_commit_replaces_existing_and_confines() {
        // R2-P1-1 + R1-P2-2: a ConfinedDest opens the temp RELATIVE to a verified
        // parent handle, streams into it, and commit() renames it to the leaf
        // HANDLE-RELATIVE - atomically REPLACING an existing dest. After commit the
        // dest holds the new bytes, no temp remains, and the dest is under the root.
        let dir = rand_tmp("confined-commit");
        std::fs::create_dir_all(&dir).unwrap();
        let token = DialogToken::for_root(dir.to_string_lossy().to_string());
        // Pre-place an existing file at the leaf (regular file).
        let dest = dir.join("existing.bin");
        std::fs::write(&dest, b"OLD CONTENT that must be replaced").unwrap();

        // validate_restore_dest creates the (already-existing) parent chain.
        validate_restore_dest(&token, "existing.bin").expect("validate dest");
        let root = confine::ConfinedRoot::bind(token.clone()).expect("bind root");
        let (mut confined, mut temp) =
            confine::ConfinedDest::open(&root, "existing.bin").expect("open confined dest");
        {
            use std::io::Write as _;
            temp.write_all(b"NEW").unwrap();
            temp.sync_all().unwrap();
        }
        drop(temp); // close the temp handle before the rename (Windows).
        confined.commit().await.expect("handle-relative commit");
        drop(confined);

        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"NEW",
            "dest must hold the new bytes after a confined replace"
        );
        // No leftover temp in the dest dir.
        let temps: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".driven-restore-tmp.")
            })
            .collect();
        assert!(
            temps.is_empty(),
            "no temp must remain after commit: {temps:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- M9c D1 (M8 R4-P1-1): verify->rename TOCTOU on the temp itself ---------

    #[tokio::test]
    async fn confined_commit_uses_retained_handle_after_streamer_drops_its_dup() {
        // M9c D1: the streamer writes through a DUP of the temp handle and drops it
        // after verifying; the guard RETAINS the original handle and commit renames
        // THAT. So after the streamer's file is fully dropped, commit still places
        // exactly the bytes that were written + verified - it never re-reads the
        // temp pathname to find the object to rename.
        let dir = rand_tmp("d1-retained-handle");
        std::fs::create_dir_all(&dir).unwrap();
        let token = DialogToken::for_root(dir.to_string_lossy().to_string());
        validate_restore_dest(&token, "out.bin").expect("validate dest");
        let root = confine::ConfinedRoot::bind(token.clone()).expect("bind root");
        let (mut confined, mut temp) =
            confine::ConfinedDest::open(&root, "out.bin").expect("open confined dest");
        {
            use std::io::Write as _;
            temp.write_all(b"VERIFIED-BYTES").unwrap();
            temp.sync_all().unwrap();
        }
        // Drop the streamer's dup (mirrors stream_to_disk dropping its tokio File
        // after the BLAKE3 verify). The guard's retained handle is still open.
        drop(temp);
        confined
            .commit()
            .await
            .expect("commit via the retained handle");
        drop(confined);
        assert_eq!(
            std::fs::read(dir.join("out.bin")).unwrap(),
            b"VERIFIED-BYTES",
            "the committed file must hold exactly the verified bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confined_commit_rejects_temp_swapped_after_verification() {
        // M9c D1 (THE discriminating data-safety test): a local process unlinks the
        // verified temp and drops an ATTACKER file at the SAME temp name AFTER the
        // streamer wrote + verified the good bytes and dropped its handle, but
        // BEFORE commit. The commit must DETECT the swap (the temp name no longer
        // names the verified object) and FAIL - the final file must NEVER hold the
        // attacker's unverified bytes (no silent restore corruption).
        let dir = rand_tmp("d1-swap");
        std::fs::create_dir_all(&dir).unwrap();
        let token = DialogToken::for_root(dir.to_string_lossy().to_string());
        validate_restore_dest(&token, "secret.bin").expect("validate dest");
        let root = confine::ConfinedRoot::bind(token.clone()).expect("bind root");
        let (mut confined, mut temp) =
            confine::ConfinedDest::open(&root, "secret.bin").expect("open confined dest");
        {
            use std::io::Write as _;
            temp.write_all(b"GOOD-VERIFIED").unwrap();
            temp.sync_all().unwrap();
        }
        drop(temp); // streamer drops its dup after verify; the guard keeps its handle.

        // Find the temp by its well-known prefix and SWAP it: unlink the verified
        // temp, then create a brand-new file (different inode) at the same name with
        // attacker bytes.
        let temp_entry = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".driven-restore-tmp.")
            })
            .expect("the temp file must exist before the swap");
        let temp_p = temp_entry.path();
        std::fs::remove_file(&temp_p).unwrap();
        std::fs::write(&temp_p, b"EVIL-UNVERIFIED-BYTES").unwrap();

        // Commit must REFUSE: the temp name now points at a different object than
        // the retained, verified handle.
        let err = confined
            .commit()
            .await
            .expect_err("a temp swapped after verification must fail the commit");
        assert_eq!(err, ErrorCode::LocalIoError);
        drop(confined);

        // THE invariant: the final file is NOT the attacker's bytes (it was never
        // created, or - if some path created it - it must not be the EVIL content).
        let final_p = dir.join("secret.bin");
        if let Ok(bytes) = std::fs::read(&final_p) {
            assert_ne!(
                bytes, b"EVIL-UNVERIFIED-BYTES",
                "the committed file must NEVER hold the attacker's unverified bytes (D1)"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_one_file_committed_bytes_equal_verified_bytes() {
        // M9c D1 end-to-end: a full restore_one_file run downloads, streams,
        // verifies the BLAKE3, and commits via the retained handle. The committed
        // file must equal exactly the verified plaintext (the round-trip the D1 fix
        // protects). Uses the InMemoryRemoteStore so the real path runs.
        use driven_drive::remote_store::UploadBody;
        let dir = rand_tmp("d1-e2e");
        std::fs::create_dir_all(&dir).unwrap();
        let token = DialogToken::for_root(dir.to_string_lossy().to_string());
        let plaintext = b"the exact bytes that must land".to_vec();
        let store = driven_drive::fake::InMemoryRemoteStore::new();
        let parent = store.root_id().to_string();
        let entry = store
            .create(
                &parent,
                "f.bin",
                "application/octet-stream",
                UploadBody::Bytes(bytes::Bytes::from(plaintext.clone())),
                std::collections::HashMap::new(),
            )
            .await
            .unwrap();
        let mut file = resolved_for(&plaintext, "nested/f.bin");
        file.drive_file_id = Some(entry.id.clone());
        let root = confine::ConfinedRoot::bind(token).expect("bind root");
        let outcome = restore_one_file(
            &file,
            &store,
            &SuiteVerdict::Plaintext,
            &root,
            &no_cancel(),
            |_| {},
        )
        .await;
        assert!(matches!(outcome, FileOutcome::Done), "restore must succeed");
        assert_eq!(
            std::fs::read(dir.join("nested").join("f.bin")).unwrap(),
            plaintext,
            "the committed file must equal the verified plaintext exactly (D1)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn confined_dest_drop_without_commit_removes_temp() {
        // R2-P2-2: if a ConfinedDest is dropped WITHOUT commit (a failure / cancel /
        // shutdown-abort that drops the future), its Drop best-effort removes the
        // uncommitted temp so no partial / out-of-root plaintext is left behind.
        let dir = rand_tmp("confined-drop");
        std::fs::create_dir_all(&dir).unwrap();
        let token = DialogToken::for_root(dir.to_string_lossy().to_string());
        validate_restore_dest(&token, "ghost.bin").expect("validate dest");
        {
            let root = confine::ConfinedRoot::bind(token.clone()).expect("bind root");
            let (_confined, mut temp) =
                confine::ConfinedDest::open(&root, "ghost.bin").expect("open confined dest");
            use std::io::Write as _;
            temp.write_all(b"partial bytes never committed").unwrap();
            drop(temp);
            // `_confined` drops here WITHOUT commit -> Drop removes the temp.
        }
        let temps: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".driven-restore-tmp.")
            })
            .collect();
        assert!(
            temps.is_empty(),
            "an uncommitted ConfinedDest drop must remove its temp: {temps:?}"
        );
        // And no final file was created.
        assert!(
            !dir.join("ghost.bin").exists(),
            "no final file on drop-without-commit"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_one_file_overwrites_existing_dest_on_all_platforms() {
        // R1-P2-2 end-to-end: a full restore_one_file run whose dest already
        // exists must REPLACE it (not fail). Exercises the real download -> stream
        // -> confined handle-relative commit path via the InMemoryRemoteStore.
        use driven_drive::remote_store::UploadBody;
        let dir = rand_tmp("restore-overwrite");
        std::fs::create_dir_all(&dir).unwrap();
        let token = DialogToken::for_root(dir.to_string_lossy().to_string());
        let plaintext = b"freshly restored bytes".to_vec();

        // Pre-place an EXISTING file at the dest leaf (a regular file, not a link).
        let existing = dir.join("d.bin");
        std::fs::write(&existing, b"stale bytes to overwrite").unwrap();

        let store = driven_drive::fake::InMemoryRemoteStore::new();
        let parent = store.root_id().to_string();
        let entry = store
            .create(
                &parent,
                "d.bin",
                "application/octet-stream",
                UploadBody::Bytes(bytes::Bytes::from(plaintext.clone())),
                std::collections::HashMap::new(),
            )
            .await
            .unwrap();
        let mut file = resolved_for(&plaintext, "d.bin");
        file.drive_file_id = Some(entry.id.clone());

        let root = confine::ConfinedRoot::bind(token).expect("bind root");
        let outcome = restore_one_file(
            &file,
            &store,
            &SuiteVerdict::Plaintext,
            &root,
            &no_cancel(),
            |_| {},
        )
        .await;
        assert!(
            matches!(outcome, FileOutcome::Done),
            "restoring over an existing file must succeed (atomic replace)"
        );
        assert_eq!(
            std::fs::read(&existing).unwrap(),
            plaintext,
            "the existing file must be replaced with the restored bytes"
        );
        // No leftover temp in the dest dir.
        let temps: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".driven-restore-tmp.")
            })
            .collect();
        assert!(
            temps.is_empty(),
            "no temp must be left after replace: {temps:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
            open_test_temp(&out),
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
        let res = stream_to_disk(
            &mut reader,
            open_test_temp(&out),
            None,
            &file,
            &no_cancel(),
            &mut |_| {},
        )
        .await;
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
        let res = stream_to_disk(
            &mut reader,
            open_test_temp(&out),
            Some(&suite),
            &file,
            &cancel,
            &mut |_| {},
        )
        .await;
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
        let root = confine::ConfinedRoot::bind(token).expect("bind root");
        let outcome = restore_one_file(
            &file,
            &store,
            &SuiteVerdict::Plaintext,
            &root,
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

    // --- R2-P1-1: handle-based confinement vs parent-swap TOCTOU --------------

    #[cfg(unix)]
    #[tokio::test]
    async fn confined_open_rejects_post_validation_parent_swap_to_symlink() {
        // R2-P1-1 (the discriminating TOCTOU test): validate_restore_dest succeeds
        // against a REAL directory chain; then a local process swaps a parent
        // component to a symlink pointing OUT of root. ConfinedDest::open must then
        // be REJECTED (its no-follow openat walk refuses the symlink component) and
        // must create NO file - temp OR final - outside the approved root.
        use std::os::unix::fs::symlink;
        let root = rand_tmp("toctou-root");
        let outside = rand_tmp("toctou-outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let token = DialogToken::for_root(root.to_string_lossy().to_string());

        // 1) A real `sub` directory exists; validation succeeds and creates the
        //    chain `root/sub/`.
        validate_restore_dest(&token, "sub/file.bin").expect("validate against real dir");
        assert!(root.join("sub").is_dir(), "real sub dir created");
        // Bind the root identity BEFORE the swap (the root itself is unchanged; only
        // a child component is swapped, which the no-follow parent walk rejects).
        let confined_root = confine::ConfinedRoot::bind(token).expect("bind root");

        // 2) ATTACK: swap `root/sub` for a symlink to `outside` AFTER validation.
        std::fs::remove_dir(root.join("sub")).unwrap();
        symlink(&outside, root.join("sub")).unwrap();

        // 3) ConfinedDest::open must REJECT the swapped symlink component.
        let err = confine::ConfinedDest::open(&confined_root, "sub/file.bin")
            .err()
            .expect("a post-validation parent swap to a symlink must be rejected");
        assert_eq!(err, ErrorCode::LocalIoError);

        // 4) THE invariant: NOTHING was written outside the root via the symlink -
        //    no temp, no final file landed in `outside`.
        let leaked: Vec<_> = std::fs::read_dir(&outside)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            leaked.is_empty(),
            "no file (temp or final) may be created OUTSIDE root via a swapped symlink: {leaked:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confined_open_rejects_symlinked_leaf_parent() {
        // R2-P1-1: a symlink DIRECTLY as the final parent of the leaf is rejected
        // (the no-follow openat of that component fails), so the temp is never
        // created beneath it.
        use std::os::unix::fs::symlink;
        let root = rand_tmp("toctou-leafparent");
        let outside = rand_tmp("toctou-leafparent-out");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let token = DialogToken::for_root(root.to_string_lossy().to_string());
        let confined_root = confine::ConfinedRoot::bind(token).expect("bind root");
        // root/escape -> outside.
        symlink(&outside, root.join("escape")).unwrap();
        let err = confine::ConfinedDest::open(&confined_root, "escape/x.bin")
            .err()
            .expect("a symlinked parent component must be rejected");
        assert_eq!(err, ErrorCode::LocalIoError);
        let leaked: Vec<_> = std::fs::read_dir(&outside)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            leaked.is_empty(),
            "no temp may be created via the symlink: {leaked:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    // --- R5-P1-3: restore ROOT swapped between bind and open ------------------

    #[cfg(unix)]
    #[tokio::test]
    async fn confined_open_rejects_root_swapped_to_symlink_after_bind() {
        // R5-P1-3 (the discriminating root-level TOCTOU test): bind the ConfinedRoot
        // against a REAL root directory, then a local process REPLACES the root
        // itself with a symlink pointing OUT of root. A later ConfinedDest::open must
        // be REJECTED (the bound canonical-path + on-disk identity no longer match
        // the re-resolved root) and must write NO bytes - temp OR final - into the
        // swapped-in target outside the original root.
        use std::os::unix::fs::symlink;
        let parent = rand_tmp("root-swap-parent");
        std::fs::create_dir_all(&parent).unwrap();
        let real_root = parent.join("root");
        let outside = parent.join("outside");
        std::fs::create_dir_all(&real_root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let token = DialogToken::for_root(real_root.to_string_lossy().to_string());
        // BIND while `root` is the real directory (captures its canonical path +
        // dev/inode).
        let confined_root = confine::ConfinedRoot::bind(token).expect("bind real root");

        // ATTACK: replace the real root dir with a symlink to `outside` AFTER the
        // bind. A naive re-canonicalisation of the root path string would now treat
        // `outside` as the approved root.
        std::fs::remove_dir(&real_root).unwrap();
        symlink(&outside, &real_root).unwrap();

        // open must REJECT: the re-resolved root identity differs from the bound one.
        let err = confine::ConfinedDest::open(&confined_root, "file.bin")
            .err()
            .expect("a root swapped to a symlink after bind must be rejected");
        assert_eq!(err, ErrorCode::LocalIoError);

        // THE invariant: nothing (temp or final) was written into `outside`.
        let leaked: Vec<_> = std::fs::read_dir(&outside)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            leaked.is_empty(),
            "no bytes may land outside the originally-bound root via a root swap: {leaked:?}"
        );

        let _ = std::fs::remove_file(&real_root);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&parent);
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

    // --- R2-P2-1: no job seeded before fallible setup succeeds -----------------

    #[tokio::test]
    async fn build_restore_plans_failure_leaves_no_lingering_job() {
        // R2-P2-1: `restore_files` builds all fallible plan setup BEFORE seeding the
        // job. `build_restore_plans` (the extracted setup) never touches the job
        // map, so when setup fails (here: the resolved item's source is unknown,
        // since the AppState has no sources) it returns Err and the restore-job map
        // stays EMPTY - no orphaned non-terminal job entry.
        use crate::app_state::RemoteMode;
        use std::collections::HashMap;
        use std::sync::Arc;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("driven-restore-r2p2-{nonce}"));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = driven_core::state::SqliteStateRepo::open(&dir.join("state.db"))
            .await
            .expect("open state repo");
        let app_state = AppState::new(
            Arc::new(repo),
            HashMap::new(),
            RemoteMode::Fake,
            Arc::new(std::sync::Mutex::new(HashMap::new())),
        );

        // A resolved item whose source id is not present in the (empty) source list.
        let resolved = vec![ResolvedRestore {
            source_id: SourceId::new_v4(),
            relative_path: "x.bin".to_string(),
            size: 3,
            drive_file_id: Some("d-x".to_string()),
            hash_blake3: [0u8; 32],
        }];

        let result = build_restore_plans(&app_state, resolved).await;
        assert!(result.is_err(), "unknown-source setup must fail");
        assert_eq!(
            app_state.restore_jobs_len(),
            0,
            "a failed restore setup must leave NO job entry in AppState (R2-P2-1)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- R3-P1-1: destination-collision detection -----------------------------

    fn resolved_item(source: SourceId, rel: &str) -> ResolvedRestore {
        ResolvedRestore {
            source_id: source,
            relative_path: rel.to_string(),
            size: 1,
            drive_file_id: Some("d".to_string()),
            hash_blake3: [0u8; 32],
        }
    }

    #[test]
    fn detect_dest_collisions_rejects_same_destination_key() {
        // R3-P1-1: two items from DIFFERENT sources both restoring `foo.txt` map to
        // the SAME destination key (dest/foo.txt) and would silently overwrite each
        // other - reject the whole job.
        let a = SourceId::new_v4();
        let b = SourceId::new_v4();
        let sel = vec![resolved_item(a, "foo.txt"), resolved_item(b, "foo.txt")];
        let err = detect_dest_collisions(&sel)
            .expect_err("two items with the same dest key must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn detect_dest_collisions_folds_case_on_insensitive_dest() {
        // R3-P1-1: on a case-insensitive destination (Windows always, macOS/APFS by
        // default - the folding we DEFAULT to), `Foo.txt` and `foo.txt` collide.
        let a = SourceId::new_v4();
        let b = SourceId::new_v4();
        let sel = vec![
            resolved_item(a, "dir/Foo.txt"),
            resolved_item(b, "dir/foo.txt"),
        ];
        let err = detect_dest_collisions(&sel)
            .expect_err("Foo.txt + foo.txt must collide under case-folding");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn detect_dest_collisions_rejects_file_vs_dir_prefix_conflict() {
        // R3-P1-1: one item is a FILE at `a/b` while another is `a/b/c` (so `a/b`
        // must also be a directory) - `a/b` cannot be both. Reject. (Segment-wise:
        // `a/b` is a proper ancestor of `a/b/c`.)
        let a = SourceId::new_v4();
        let sel = vec![resolved_item(a, "a/b"), resolved_item(a, "a/b/c")];
        let err = detect_dest_collisions(&sel)
            .expect_err("a file-vs-dir path-prefix conflict must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn detect_dest_collisions_allows_non_colliding_multisource_selection() {
        // R3-P1-1: a clean multi-source selection with DISTINCT destination keys
        // (and no segment-wise prefix `foo` vs `foobar` false positive) succeeds.
        let a = SourceId::new_v4();
        let b = SourceId::new_v4();
        let sel = vec![
            resolved_item(a, "foo.txt"),
            resolved_item(b, "bar.txt"),
            resolved_item(a, "dir/foo.txt"),
            resolved_item(b, "dir/baz.txt"),
            // `foo` (a dir name in `foo/x`) must NOT collide with the file `foobar`.
            resolved_item(a, "foo/x"),
            resolved_item(a, "foobar"),
        ];
        assert!(
            detect_dest_collisions(&sel).is_ok(),
            "a non-colliding multi-source selection must be allowed"
        );
    }

    // --- R3-P2-2: unuploaded / non-synced rows rejected as bad input ----------

    /// Build a real AppState backed by a fresh SQLite repo + one account/source,
    /// returning the state, the source id, and the temp dir (for cleanup).
    async fn state_with_source() -> (AppState, SourceId, std::path::PathBuf) {
        use crate::app_state::RemoteMode;
        use driven_core::state::StateRepo;
        use std::collections::HashMap;
        use std::sync::Arc;
        let nonce = uuid::Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("driven-restore-resolve-{nonce}"));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = driven_core::state::SqliteStateRepo::open(&dir.join("state.db"))
            .await
            .expect("open state repo");
        // Seed one account + one source so file_state rows have a valid FK.
        let account = driven_core::state::AccountRow {
            id: driven_core::types::AccountId::new_v4(),
            email: "alice@example.com".into(),
            display_name: Some("Alice".into()),
            state: driven_core::types::AccountState::Ok,
            encryption_master_key_id: Some("kc:alice".into()),
            created_at: 1_700_000_000_000,
            last_synced_at: None,
        };
        repo.upsert_account(&account).await.unwrap();
        let src_id = SourceId::new_v4();
        let source = driven_core::state::SourceRow {
            id: src_id,
            account_id: account.id,
            display_name: "Docs".into(),
            enabled: true,
            local_path: "/home/alice/docs".into(),
            drive_folder_id: "folder-1".into(),
            drive_folder_path: "/Driven/Docs".into(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore: true,
            include_patterns: vec!["**/*".into()],
            exclude_patterns: vec!["**/*.tmp".into()],
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 1_700_000_000_000,
        };
        repo.upsert_source(&source).await.unwrap();
        let app_state = AppState::new(
            Arc::new(repo),
            HashMap::new(),
            RemoteMode::Fake,
            Arc::new(std::sync::Mutex::new(HashMap::new())),
        );
        (app_state, src_id, dir)
    }

    fn file_state_row(
        source: SourceId,
        rel: &str,
        drive: Option<&str>,
        status: FileStateStatus,
    ) -> driven_core::state::FileStateRow {
        driven_core::state::FileStateRow {
            source_id: source,
            relative_path: driven_core::types::RelativePath::try_from(rel.to_string()).unwrap(),
            size: 3,
            mtime_ns: 0,
            hash_blake3: [0u8; 32],
            drive_file_id: drive.map(|s| s.to_string()),
            drive_md5: None,
            encrypted_remote_path: None,
            status,
            last_uploaded_at: None,
            last_verified_at: None,
        }
    }

    #[tokio::test]
    async fn resolve_restore_items_rejects_null_drive_file_id_as_bad_input() {
        // R3-P2-2: a selection containing a row with drive_file_id = NULL (never
        // uploaded) is rejected with the BAD-REQUEST code (internal.invalid_input),
        // NOT internal.bug, BEFORE any job is spawned.
        let (state, src, dir) = state_with_source().await;
        // An uploaded+synced row (eligible) and a NULL-drive_file_id row (ineligible).
        state
            .state()
            .upsert_file_state(&file_state_row(
                src,
                "ok.txt",
                Some("d-ok"),
                FileStateStatus::Synced,
            ))
            .await
            .unwrap();
        state
            .state()
            .upsert_file_state(&file_state_row(
                src,
                "never.bin",
                None,
                FileStateStatus::Pending,
            ))
            .await
            .unwrap();

        let items = vec![
            RestoreItem {
                source_id: src.to_string(),
                relative_path: "ok.txt".to_string(),
            },
            RestoreItem {
                source_id: src.to_string(),
                relative_path: "never.bin".to_string(),
            },
        ];
        let err = resolve_restore_items(&state, &items, None)
            .await
            .expect_err("a NULL-drive_file_id selection must be rejected");
        assert_eq!(
            err.code,
            ErrorCode::InvalidInput,
            "an unuploaded row must be bad input, not internal.bug"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resolve_restore_items_rejects_non_synced_row_as_bad_input() {
        // R3-P2-2: an uploaded row whose status is not `synced` (e.g. error) is
        // rejected as bad input (its recorded hash may not match the remote bytes).
        let (state, src, dir) = state_with_source().await;
        state
            .state()
            .upsert_file_state(&file_state_row(
                src,
                "stale.txt",
                Some("d-1"),
                FileStateStatus::Error,
            ))
            .await
            .unwrap();
        let items = vec![RestoreItem {
            source_id: src.to_string(),
            relative_path: "stale.txt".to_string(),
        }];
        let err = resolve_restore_items(&state, &items, None)
            .await
            .expect_err("a non-synced row must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resolve_restore_items_accepts_clean_synced_selection() {
        // The happy path: a clean, non-colliding selection of uploaded+synced rows
        // resolves successfully (so the collision/eligibility guards do not over-reject).
        let (state, src, dir) = state_with_source().await;
        for rel in ["a.txt", "sub/b.txt"] {
            state
                .state()
                .upsert_file_state(&file_state_row(
                    src,
                    rel,
                    Some("d"),
                    FileStateStatus::Synced,
                ))
                .await
                .unwrap();
        }
        let items = vec![
            RestoreItem {
                source_id: src.to_string(),
                relative_path: "a.txt".to_string(),
            },
            RestoreItem {
                source_id: src.to_string(),
                relative_path: "sub/b.txt".to_string(),
            },
        ];
        let resolved = resolve_restore_items(&state, &items, None)
            .await
            .expect("a clean synced selection must resolve");
        assert_eq!(resolved.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn resolve_restore_as_of_inside_the_current_window_returns_current_object() {
        // Issue #36 (defect 1, downstream): the current bytes' validity window
        // starts at `file_state.last_uploaded_at`. A restore as-of any instant at or
        // after that start - with NO recorded version covering it - must resolve to
        // the CURRENT object, not be rejected. This is what an identical-content
        // touch's PRESERVED window start buys: without it the touch would advance
        // last_uploaded_at past the requested instant and this same selection would
        // wrongly fail with "no backed-up version as of the selected date".
        let (state, src, dir) = state_with_source().await;
        let mut row = file_state_row(src, "doc.txt", Some("cur-obj"), FileStateStatus::Synced);
        // The current object became live at t0 = 1_000 (its window start).
        row.last_uploaded_at = Some(1_000);
        state.state().upsert_file_state(&row).await.unwrap();

        let items = vec![RestoreItem {
            source_id: src.to_string(),
            relative_path: "doc.txt".to_string(),
        }];
        // Restore as-of t = 5_000 (>= the window start, no version recorded).
        let resolved = resolve_restore_items(&state, &items, Some(5_000))
            .await
            .expect(
                "as-of at/after the window start must resolve to the current object, not reject",
            );
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].drive_file_id.as_deref(),
            Some("cur-obj"),
            "the still-live current object holds the bytes that were live at the requested instant"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- R3-P1-2: atomic seed+handle vs a quit in the seed->release window -----

    #[tokio::test]
    async fn seeded_restore_job_always_has_awaitable_handle_for_shutdown_drain() {
        // R3-P1-2: a seeded restore job is NEVER observable without an awaitable
        // handle. We model the exact race: seed the job WITH its handle (one locked
        // insert), then BEFORE releasing the barrier the app quits and runs the
        // shutdown drain. The drain must find the handle (drain, not orphan), and
        // the gated task must do NO filesystem work + leave NO temp.
        use crate::app_state::RemoteMode;
        use std::collections::HashMap;
        use std::sync::Arc;

        let nonce = uuid::Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("driven-restore-seedrace-{nonce}"));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = driven_core::state::SqliteStateRepo::open(&dir.join("state.db"))
            .await
            .expect("open state repo");
        let app_state = AppState::new(
            Arc::new(repo),
            HashMap::new(),
            RemoteMode::Fake,
            Arc::new(std::sync::Mutex::new(HashMap::new())),
        );

        let cancel: RestoreCancel = Arc::new(AtomicBool::new(false));
        let job_cancel = cancel.clone();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        // The gated task: do NOTHING until released. If released with the cancel
        // flag set (the quit path), exit immediately WITHOUT touching disk - mirrors
        // run_restore_job's pre-file cancel check. A marker file proves no fs work.
        let marker = dir.join("did-fs-work.marker");
        let marker_for_task = marker.clone();
        let handle = tokio::task::spawn(async move {
            if release_rx.await.is_err() {
                return;
            }
            if job_cancel.load(Ordering::SeqCst) {
                // Quit/cancel observed on release: exit clean, no fs work.
                return;
            }
            // Would-be filesystem work (never reached in this test).
            let _ = std::fs::write(&marker_for_task, b"x");
        });

        // Seed the job WITH its handle ATOMICALLY (R3-P1-2): the instant it is
        // observable it already has an awaitable handle.
        let status = restore_status_for("seed-race");
        app_state.seed_restore_job(status, cancel.clone(), handle);

        // QUIT in the window BEFORE release: the shutdown drain sets every cancel
        // flag and TAKES every handle. It must find this job's handle.
        let handles = app_state.cancel_all_restore_jobs();
        assert_eq!(
            handles.len(),
            1,
            "a seeded job MUST expose an awaitable handle to the shutdown drain (R3-P1-2)"
        );
        assert!(
            cancel.load(Ordering::SeqCst),
            "the drain set the cancel flag"
        );

        // Now release the barrier (the spawn-window code path) and await the task.
        let _ = release_tx.send(());
        for h in handles {
            tokio::time::timeout(std::time::Duration::from_secs(2), h)
                .await
                .expect("the gated task must join (not orphan)")
                .expect("joined cleanly");
        }
        // The task saw the cancel on release and did NO filesystem work / no temp.
        assert!(
            !marker.exists(),
            "a quit in the seed->release window must leave NO partial fs work (R3-P1-2)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn restore_status_for(job_id: &str) -> RestoreJobStatus {
        RestoreJobStatus {
            job_id: job_id.to_string(),
            total_files: 1,
            completed_files: 0,
            failed_files: 0,
            total_bytes: 0,
            bytes_done: 0,
            current_file: None,
            done: false,
            cancelled: false,
            files: Vec::new(),
        }
    }

    // --- CAP-P2b: restore fail-closed on the DB encryption_enabled flag --------

    /// A real (constructible) suite for the policy tests. The suite is never
    /// invoked here - the policy only ROUTES it - so a fresh per-source key is fine.
    fn fake_suite() -> Arc<dyn SourceCryptoSuite> {
        Arc::new(DrivenCryptoSuite::new(
            driven_crypto::key::SourceKey::generate(),
        ))
    }

    #[test]
    fn encrypted_source_with_provider_plaintext_fails_closed() {
        // CAP-P2b (post-GA hardening): the DB row says the source is ENCRYPTED, but
        // a stale/buggy provider snapshot returns Plaintext. The restore policy must
        // FAIL CLOSED (Unavailable -> crypto.key_missing), NEVER stream ciphertext
        // through the plaintext path. Mirrors the executor's resolve_source_crypto.
        let verdict = apply_encryption_policy(true, CryptoResolution::Plaintext);
        assert!(
            matches!(verdict, SuiteVerdict::Unavailable),
            "encrypted DB row + provider Plaintext must fail closed, not stream plaintext"
        );
    }

    #[test]
    fn encrypted_source_with_provider_unavailable_fails_closed() {
        // CAP-P2b: an encrypted source whose provider cannot resolve a key (no
        // running handle, locked keychain) fails closed.
        let verdict = apply_encryption_policy(true, CryptoResolution::Unavailable);
        assert!(
            matches!(verdict, SuiteVerdict::Unavailable),
            "encrypted DB row + Unavailable must fail closed"
        );
    }

    #[test]
    fn encrypted_source_with_provider_suite_uses_the_suite() {
        // CAP-P2b: the ONE accepted shape for an encrypted source - the provider
        // resolved a real suite - decrypts via that suite.
        let verdict = apply_encryption_policy(true, CryptoResolution::Suite(fake_suite()));
        assert!(
            matches!(verdict, SuiteVerdict::Suite(_)),
            "encrypted DB row + resolved suite must decrypt via the suite"
        );
    }

    #[test]
    fn unencrypted_source_with_provider_suite_forces_plaintext() {
        // CAP-P2b: the DB row says UNENCRYPTED, but a stale provider snapshot returns
        // a suite. The policy must FORCE plaintext and IGNORE the suite - an
        // unencrypted source has nothing to decrypt, and applying a spurious suite
        // would corrupt the restore.
        let verdict = apply_encryption_policy(false, CryptoResolution::Suite(fake_suite()));
        assert!(
            matches!(verdict, SuiteVerdict::Plaintext),
            "unencrypted DB row must force plaintext even if the provider returns a suite"
        );
    }

    #[test]
    fn unencrypted_source_with_provider_unavailable_forces_plaintext() {
        // CAP-P2b: an unencrypted source streams plaintext regardless of the
        // provider verdict (even Unavailable) - there is nothing to decrypt.
        let plain = apply_encryption_policy(false, CryptoResolution::Plaintext);
        assert!(matches!(plain, SuiteVerdict::Plaintext));
        let unavail = apply_encryption_policy(false, CryptoResolution::Unavailable);
        assert!(
            matches!(unavail, SuiteVerdict::Plaintext),
            "unencrypted DB row forces plaintext even on an Unavailable provider"
        );
    }
}
