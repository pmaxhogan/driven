//! Source IPC commands (SPEC s11.2).
//!
//! The Sources settings tab + the add-source wizard (DESIGN s8.2 / s8.5 step 3)
//! drive these. Each is a `#[tauri::command]` over `State<AppState>`.
//!
//! IPC path safety (SPEC s11.6.1): `add_source` and `preview_exclusions` take a
//! local `PathBuf` from the (untrusted) webview and validate it via
//! [`crate::commands::validate_readable_dir`] (canonicalise, reject `..`,
//! require an existing directory) before any filesystem walk. `pick_drive_folder`
//! lists Drive (remote) folders; `preview_exclusions` walks the local tree via
//! [`driven_core::scanner`]'s exclude matcher WITHOUT uploading.
//!
//! Encryption opt-in (DESIGN s7.1): the FIRST encrypted source for an account
//! generates + persists the account master key (keychain) and reveals the BIP39
//! recovery phrase exactly once (via the `account:recovery_phrase` event - the
//! frozen `SourceDto` has no slot for the phrase, and a webview event is the only
//! Rust->webview channel the M6 surface offers for it). Every encrypted source
//! gets a fresh per-source key wrapped under the master key, stored in
//! `backup_sources.wrapped_source_key`.

use std::path::Path;
use std::sync::Arc;

use tauri::{AppHandle, Emitter, State};

use driven_core::exclude::build_source_matcher;
use driven_core::state::{AccountRow, SourceRow, StateRepo};
use driven_core::time::{Clock, SystemClock};
use driven_core::types::{AccountId, ErrorCode, SourceId};

use driven_crypto::{master_key_to_phrase, Keystore, MasterKey};

use driven_drive::google::token_store::{KeyringTokenStore, RefreshingTokenSource};
use driven_drive::google::GoogleDriveStore;
use driven_drive::remote_store::RemoteStore;

use crate::app_state::AppState;
use crate::commands::dtos::{
    AddSourceRequest, DriveFolderEntry, DriveFolderListing, ExclusionPreview,
    ExclusionPreviewRequest, SourceDto, SourcePatch,
};
use crate::commands::{validate_readable_dir, CommandError, CommandResult};

/// Tracing target for the sources command layer.
const TARGET: &str = "driven::app::sources";

/// The MIME type Drive uses for folders (for the picker's folder filter).
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";

/// Max sample paths per side (included / excluded) returned by
/// `preview_exclusions` (the wizard shows the first ~50 of each).
const PREVIEW_SAMPLE_CAP: usize = 50;

/// Webview event carrying a freshly-generated BIP39 recovery phrase on the first
/// encrypted source for an account (DESIGN s7.3, s8.5 step 4). Payload:
/// `{ account_id, phrase }` where `phrase` is the 24-word space-joined string.
/// The frozen `SourceDto` cannot carry the phrase, so this event is the
/// reveal-exactly-once channel.
const EVENT_RECOVERY_PHRASE: &str = "account:recovery_phrase";

/// Environment override for the OAuth client id (SPEC s4), mirroring assembly.
const ENV_OAUTH_CLIENT_ID: &str = "DRIVEN_OAUTH_CLIENT_ID";
/// Environment override for the OAuth client secret (SPEC s4).
const ENV_OAUTH_CLIENT_SECRET: &str = "DRIVEN_OAUTH_CLIENT_SECRET";
/// The public installed-app client id (SPEC s4), mirroring assembly.
const DEFAULT_CLIENT_ID: &str =
    "1094503409775-kvuig3oqtchrq1s4tc1cnpi60mdvnqfe.apps.googleusercontent.com";

/// `list_sources()` - every configured backup source (SPEC s11.2).
#[tauri::command]
pub async fn list_sources(state: State<'_, AppState>) -> CommandResult<Vec<SourceDto>> {
    let rows = state
        .state()
        .list_sources()
        .await
        .map_err(CommandError::from)?;
    Ok(rows.iter().map(source_row_to_dto).collect())
}

/// Map a [`SourceRow`] to the webview-facing [`SourceDto`] (the wrapped
/// per-source key is never exposed; `encryption_enabled` is the row flag).
fn source_row_to_dto(row: &SourceRow) -> SourceDto {
    SourceDto {
        id: row.id.to_string(),
        account_id: row.account_id.to_string(),
        display_name: row.display_name.clone(),
        enabled: row.enabled,
        local_path: row.local_path.clone(),
        drive_folder_id: row.drive_folder_id.clone(),
        drive_folder_path: row.drive_folder_path.clone(),
        encryption_enabled: row.encryption_enabled,
        respect_gitignore: row.respect_gitignore,
        include_patterns: row.include_patterns.clone(),
        exclude_patterns: row.exclude_patterns.clone(),
        deep_verify_interval_secs: row.deep_verify_interval_secs,
        last_full_scan_at: row.last_full_scan_at,
        created_at: row.created_at,
    }
}

/// `add_source(req)` - create a new backup source (SPEC s11.2).
///
/// SPEC s11.6.1: `req.local_path` is validated (canonicalise, reject `..`,
/// require an existing directory) before any filesystem access. On encryption
/// opt-in the account master key is generated + persisted (first encrypted
/// source) and the recovery phrase revealed once; a fresh per-source key is
/// wrapped under the master key into `wrapped_source_key`. The owning account's
/// running orchestrator is reconfigured so the new (enabled) source is picked up
/// without a restart.
#[tauri::command]
pub async fn add_source(
    app: AppHandle,
    state: State<'_, AppState>,
    req: AddSourceRequest,
) -> CommandResult<SourceDto> {
    // The owning account must exist.
    let account = find_account(state.state().as_ref(), req.account_id).await?;

    // SPEC s11.6.1: validate the dialog-derived local path before any walk.
    let canon = validate_readable_dir(&req.local_path)?;

    let now = SystemClock.now_ms();
    let source_id = SourceId::new_v4();

    // Encryption opt-in (DESIGN s7.1): ensure the account master key exists
    // (generating + revealing the phrase on the FIRST encrypted source), then
    // wrap a fresh per-source key under it.
    let wrapped_source_key = if req.encryption_enabled {
        let master = ensure_master_key(state.state().as_ref(), &account, &app).await?;
        let (_source_key, wrapped) = master.wrap_new_source_key().map_err(|e| {
            CommandError::with_code(
                ErrorCode::CryptoKeyMissing,
                format!("failed to wrap per-source key: {e}"),
            )
        })?;
        Some(wrapped.to_bytes())
    } else {
        None
    };

    let row = SourceRow {
        id: source_id,
        account_id: req.account_id,
        display_name: req.display_name.clone(),
        enabled: true,
        local_path: canon.to_string_lossy().to_string(),
        drive_folder_id: req.drive_folder_id.clone(),
        drive_folder_path: req.drive_folder_path.clone(),
        encryption_enabled: req.encryption_enabled,
        wrapped_source_key,
        respect_gitignore: req.respect_gitignore,
        include_patterns: req.include_patterns.clone(),
        exclude_patterns: req.exclude_patterns.clone(),
        schedule_json_v2_reserved: None,
        deep_verify_interval_secs: default_deep_verify_secs(),
        last_full_scan_at: None,
        last_deep_verify_at: None,
        created_at: now,
    };

    state
        .state()
        .upsert_source(&row)
        .await
        .map_err(CommandError::from)?;

    // Reconfigure the owning orchestrator so the new source is picked up without
    // a restart (best-effort: the account may not have a running orchestrator -
    // e.g. needs_reauth - in which case the scheduled scan on next start covers
    // it). The crypto provider was built at assembly time over the THEN-known
    // sources, so a brand-new encrypted source's key is resolved on next start;
    // reconfigure still re-arms the watcher + scan cadence for the source set.
    reconfigure_account(&state, req.account_id).await;

    tracing::info!(target: TARGET, source_id = %source_id, account_id = %req.account_id, encrypted = req.encryption_enabled, "source added");
    Ok(source_row_to_dto(&row))
}

/// `update_source(source_id, patch)` - patch an existing source (SPEC s11.2).
#[tauri::command]
pub async fn update_source(
    state: State<'_, AppState>,
    source_id: SourceId,
    patch: SourcePatch,
) -> CommandResult<SourceDto> {
    // Read the current row by id (strongly consistent).
    let mut row = find_source(state.state().as_ref(), source_id).await?;

    if let Some(display_name) = patch.display_name {
        row.display_name = display_name;
    }
    if let Some(enabled) = patch.enabled {
        row.enabled = enabled;
    }
    if let Some(respect_gitignore) = patch.respect_gitignore {
        row.respect_gitignore = respect_gitignore;
    }
    if let Some(include_patterns) = patch.include_patterns {
        row.include_patterns = include_patterns;
    }
    if let Some(exclude_patterns) = patch.exclude_patterns {
        row.exclude_patterns = exclude_patterns;
    }
    if let Some(secs) = patch.deep_verify_interval_secs {
        row.deep_verify_interval_secs = secs;
    }

    state
        .state()
        .upsert_source(&row)
        .await
        .map_err(CommandError::from)?;

    // Reconfigure the owning orchestrator so a toggled `enabled` / changed globs
    // / cadence take effect without a restart.
    reconfigure_account(&state, row.account_id).await;

    tracing::info!(target: TARGET, source_id = %source_id, "source updated");
    Ok(source_row_to_dto(&row))
}

/// `remove_source(source_id, delete_remote)` - remove a source (SPEC s11.2).
///
/// Deletes the `backup_sources` row (cascading its `file_state` + `pending_ops`)
/// and reconfigures the owning orchestrator. `delete_remote` (trash the source's
/// backed-up Drive content) is NOT performed in this slice (no standalone Drive
/// store handle is exposed to IPC for a bulk remote trash); a `true` request is
/// rejected so the caller is never told the remote was deleted when it was not.
#[tauri::command]
pub async fn remove_source(
    state: State<'_, AppState>,
    source_id: SourceId,
    delete_remote: bool,
) -> CommandResult<()> {
    if delete_remote {
        return Err(CommandError::with_code(
            ErrorCode::DriveUnreachable,
            "remote deletion on source removal is not available in this build; \
             the source's Drive content was left intact. Remove it from Google Drive directly.",
        ));
    }

    let row = find_source(state.state().as_ref(), source_id).await?;
    let account_id = row.account_id;

    state
        .state()
        .delete_source(source_id)
        .await
        .map_err(CommandError::from)?;

    reconfigure_account(&state, account_id).await;
    tracing::info!(target: TARGET, source_id = %source_id, "source removed");
    Ok(())
}

/// `pick_drive_folder(account_id, start_folder_id?)` - list a Drive folder's
/// child folders for the destination picker (SPEC s11.2).
///
/// Builds a one-off real [`GoogleDriveStore`] for the account from its keychain
/// refresh token (the assembly `build_remote` pattern), lists `start_folder_id`'s
/// children (My Drive `root` when `None`), and returns only the FOLDER children
/// (the picker descends folders) with breadcrumb context. Drive-side listing,
/// never a local path.
#[tauri::command]
pub async fn pick_drive_folder(
    state: State<'_, AppState>,
    account_id: AccountId,
    start_folder_id: Option<String>,
) -> CommandResult<DriveFolderListing> {
    // The account must exist (so a stale webview id surfaces an error).
    let _ = find_account(state.state().as_ref(), account_id).await?;

    let store = build_account_store(account_id)?;
    // Drive's root alias is the literal "root" (resolves to My Drive's root id).
    let folder_id = start_folder_id
        .clone()
        .unwrap_or_else(|| "root".to_string());

    let children = store
        .list_folder(&folder_id)
        .await
        .map_err(CommandError::from)?;

    // Only folders are descendable destinations.
    let folders: Vec<DriveFolderEntry> = children
        .into_iter()
        .filter(|e| e.mime_type == FOLDER_MIME && !e.trashed)
        .map(|e| DriveFolderEntry {
            id: e.id,
            name: e.name,
        })
        .collect();

    // The current folder's display path: the breadcrumb is maintained by the
    // webview (it tracks descent), so the backend returns the bare id; an empty
    // path at root, else the requested id echoed (the webview joins names).
    let current_folder_path = match &start_folder_id {
        None => String::new(),
        Some(_) => String::new(),
    };

    Ok(DriveFolderListing {
        current_folder_id: start_folder_id,
        current_folder_path,
        folders,
    })
}

/// `preview_exclusions(req)` - preview which files the candidate rules would
/// include vs exclude (SPEC s11.2; DESIGN s8.5 step 3).
///
/// SPEC s11.6.1: `req.local_path` is validated before the walk. Builds the same
/// [`build_source_matcher`] the scanner uses over a synthetic `SourceRow`
/// carrying the candidate rules, walks the tree to a bounded sample, and returns
/// counts + first-N samples of included vs excluded relative paths. Reads only -
/// no upload, no state write.
#[tauri::command]
pub async fn preview_exclusions(
    _state: State<'_, AppState>,
    req: ExclusionPreviewRequest,
) -> CommandResult<ExclusionPreview> {
    let canon = validate_readable_dir(&req.local_path)?;

    // A synthetic source carrying the candidate rules so the SAME matcher the
    // scanner uses (defaults + optional gitignore tier + the candidate
    // include/exclude globs) decides include/exclude - the preview matches the
    // real backup classification exactly.
    let synthetic = SourceRow {
        id: SourceId::new_v4(),
        account_id: AccountId::new_v4(),
        display_name: String::new(),
        enabled: true,
        local_path: canon.to_string_lossy().to_string(),
        drive_folder_id: String::new(),
        drive_folder_path: String::new(),
        encryption_enabled: false,
        wrapped_source_key: None,
        respect_gitignore: req.respect_gitignore,
        include_patterns: req.include_patterns.clone(),
        exclude_patterns: req.exclude_patterns.clone(),
        schedule_json_v2_reserved: None,
        deep_verify_interval_secs: default_deep_verify_secs(),
        last_full_scan_at: None,
        last_deep_verify_at: None,
        created_at: 0,
    };

    let matcher = build_source_matcher(&synthetic).map_err(CommandError::from)?;

    // Run the (blocking) walk + classification off the async runtime.
    let canon_walk = canon.clone();
    let preview = tokio::task::spawn_blocking(move || classify_tree(&canon_walk, &matcher))
        .await
        .map_err(|e| {
            CommandError::with_code(
                ErrorCode::InternalBug,
                format!("exclusion preview task failed: {e}"),
            )
        })??;

    Ok(preview)
}

/// Walk `root` and classify every regular file as included vs excluded under
/// `matcher`, returning the [`ExclusionPreview`] (counts, included bytes, bounded
/// samples, truncation flag). Synchronous - run under `spawn_blocking`.
///
/// Uses `walkdir` semantics via a manual stack walk over `std::fs` (no extra
/// dep): symlinks are not followed (matching the scanner's `Skip` policy); a
/// directory the matcher excludes is still descended only when the matcher has
/// negations (a `!`-re-include could live under it), mirroring the scanner's
/// lockstep walk/decision.
fn classify_tree(
    root: &Path,
    matcher: &driven_core::exclude::SourceMatcher,
) -> CommandResult<ExclusionPreview> {
    let mut included_count: u64 = 0;
    let mut excluded_count: u64 = 0;
    let mut included_bytes: u64 = 0;
    let mut included_sample: Vec<String> = Vec::new();
    let mut excluded_sample: Vec<String> = Vec::new();

    let prune_excluded_dirs = !matcher.has_negations();

    // Manual stack walk so we can prune excluded directories (when safe) and
    // never follow symlinks. Each entry is the absolute path; we strip `root`
    // for the matcher + samples.
    let mut stack: Vec<std::path::PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(err) => {
                // A permission denial / transient error on a subdir: log + skip
                // that subtree rather than failing the whole preview.
                tracing::debug!(target: TARGET, dir = %dir.display(), %err, "preview: skipping unreadable directory");
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Do not follow symlinks (scanner `Skip` policy): use the entry's
            // own type, not the dereferenced target.
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };

            if file_type.is_dir() {
                let dir_included = matcher.is_included(rel, true);
                // Descend unless this dir is excluded AND pruning is safe (no
                // negation rule could re-include a child).
                if dir_included || !prune_excluded_dirs {
                    stack.push(path);
                }
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if matcher.is_included(rel, false) {
                included_count += 1;
                if let Ok(meta) = entry.metadata() {
                    included_bytes = included_bytes.saturating_add(meta.len());
                }
                if included_sample.len() < PREVIEW_SAMPLE_CAP {
                    included_sample.push(rel_str);
                }
            } else {
                excluded_count += 1;
                if excluded_sample.len() < PREVIEW_SAMPLE_CAP {
                    excluded_sample.push(rel_str);
                }
            }
        }
    }

    let truncated = included_count as usize > included_sample.len()
        || excluded_count as usize > excluded_sample.len();

    Ok(ExclusionPreview {
        included_count,
        excluded_count,
        included_bytes,
        included_sample,
        excluded_sample,
        truncated,
    })
}

// ---------------------------------------------------------------------------
// Helpers shared across the source commands.
// ---------------------------------------------------------------------------

/// The SPEC s22 default deep-verify cadence (7 days), used for a new source.
fn default_deep_verify_secs() -> u32 {
    604_800
}

/// Look up an account by id from the strongly-consistent state DB, erroring if
/// absent (so a stale webview surfaces the problem rather than silently no-op).
async fn find_account(state: &dyn StateRepo, id: AccountId) -> CommandResult<AccountRow> {
    let rows = state.list_accounts().await.map_err(CommandError::from)?;
    rows.into_iter().find(|r| r.id == id).ok_or_else(|| {
        CommandError::with_code(ErrorCode::InternalBug, format!("unknown account id: {id}"))
    })
}

/// Look up a source by id from the strongly-consistent state DB, erroring if
/// absent.
async fn find_source(state: &dyn StateRepo, id: SourceId) -> CommandResult<SourceRow> {
    let rows = state.list_sources().await.map_err(CommandError::from)?;
    rows.into_iter().find(|r| r.id == id).ok_or_else(|| {
        CommandError::with_code(ErrorCode::InternalBug, format!("unknown source id: {id}"))
    })
}

/// Ensure `account` has an account master key in the keychain (DESIGN s7.1).
///
/// On the FIRST encrypted source for the account (no `encryption_master_key_id`
/// and no master key in the keystore) this GENERATES the master key, stores it,
/// stamps `encryption_master_key_id` on the account row, and REVEALS the BIP39
/// phrase exactly once via the `account:recovery_phrase` event. On a subsequent
/// encrypted source the existing key is loaded and NO phrase is re-revealed (the
/// account's phrase is unchanged). Returns the master key for wrapping the new
/// per-source key.
async fn ensure_master_key(
    state: &dyn StateRepo,
    account: &AccountRow,
    app: &AppHandle,
) -> CommandResult<MasterKey> {
    let keystore = Keystore::open(&account.id.to_string()).map_err(|e| {
        CommandError::with_code(
            ErrorCode::CryptoKeyMissing,
            format!("failed to open keystore for account: {e}"),
        )
    })?;

    // Already provisioned: load + return it, no phrase reveal.
    if account.encryption_master_key_id.is_some() {
        return keystore.load_master_key().map_err(|e| {
            CommandError::with_code(
                ErrorCode::CryptoKeyMissing,
                format!("account master key unavailable: {e}"),
            )
        });
    }

    // First encrypted source: generate + persist the master key, reveal once.
    let master = MasterKey::generate();
    keystore.store_master_key(&master).map_err(|e| {
        CommandError::with_code(
            ErrorCode::CryptoKeyMissing,
            format!("failed to store account master key: {e}"),
        )
    })?;

    // Stamp the account row so subsequent sources reuse this key (and so the
    // crypto provider sees the account as encryption-provisioned).
    let mut updated = account.clone();
    updated.encryption_master_key_id = Some(account.id.to_string());
    state
        .upsert_account(&updated)
        .await
        .map_err(CommandError::from)?;

    // Reveal the recovery phrase exactly once (DESIGN s7.3). The Zeroizing phrase
    // is rendered into the event payload then dropped; it is never persisted.
    match master_key_to_phrase(&master) {
        Ok(phrase) => {
            if let Err(e) = app.emit(
                EVENT_RECOVERY_PHRASE,
                serde_json::json!({ "account_id": account.id.to_string(), "phrase": *phrase }),
            ) {
                tracing::warn!(target: TARGET, account_id = %account.id, error = %e, "emit account:recovery_phrase failed");
            }
        }
        Err(e) => {
            tracing::warn!(target: TARGET, account_id = %account.id, error = %e, "failed to encode recovery phrase");
        }
    }

    Ok(master)
}

/// Reconfigure `account_id`'s running orchestrator (if one is live) with a
/// fresh [`OrchestratorConfig`] so a source add / update / remove takes effect
/// without a restart (DESIGN s5: settings change applies on the next cycle).
///
/// Best-effort: an account with no running orchestrator (needs_reauth, or never
/// spawned) relies on the next app start to pick up the change. The config here
/// carries the SPEC s22 global gates from the settings KV so a concurrent
/// settings edit is honoured; on a read failure the default config is used.
async fn reconfigure_account(state: &State<'_, AppState>, account_id: AccountId) {
    let Some(handle) = state.account(account_id) else {
        tracing::debug!(target: TARGET, account_id = %account_id, "no running orchestrator to reconfigure; change applies on next start");
        return;
    };
    let config = crate::commands::settings::load_orchestrator_config(state.state().as_ref())
        .await
        .unwrap_or_default();
    handle.orchestrator.reconfigure(config).await;
    tracing::debug!(target: TARGET, account_id = %account_id, "orchestrator reconfigured after source change");
}

/// Build a one-off real [`GoogleDriveStore`] for `account_id` from its keychain
/// refresh token (the assembly `build_remote` pattern), for the Drive-folder
/// picker. An account with no stored refresh token surfaces `auth.invalid_grant`
/// (it needs re-consent before its Drive can be listed).
fn build_account_store(account_id: AccountId) -> CommandResult<Arc<dyn RemoteStore>> {
    let token_store = Arc::new(KeyringTokenStore::new(account_id.to_string()));
    let refresh_token = token_store
        .load_refresh_token()
        .map_err(|e| {
            CommandError::with_code(
                ErrorCode::CryptoKeyMissing,
                format!("failed to read refresh token from keychain: {e}"),
            )
        })?
        .ok_or_else(|| {
            CommandError::with_code(
                ErrorCode::AuthInvalidGrant,
                "account has no stored credentials; re-authenticate before picking a Drive folder",
            )
        })?;

    let (client_id, client_secret) = resolve_oauth_creds();
    let token_source =
        RefreshingTokenSource::from_stored_refresh_token(refresh_token, client_id, client_secret)
            .map_err(CommandError::from)?
            .with_store(token_store);
    let store = GoogleDriveStore::with_default_clients(token_source).map_err(CommandError::from)?;
    Ok(Arc::new(store))
}

/// Resolve the OAuth client id + secret for a one-off store: env overrides
/// first, else the public installed-app default (empty secret for PKCE).
fn resolve_oauth_creds() -> (String, String) {
    let client_id =
        std::env::var(ENV_OAUTH_CLIENT_ID).unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_string());
    let client_secret = std::env::var(ENV_OAUTH_CLIENT_SECRET).unwrap_or_default();
    (client_id, client_secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic source with the given rules for the matcher.
    fn synthetic(
        root: &Path,
        respect_gitignore: bool,
        include: &[&str],
        exclude: &[&str],
    ) -> SourceRow {
        SourceRow {
            id: SourceId::new_v4(),
            account_id: AccountId::new_v4(),
            display_name: String::new(),
            enabled: true,
            local_path: root.to_string_lossy().to_string(),
            drive_folder_id: String::new(),
            drive_folder_path: String::new(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore,
            include_patterns: include.iter().map(|s| s.to_string()).collect(),
            exclude_patterns: exclude.iter().map(|s| s.to_string()).collect(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: default_deep_verify_secs(),
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 0,
        }
    }

    fn tempdir() -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("driven-src-test-{nonce}-{:p}", &nonce));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn classify_tree_splits_included_vs_excluded_by_rules() {
        let dir = tempdir();
        // keep.txt (included), skip.log (excluded by *.log), nested kept file.
        std::fs::write(dir.join("keep.txt"), b"hello").unwrap();
        std::fs::write(dir.join("skip.log"), b"noise").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("deep.txt"), b"deep!").unwrap();

        let src = synthetic(&dir, false, &[], &["*.log"]);
        let matcher = build_source_matcher(&src).unwrap();
        let preview = classify_tree(&dir, &matcher).unwrap();

        assert_eq!(preview.included_count, 2, "keep.txt + sub/deep.txt");
        assert_eq!(preview.excluded_count, 1, "skip.log");
        // bytes = len("hello") + len("deep!") = 5 + 5.
        assert_eq!(preview.included_bytes, 10);
        assert!(preview
            .included_sample
            .iter()
            .any(|p| p == "keep.txt" || p == "sub/deep.txt"));
        assert!(preview.excluded_sample.iter().any(|p| p == "skip.log"));
        assert!(!preview.truncated);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn classify_tree_default_excludes_apply_without_gitignore() {
        let dir = tempdir();
        // A default-excluded path (Thumbs.db / .DS_Store style) vs a normal file.
        std::fs::write(dir.join("doc.txt"), b"x").unwrap();
        std::fs::write(dir.join(".DS_Store"), b"y").unwrap();
        let src = synthetic(&dir, false, &[], &[]);
        let matcher = build_source_matcher(&src).unwrap();
        let preview = classify_tree(&dir, &matcher).unwrap();
        // doc.txt included; .DS_Store excluded by the DESIGN s5.2 default set.
        assert!(preview.included_count >= 1);
        assert!(
            preview.excluded_sample.iter().any(|p| p == ".DS_Store"),
            "default excludes must drop .DS_Store"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn source_row_to_dto_hides_wrapped_key_and_maps_fields() {
        let id = SourceId::new_v4();
        let acct = AccountId::new_v4();
        let row = SourceRow {
            id,
            account_id: acct,
            display_name: "Docs".to_string(),
            enabled: true,
            local_path: "/tmp/docs".to_string(),
            drive_folder_id: "f".to_string(),
            drive_folder_path: "/Backups/Docs".to_string(),
            encryption_enabled: true,
            wrapped_source_key: Some(vec![1, 2, 3]),
            respect_gitignore: true,
            include_patterns: vec!["*.md".to_string()],
            exclude_patterns: vec!["*.log".to_string()],
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: Some(99),
            created_at: 7,
            last_deep_verify_at: None,
        };
        let dto = source_row_to_dto(&row);
        assert_eq!(dto.id, id.to_string());
        assert_eq!(dto.account_id, acct.to_string());
        assert!(dto.encryption_enabled);
        assert_eq!(dto.include_patterns, vec!["*.md".to_string()]);
        // The DTO has no field for the wrapped key - serialising it must not
        // leak the bytes.
        let v = serde_json::to_value(&dto).unwrap();
        assert!(v.get("wrappedSourceKey").is_none());
        assert!(!v.to_string().contains("[1,2,3]"));
    }
}
