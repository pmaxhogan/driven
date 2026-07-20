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
//! generates + persists the account master key (keychain) and returns the BIP39
//! recovery phrase ONCE as a RETURN VALUE on [`AddSourceResult`] (B3 - the phrase
//! is delivered as a one-time value the UI cannot miss, NOT a fire-and-forget
//! event; the UI shows it via RecoveryPhraseReveal and gates Finish on an
//! explicit acknowledgement). Every encrypted source gets a fresh per-source key
//! wrapped under the master key, stored in `backup_sources.wrapped_source_key`.

use std::path::Path;
use std::sync::Arc;

use tauri::State;

use driven_core::exclude::{build_source_matcher, validate_patterns};
use driven_core::state::{AccountRow, PlaceholderPolicy, SourceRow, StateRepo, VersioningConfig};
use driven_core::time::{Clock, SystemClock};
use driven_core::types::{AccountId, ErrorCode, SourceId};

use driven_crypto::{master_key_to_phrase, Keystore, MasterKey};

use driven_drive::google::token_store::{KeyringTokenStore, RefreshingTokenSource};
use driven_drive::google::GoogleDriveStore;
use driven_drive::remote_store::{DriveContext, RemoteStore};
use driven_drive::CustomCaConfig;

use crate::app_state::{AppState, RemoteMode};
use crate::commands::dtos::{
    AddSourceRequest, AddSourceResult, DriveFolderEntry, DriveFolderListing, ExclusionPreview,
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

/// `list_sources()` - every configured backup source (SPEC s11.2).
///
/// R4-P1-2: each DTO is enriched with `pending_recovery_ack` from the durable
/// `recovery_phrase_acks` table so the UI can DISABLE the enable toggle for a
/// first-encrypted source still awaiting its recovery-phrase ack (the backend
/// `update_source` is the real guard, but the UI should not even offer the toggle).
#[tauri::command]
pub async fn list_sources(state: State<'_, AppState>) -> CommandResult<Vec<SourceDto>> {
    let rows = state
        .state()
        .list_sources()
        .await
        .map_err(CommandError::from)?;
    let pending: std::collections::HashSet<SourceId> = state
        .state()
        .list_pending_recovery_acks()
        .await
        .map_err(CommandError::from)?
        .into_iter()
        .map(|r| r.source_id)
        .collect();
    Ok(rows
        .iter()
        .map(|r| source_row_to_dto_with_pending(r, pending.contains(&r.id)))
        .collect())
}

/// Map a [`SourceRow`] to the webview-facing [`SourceDto`] (the wrapped
/// per-source key is never exposed; `encryption_enabled` is the row flag).
/// `pending_recovery_ack` defaults to `false`; use
/// [`source_row_to_dto_with_pending`] when the durable pending-ack state is known.
fn source_row_to_dto(row: &SourceRow) -> SourceDto {
    source_row_to_dto_with_pending(row, false)
}

/// R4-P1-2: map a [`SourceRow`] to a [`SourceDto`], setting `pending_recovery_ack`
/// from the caller's knowledge of the durable `recovery_phrase_acks` table.
fn source_row_to_dto_with_pending(row: &SourceRow, pending_recovery_ack: bool) -> SourceDto {
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
        placeholder_policy: row.placeholder_policy,
        deep_verify_interval_secs: row.deep_verify_interval_secs,
        last_full_scan_at: row.last_full_scan_at,
        created_at: row.created_at,
        pending_recovery_ack,
    }
}

/// `add_source(req)` - create a new backup source (SPEC s11.2).
///
/// SPEC s11.6.1: `req.local_path` is validated (canonicalise, reject `..`,
/// require an existing directory) before any filesystem access. On encryption
/// opt-in the account master key is generated + persisted (first encrypted
/// source) and the recovery phrase is RETURNED on [`AddSourceResult`] once (B3);
/// a fresh per-source key is wrapped under the master key into
/// `wrapped_source_key`. The owning account's running orchestrator is
/// reconfigured (and its crypto provider refreshed, B2) so the new (enabled)
/// source - including an encrypted one - is picked up without a restart.
#[tauri::command]
pub async fn add_source(
    state: State<'_, AppState>,
    req: AddSourceRequest,
) -> CommandResult<AddSourceResult> {
    // The owning account must exist (a stale webview id surfaces an error). The
    // master-key state is re-read INSIDE the per-account lock below (R2-P1-1), so
    // only the existence check matters here.
    let _ = find_account(state.state().as_ref(), req.account_id).await?;

    // C1 (SPEC s11.6.1): resolve the local path from the backend-minted dialog
    // token (single-use) - NOT the webview-supplied `local_path` string. Reject
    // any request whose token does not map to a backend-dialog path.
    let dialog_path = state
        .take_dialog_token(&req.local_path_token)
        .ok_or_else(|| {
            CommandError::with_code(
                ErrorCode::LocalIoError,
                "no matching dialog token for the source folder; pick a folder first",
            )
        })?;

    // Validate the dialog-derived folder before any walk (canonicalise, reject
    // `..`, require an existing directory).
    let canon = validate_readable_dir(&dialog_path)?;

    // R1-P2-2 (DESIGN s5.2.2): reject a new source root that overlaps an
    // EXISTING source root (one is an ancestor of the other, or they are
    // identical). Sibling / disjoint roots are allowed. Checked BEFORE any
    // master-key generation so an overlap never provisions a key.
    reject_overlapping_root(state.state().as_ref(), &canon).await?;

    // R2-P1-3 (DESIGN s5.2): validate the candidate include / exclude globs
    // BEFORE any master-key generation or persistence - max count, max length
    // per pattern, and compile each with the SAME matcher the scanner uses. An
    // invalid / oversized glob is rejected up front (a stable s24 code) rather
    // than slipping into SQLite and breaking the next scan's matcher build.
    validate_source_patterns(&req.include_patterns, &req.exclude_patterns)?;

    // R4-P2-3: validate the renderer-supplied metadata (display name + Drive
    // destination fields) before persisting - non-empty, no control chars,
    // length-capped - so a buggy / hostile renderer cannot write junk into
    // `backup_sources`. Done before any master-key work (no key for a rejected
    // add).
    validate_source_metadata(
        &req.display_name,
        &req.drive_folder_id,
        &req.drive_folder_path,
    )?;
    // Issue #7: validate the optional Shared Drive id the same way as the folder
    // id (bounded, no control/whitespace) so a junk renderer value cannot bloat
    // the row or carry control chars.
    validate_drive_id(req.drive_id.as_deref())?;

    let now = SystemClock.now_ms();
    let source_id = SourceId::new_v4();

    // R2-P1-1: serialise the FIRST-encrypted-source critical section per account.
    // Two concurrent encrypted adds on an account whose
    // `encryption_master_key_id` is still NULL could otherwise BOTH generate
    // DIFFERENT master keys into the same keychain slot and wrap different source
    // keys - leaving one source permanently unrestorable. The per-account async
    // lock (held across the awaited DB insert) makes the second add observe the
    // master key the first installed and wrap under the SAME key. Only encrypted
    // adds take the lock; unencrypted adds stay fully parallel.
    //
    // Defense in depth: the prepare re-reads the account's CURRENT master-key
    // state INSIDE the lock (so a stale pre-lock read cannot cause a re-generate),
    // and the stamp is a COMPARE-AND-SET (stamp only if still NULL).
    let lock = if req.encryption_enabled {
        Some(state.ensure_master_key_lock(req.account_id))
    } else {
        None
    };
    let _guard = match &lock {
        Some(l) => Some(l.lock().await),
        None => None,
    };

    // R6-P1-1 (DATA-SAFETY gate bypass): inside the per-account encrypted lock,
    // REJECT a second encrypted add while ANY recovery-phrase ack is still pending
    // on this account. The first encrypted source stamps the account master key
    // and is held DISABLED + pending-ack; a SECOND encrypted add would see
    // `newly_generated = false` (the key already exists), persist ENABLED, and
    // start encrypted backups BEFORE the user ever revealed/acked the recovery
    // phrase - the backups would be unrestorable on a new machine. The durable
    // `recovery_phrase_acks` table (read under the lock so it is current) is the
    // gate source of truth: while any row exists for this account, no further
    // encrypted source may be added. The user must finish revealing + acking the
    // pending source first (an s24 invalid-input the wizard surfaces). Net
    // invariant: no encrypted source on an account can be enabled / start backups
    // while any recovery-phrase ack is still pending on that account.
    if req.encryption_enabled {
        reject_encrypted_add_while_ack_pending(state.state().as_ref(), req.account_id).await?;
    }

    // Encryption opt-in (DESIGN s7.1): prepare the account master key (generating
    // it + encoding the recovery phrase on the FIRST encrypted source), then wrap
    // a fresh per-source key under it. `recovery_phrase` is Some ONLY when the
    // master key was just generated (B3). `newly_generated_key` records whether
    // THIS call minted + stored a brand-new master key in the keychain, so the
    // atomic DB write below knows it must (a) stamp the account row and (b) on a
    // DB failure ROLL BACK the keychain entry so a retry re-reveals (R1-P1-1).
    let (wrapped_source_key, recovery_phrase, newly_generated_key) = if req.encryption_enabled {
        // R2-P1-1: re-read the account INSIDE the lock so the master-key state is
        // current. A losing-race second add sees the key the first add just
        // stamped and loads it (newly_generated = false) rather than generating a
        // second, divergent key.
        let fresh = find_account(state.state().as_ref(), req.account_id).await?;
        let prepared = prepare_master_key(&fresh)?;
        let (_source_key, wrapped) = prepared.master.wrap_new_source_key().map_err(|e| {
            // The key may have just been stored in the keychain but no DB row
            // exists yet; roll it back so a retry starts clean (R1-P1-1).
            if prepared.newly_generated {
                let _ = delete_master_key(&req.account_id);
            }
            CommandError::with_code(
                ErrorCode::CryptoKeyMissing,
                format!("failed to wrap per-source key: {e}"),
            )
        })?;
        (
            Some(wrapped.to_bytes()),
            prepared.phrase,
            prepared.newly_generated,
        )
    } else {
        (None, None, false)
    };

    // M9c D4 (M6 R4-P1-1, DATA-SAFETY): the FIRST encrypted source (the one that
    // generated the account master key, `newly_generated_key`) is persisted
    // DISABLED and held pending a recovery-phrase ACK. The scheduler + manual sync
    // filter on `enabled`, so a disabled source is NEVER backed up - this closes the
    // window where the app/renderer could die between `add_source` returning the
    // phrase and the user acknowledging it, leaving an ENABLED encrypted source
    // whose backups are unrestorable on a new machine. The source is enabled only
    // by `ack_recovery_phrase_saved`, which itself requires a recorded backend
    // `reveal_recovery_phrase`. A subsequent encrypted source (master key already
    // exists) or an unencrypted source is enabled immediately as before.
    let pending_recovery_ack = newly_generated_key;
    let row = SourceRow {
        id: source_id,
        account_id: req.account_id,
        display_name: req.display_name.clone(),
        enabled: !pending_recovery_ack,
        local_path: canon.to_string_lossy().to_string(),
        drive_folder_id: req.drive_folder_id.clone(),
        // Issue #7: normalise the picker's driveId - My Drive (None / "" /
        // "my-drive") persists as NULL, a Shared Drive as its driveId.
        drive_id: DriveContext::from_stored(req.drive_id.as_deref())
            .drive_id()
            .map(str::to_string),
        drive_folder_path: req.drive_folder_path.clone(),
        encryption_enabled: req.encryption_enabled,
        wrapped_source_key,
        respect_gitignore: req.respect_gitignore,
        include_patterns: req.include_patterns.clone(),
        exclude_patterns: req.exclude_patterns.clone(),
        placeholder_policy: req.placeholder_policy,
        schedule_json_v2_reserved: None,
        deep_verify_interval_secs: default_deep_verify_secs(),
        last_full_scan_at: None,
        last_deep_verify_at: None,
        created_at: now,
    };

    // R1-P1-1 / R2-P1-1 / R4-P1-1: ATOMIC account-stamp + source-insert (+ durable
    // pending-ack record on the first encrypted source). On the FIRST encrypted
    // source the account's `encryption_master_key_id` is stamped IN THE SAME
    // transaction as the source insert (a COMPARE-AND-SET: it only stamps when the
    // column is still NULL, so a concurrent stamp can never be overwritten) AND the
    // DURABLE `recovery_phrase_acks` pending record is written in that same
    // transaction (R4-P1-1) - so a durable encrypted (disabled) source can never
    // exist without its durable pending-ack gate record, EVEN ACROSS A CRASH. The
    // phrase (and the keychain key) is only "kept" once this commits; if it fails
    // AND we just generated a key, delete the keychain entry so the account is left
    // unprovisioned and a retry re-reveals the phrase.
    let insert_result = if pending_recovery_ack {
        // First encrypted source: atomic stamp + (disabled) insert + durable
        // pending-ack record.
        state
            .state()
            .insert_first_encrypted_source_pending_ack(&row, req.account_id.to_string(), now)
            .await
    } else {
        // Unencrypted, or a SUBSEQUENT encrypted source (account already
        // provisioned): no stamp, no pending-ack record.
        state
            .state()
            .insert_source_with_optional_master_key_stamp(&row, None)
            .await
    };
    if let Err(err) = insert_result {
        if newly_generated_key {
            // Roll back the just-stored master key so the account is NOT left
            // provisioned-without-a-revealed-phrase (R1-P1-1).
            if let Err(del) = delete_master_key(&req.account_id) {
                tracing::error!(target: TARGET, account_id = %req.account_id, error = %del, "failed to roll back orphaned master key after atomic source-insert failure");
            } else {
                tracing::warn!(target: TARGET, account_id = %req.account_id, "rolled back master key after atomic source-insert failure; retry will re-reveal the phrase");
            }
        }
        return Err(CommandError::from(err));
    }

    // R2-P1-1: the per-account critical section is complete; release the lock
    // before the (best-effort) reconfigure so a slow reconfigure does not
    // serialise unrelated adds.
    drop(_guard);

    if pending_recovery_ack {
        // M9c D4: the source is DISABLED + awaiting a recovery-phrase ack. Register
        // the pending ack so `reveal_recovery_phrase` / `ack_recovery_phrase_saved`
        // can gate enabling it. Do NOT reconfigure the orchestrator: a disabled
        // source has nothing to sync, and reconfiguring now would refresh the crypto
        // provider for a source that must not be backed up until acknowledged. The
        // ack enables + reconfigures.
        state.register_pending_recovery_ack(source_id, req.account_id);
        tracing::info!(target: TARGET, source_id = %source_id, account_id = %req.account_id, "first encrypted source persisted DISABLED, awaiting recovery-phrase ack (D4)");
    } else {
        // Reconfigure the owning orchestrator so the new source is picked up without
        // a restart (best-effort: the account may not have a running orchestrator -
        // e.g. needs_reauth - in which case the scheduled scan on next start covers
        // it). B2: reconfigure ALSO refreshes the LIVE crypto provider with the
        // current sources, so a brand-new ENCRYPTED source's key resolves on the next
        // tick (it is no longer stranded `Unavailable` until restart).
        reconfigure_account(&state, req.account_id).await;
    }

    tracing::info!(target: TARGET, source_id = %source_id, account_id = %req.account_id, encrypted = req.encryption_enabled, revealed_phrase = recovery_phrase.is_some(), pending_recovery_ack, "source added");
    Ok(AddSourceResult {
        source: source_row_to_dto_with_pending(&row, pending_recovery_ack),
        recovery_phrase,
        pending_recovery_ack,
    })
}

/// `reveal_recovery_phrase(source_id)` - re-derive + return the account's BIP39
/// recovery phrase for a source that is awaiting a recovery-phrase ack (M9c D4,
/// M6 R4-P1-1; DATA-SAFETY).
///
/// This is the BACKEND-verified reveal the ack gate depends on: it is valid ONLY
/// for a source registered as pending-ack (the first encrypted source for its
/// account). It loads the account master key from the keychain and re-encodes the
/// deterministic phrase (`master_key_to_phrase`), RECORDS that a real reveal
/// happened for this source, and returns the 24 words. `ack_recovery_phrase_saved`
/// is rejected unless this has been called, so a user can never acknowledge a
/// phrase the backend never actually revealed. The words are returned once and the
/// caller (the wizard) shows them then drops them; they are never persisted.
#[tauri::command]
pub async fn reveal_recovery_phrase(
    state: State<'_, AppState>,
    source_id: SourceId,
) -> CommandResult<Vec<String>> {
    // R4-P1-1: only a source with a DURABLE pending-ack record may have its phrase
    // revealed here (the durable table is the gate source of truth, so a restart
    // mid-onboarding can still reveal). An unknown / already-acked source is
    // rejected (the phrase is shown ONCE during onboarding; it is never a general
    // "show me my phrase" API). Look the owning account up from the durable record.
    let account_id = find_pending_recovery_ack_account(state.state().as_ref(), source_id)
        .await?
        .ok_or_else(|| {
            CommandError::with_code(
                ErrorCode::InvalidInput,
                "no recovery phrase is pending for this source",
            )
        })?;

    // Re-derive the phrase from the account master key (deterministic). The key was
    // generated + stored by `add_source`; load it and re-encode the phrase.
    let account = find_account(state.state().as_ref(), account_id).await?;
    let prepared = load_master_key_for_reveal(&account)?;
    let phrase = master_key_to_phrase(&prepared).map_err(|e| {
        CommandError::with_code(
            ErrorCode::CryptoKeyMissing,
            format!("failed to encode recovery phrase: {e}"),
        )
    })?;
    let words: Vec<String> = phrase.split_whitespace().map(|w| w.to_string()).collect();

    // R4-P1-1: DURABLY record the backend reveal so the ack can proceed even after
    // a restart. Also mirror it into the in-memory gate.
    let recorded = state
        .state()
        .mark_recovery_phrase_revealed(source_id)
        .await
        .map_err(CommandError::from)?;
    debug_assert!(recorded, "pending-ack source must record a durable reveal");
    let _ = recorded;
    state.record_recovery_reveal(source_id);
    tracing::info!(target: TARGET, source_id = %source_id, account_id = %account_id, "recovery phrase revealed by backend; durably recorded (D4 / R4-P1-1)");
    Ok(words)
}

/// `ack_recovery_phrase_saved(source_id)` - record the user's durable
/// acknowledgement that they saved the recovery phrase, ENABLE the (until-now
/// disabled) first encrypted source, and reconfigure its account so backups can
/// begin (M9c D4, M6 R4-P1-1; DATA-SAFETY).
///
/// The ack is REJECTED unless a real backend `reveal_recovery_phrase` was recorded
/// for this source - a UI checkbox alone can never enable encrypted backups. On a
/// valid ack the source's `enabled` flag is flipped to `true` (the scheduler +
/// manual sync now include it), the pending-ack entry is cleared, and the
/// orchestrator is reconfigured (refreshing the live crypto provider) so the
/// source is picked up without a restart.
#[tauri::command]
pub async fn ack_recovery_phrase_saved(
    state: State<'_, AppState>,
    source_id: SourceId,
) -> CommandResult<SourceDto> {
    // R4-P1-1 DATA-SAFETY gate: the ack is ineffective unless the backend actually
    // revealed the phrase first, read from the DURABLE record (so the gate survives
    // a restart). `None` = no pending ack (unknown / already enabled); `Some(false)`
    // = pending but never revealed (reject); `Some(true)` = ok.
    match state
        .state()
        .recovery_ack_revealed(source_id)
        .await
        .map_err(CommandError::from)?
    {
        None => {
            return Err(CommandError::with_code(
                ErrorCode::InvalidInput,
                "no recovery-phrase acknowledgement is pending for this source",
            ));
        }
        Some(false) => {
            return Err(CommandError::with_code(
                ErrorCode::InvalidInput,
                "cannot acknowledge the recovery phrase before it has been revealed",
            ));
        }
        Some(true) => {}
    }

    // R4-P1-1: ATOMICALLY enable the (until-now disabled) source AND delete its
    // durable pending-ack record in one transaction - a crash can never leave the
    // source enabled with a stale pending record or cleared but still disabled.
    state
        .state()
        .enable_source_and_clear_recovery_ack(source_id)
        .await
        .map_err(CommandError::from)?;

    // Mirror the cleared durable state into the in-memory gate (idempotent).
    state.clear_pending_recovery_ack(source_id);

    // Read the now-enabled row by id (strongly consistent) for the DTO + the
    // owning account for the reconfigure.
    let row = find_source(state.state().as_ref(), source_id).await?;

    // Reconfigure so the now-enabled (encrypted) source is picked up + its key
    // resolves on the next tick (B2), without a restart.
    reconfigure_account(&state, row.account_id).await;

    tracing::info!(target: TARGET, source_id = %source_id, account_id = %row.account_id, "recovery phrase acknowledged; first encrypted source ENABLED + durable ack cleared (D4 / R4-P1-1)");
    Ok(source_row_to_dto(&row))
}

/// M9c D4: load `account`'s master key from the keychain for a recovery-phrase
/// re-reveal. The account must already be provisioned (it generated the key on the
/// first encrypted source add); an unprovisioned account or an unreadable key is a
/// hard error (the phrase cannot be shown).
fn load_master_key_for_reveal(account: &AccountRow) -> CommandResult<MasterKey> {
    if account.encryption_master_key_id.is_none() {
        return Err(CommandError::with_code(
            ErrorCode::CryptoKeyMissing,
            "account has no encryption master key to reveal",
        ));
    }
    let keystore = Keystore::open(&account.id.to_string()).map_err(|e| {
        CommandError::with_code(
            ErrorCode::CryptoKeyMissing,
            format!("failed to open keystore for account: {e}"),
        )
    })?;
    keystore.load_master_key().map_err(|e| {
        CommandError::with_code(
            ErrorCode::CryptoKeyMissing,
            format!("account master key unavailable: {e}"),
        )
    })
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
        // R4-P2-3: validate the patched display name (non-empty, no control
        // chars, length-capped) before it lands in SQLite.
        validate_display_name(&display_name)?;
        row.display_name = display_name;
    }
    if let Some(enabled) = patch.enabled {
        // R4-P1-2 (DATA-SAFETY): REJECT enabling a source that still has a DURABLE
        // pending recovery-phrase ack. `update_source` (and any sync-trigger path
        // that toggles `enabled`) must NOT be a back door around the
        // `ack_recovery_phrase_saved` gate - a disabled first-encrypted source can
        // only be enabled once the durable ack succeeds, so its encrypted backups
        // are never armed before the recovery phrase is durably saveable. The
        // durable record is the source of truth (so this holds across a restart);
        // `ack_recovery_phrase_saved` is the ONLY enable path for a pending source.
        reject_enable_of_pending_encrypted_source(state.state().as_ref(), source_id, enabled)
            .await?;
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
    if let Some(placeholder_policy) = patch.placeholder_policy {
        row.placeholder_policy = placeholder_policy;
    }
    if let Some(secs) = patch.deep_verify_interval_secs {
        // R3-P2-2: a direct IPC patch must not set 0 (constant deep-verify churn)
        // or u32::MAX (suppress deep verify for decades). Validate against the
        // SAME duration cap the global settings validator uses (R2-P2-3) before
        // persisting, returning the stable `internal.invalid_input` s24 code.
        crate::commands::settings::validate_deep_verify_interval(secs)?;
        row.deep_verify_interval_secs = secs;
    }

    // R2-P1-3 (DESIGN s5.2): validate the EFFECTIVE include / exclude globs
    // (after the patch is applied) before persisting, so a later patch cannot
    // sneak an invalid / oversized glob past the add-time check and break the
    // next scan's matcher build.
    validate_source_patterns(&row.include_patterns, &row.exclude_patterns)?;

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
///
/// R5-P1-1 (DATA-SAFETY): a source still holding a DURABLE pending recovery-phrase
/// ack (a first encrypted source the user never saved the phrase for) is removed
/// via the explicit DISCARD transaction, which - when it was the account's only
/// encrypted source - also clears the account master-key stamp + deletes the
/// keychain master key, so a later encrypted source re-provisions and RE-REVEALS a
/// fresh phrase rather than silently reusing an unrecoverable key.
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

    // R5-P1-1 (DATA-SAFETY): if this source still has a DURABLE pending
    // recovery-phrase ack (a first encrypted source the user never acked), a plain
    // `delete_source` would cascade the ack row but LEAVE the account's master-key
    // stamp + the keychain master key. The NEXT encrypted source would then take
    // the "already provisioned" path, return NO recovery phrase, and arm encryption
    // the user can never restore. Route it through the explicit DISCARD transaction
    // instead: it deletes the source (+ ack) AND, when this was the account's only
    // encrypted source, clears the master-key stamp atomically; we then delete the
    // keychain master key so no provisioned-but-phraseless key is left behind. A
    // SUBSEQUENT encrypted source then re-provisions + re-reveals a fresh phrase.
    let pending = state
        .state()
        .recovery_ack_revealed(source_id)
        .await
        .map_err(CommandError::from)?
        .is_some();

    if pending {
        let outcome = state
            .state()
            .discard_pending_encrypted_source(source_id)
            .await
            .map_err(CommandError::from)?;
        // Mirror the cleared durable gate into the in-memory map (idempotent).
        state.clear_pending_recovery_ack(source_id);
        if outcome.master_key_cleared {
            // The account's master-key stamp was cleared in the same transaction;
            // now SAFELY delete the keychain master key (idempotent - NoEntry is
            // Ok). This only runs when no OTHER encrypted source needed it, so a
            // key another source still uses is never deleted.
            if let Err(del) = delete_master_key(&account_id) {
                tracing::error!(target: TARGET, account_id = %account_id, error = %del, "failed to delete orphaned master key after discarding the only pending encrypted source");
            } else {
                tracing::info!(target: TARGET, account_id = %account_id, "deleted account master key after discarding the only pending encrypted source (R5-P1-1)");
            }
        }
        reconfigure_account(&state, account_id).await;
        tracing::info!(target: TARGET, source_id = %source_id, account_id = %account_id, master_key_cleared = outcome.master_key_cleared, "pending encrypted source discarded");
        return Ok(());
    }

    state
        .state()
        .delete_source(source_id)
        .await
        .map_err(CommandError::from)?;

    // Issue #36: clean up the source's `versioning:<id>` settings entry (the
    // per-source config lives in the KV, not a cascaded FK). Best-effort - a
    // stale key is harmless (it just decodes to the default for a now-absent
    // source), so a failure here does not fail the removal.
    if let Err(err) = state
        .state()
        .delete_setting(&VersioningConfig::settings_key(source_id))
        .await
    {
        tracing::warn!(target: TARGET, source_id = %source_id, %err, "failed to clean up versioning config on source removal");
    }

    reconfigure_account(&state, account_id).await;
    tracing::info!(target: TARGET, source_id = %source_id, "source removed");
    Ok(())
}

/// `get_source_versioning(source_id)` - the per-source point-in-time versioning
/// config (issue #36). Absent config decodes to the default (OFF).
#[tauri::command]
pub async fn get_source_versioning(
    state: State<'_, AppState>,
    source_id: SourceId,
) -> CommandResult<VersioningConfig> {
    // Validate the source exists (strongly consistent read by id).
    find_source(state.state().as_ref(), source_id).await?;
    state
        .state()
        .versioning_config(source_id)
        .await
        .map_err(CommandError::from)
}

/// `set_source_versioning(source_id, config)` - enable/configure per-source
/// point-in-time versioning (issue #36). `count_cap` is clamped to `[1, 1000]`
/// (a cap of 0 would prune every version away). Returns the persisted config.
#[tauri::command]
pub async fn set_source_versioning(
    state: State<'_, AppState>,
    source_id: SourceId,
    config: VersioningConfig,
) -> CommandResult<VersioningConfig> {
    find_source(state.state().as_ref(), source_id).await?;
    // Clamp the cap to a sane range so a bad value cannot prune all versions
    // (cap 0) or track an unbounded history.
    let cfg = VersioningConfig {
        enabled: config.enabled,
        count_cap: config.count_cap.clamp(1, 1000),
        max_bytes: config.max_bytes,
    };
    state
        .state()
        .set_versioning_config(source_id, &cfg)
        .await
        .map_err(CommandError::from)?;
    tracing::info!(target: TARGET, source_id = %source_id, enabled = cfg.enabled, count_cap = cfg.count_cap, "source versioning config updated");
    Ok(cfg)
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
    drive_id: Option<String>,
) -> CommandResult<DriveFolderListing> {
    // The account must exist (so a stale webview id surfaces an error).
    let _ = find_account(state.state().as_ref(), account_id).await?;

    // R1-P1-3: honour the run's remote mode. In FAKE mode (dev / e2e /
    // fake-remote wizard acceptance) we MUST NOT build a real Google store or
    // touch real keychain creds - build the in-memory fake and list it. Only in
    // REAL mode do we construct the live GoogleDriveStore from the account's
    // refresh token. The fake's root id is its synthetic root (not the literal
    // "root" alias), so `None` resolves to that; in real mode `None` resolves to
    // Drive's "root" alias (My Drive root).
    // Issue #34: resolve the custom root CA for the one-off picker store's Drive
    // clients (real mode only; the fake path never builds a client).
    let ca = crate::commands::settings::load_custom_ca_config(state.state().as_ref())
        .await
        .unwrap_or_default();
    let (store, default_folder_id) = select_picker_store(state.inner(), account_id, &ca)?;

    // Issue #7: the drive context for this listing. `None`/"my-drive" is My
    // Drive; any other value is the Shared Drive being browsed. The webview
    // carries the driveId back in as it descends a Shared Drive.
    let drive_context = DriveContext::from_stored(drive_id.as_deref());

    // B1: We resolve `None` to the mode-appropriate root for the listing AND echo
    // it back as the `current_folder_id`, so the user can SELECT the current
    // folder - including the root - as the backup destination. Before this fix
    // the backend echoed `None` at the top level, leaving the wizard with no
    // selectable id. For a Shared Drive the driveId doubles as its root folder
    // id, so an unset start folder inside a Shared Drive resolves to the drive
    // root.
    let folder_id = match (&start_folder_id, &drive_context) {
        (Some(id), _) => id.clone(),
        (None, DriveContext::SharedDrive { drive_id }) => drive_id.clone(),
        (None, DriveContext::MyDrive) => default_folder_id,
    };

    let children = store
        .list_folder(&folder_id, &drive_context)
        .await
        .map_err(CommandError::from)?;

    // The persisted driveId to stamp on the listing + every folder entry: a
    // concrete driveId while inside a Shared Drive, `None` for My Drive.
    let listing_drive_id = drive_context.drive_id().map(str::to_string);

    // Only folders are descendable destinations.
    let mut folders: Vec<DriveFolderEntry> = children
        .into_iter()
        .filter(|e| e.mime_type == FOLDER_MIME && !e.trashed)
        .map(|e| DriveFolderEntry {
            id: e.id,
            name: e.name,
            drive_id: listing_drive_id.clone(),
            is_shared_drive: false,
        })
        .collect();

    // Issue #7: at the My Drive root, surface the account's Shared Drive roots
    // above the My Drive folders so the user can descend into one. Each Shared
    // Drive root's id IS its driveId; descending passes that driveId back so the
    // listing switches to `corpora=drive` scope.
    if start_folder_id.is_none() && !drive_context.is_shared_drive() {
        let shared = store
            .list_shared_drives()
            .await
            .map_err(CommandError::from)?;
        let mut shared_entries: Vec<DriveFolderEntry> = shared
            .into_iter()
            .map(|d| DriveFolderEntry {
                drive_id: Some(d.id.clone()),
                id: d.id,
                name: d.name,
                is_shared_drive: true,
            })
            .collect();
        // Shared Drive roots first, then the My Drive folders.
        shared_entries.append(&mut folders);
        folders = shared_entries;
    }

    // The current folder's display path: the breadcrumb is maintained by the
    // webview (it tracks descent), so the backend returns an empty path here
    // (the webview joins the descended folder names). "My Drive" at root.
    let current_folder_path = String::new();

    Ok(DriveFolderListing {
        // B1: always a concrete, selectable id (never `None`).
        current_folder_id: Some(folder_id),
        drive_id: listing_drive_id,
        current_folder_path,
        folders,
    })
}

/// `preview_exclusions(req)` - preview which files the candidate rules would
/// include vs exclude (SPEC s11.2; DESIGN s8.5 step 3).
///
/// R1-P1-2 (SPEC s11.6.1): the walk root is NEVER a raw webview path. For a NEW
/// candidate source the root is resolved from `req.local_path_token` (a
/// backend-minted dialog token, peeked non-consumingly so `add_source` keeps its
/// single use); for an EXISTING source it is resolved from `req.source_id` ->
/// `backup_sources.local_path`. A request with neither - or with a token that
/// does not map to a backend dialog - is rejected. The resolved path is then
/// validated before the walk. Builds the same [`build_source_matcher`] the
/// scanner uses over a synthetic `SourceRow` carrying the candidate rules, walks
/// the tree to a bounded sample, and returns counts + first-N samples of included
/// vs excluded relative paths. Reads only - no upload, no state write.
#[tauri::command]
pub async fn preview_exclusions(
    state: State<'_, AppState>,
    req: ExclusionPreviewRequest,
) -> CommandResult<ExclusionPreview> {
    // R4-P2-1: validate the candidate include / exclude globs BEFORE walking,
    // so an invalid / oversized glob surfaces a stable s24 invalid-input error
    // (the same code add_source / update_source use) instead of a confusing
    // matcher-build failure mid-preview.
    validate_source_patterns(&req.include_patterns, &req.exclude_patterns)?;

    // R1-P1-2 / R4-P2-1: resolve the walk root from a backend-trusted source,
    // never a raw webview path. The request must carry EXACTLY ONE selector - an
    // existing-source id OR a dialog token; both-set / neither-set is rejected
    // (accepting both and silently preferring one hid caller bugs).
    let root =
        match classify_preview_selector(req.source_id.as_deref(), req.local_path_token.as_deref())?
        {
            PreviewRootSelector::Source(source_id) => {
                let row = find_source(state.state().as_ref(), source_id).await?;
                std::path::PathBuf::from(row.local_path)
            }
            PreviewRootSelector::Token(token) => {
                state.peek_dialog_token(token).ok_or_else(|| {
                    CommandError::with_code(
                        ErrorCode::LocalIoError,
                        "no matching dialog token for the preview folder; pick a folder first",
                    )
                })?
            }
        };

    let canon = validate_readable_dir(&root)?;

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
        drive_id: None,
        drive_folder_path: String::new(),
        encryption_enabled: false,
        wrapped_source_key: None,
        respect_gitignore: req.respect_gitignore,
        include_patterns: req.include_patterns.clone(),
        exclude_patterns: req.exclude_patterns.clone(),
        // Placeholder policy does not affect the include/exclude preview; the
        // default is fine for the synthetic matcher.
        placeholder_policy: PlaceholderPolicy::Skip,
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

/// R2-P1-3: validate a source's candidate include / exclude glob patterns,
/// mapping a [`driven_core::exclude::PatternValidationError`] to the stable
/// `internal.invalid_input` SPEC s24 code so the webview shows a "check your
/// input" message (not a "report a bug" one). Shared by `add_source` (the
/// request's patterns) and `update_source` (the post-patch effective patterns).
fn validate_source_patterns(
    include_patterns: &[String],
    exclude_patterns: &[String],
) -> CommandResult<()> {
    validate_patterns(include_patterns, exclude_patterns).map_err(|e| {
        CommandError::with_code(
            ErrorCode::InvalidInput,
            format!("invalid backup rules: {e}"),
        )
    })
}

/// R4-P2-1: the single, validated walk-root selector for `preview_exclusions`.
#[derive(Debug)]
enum PreviewRootSelector<'a> {
    /// An existing source's id (its `local_path` is resolved from SQLite).
    Source(SourceId),
    /// A backend-minted dialog token for a NEW candidate folder.
    Token(&'a str),
}

/// R4-P2-1: validate the `(source_id, local_path_token)` pair for
/// `preview_exclusions` and return the single selector to resolve. The request
/// MUST carry EXACTLY ONE of the two: both-set is rejected (it previously
/// silently preferred `source_id`, hiding caller bugs) and neither-set is
/// rejected. A malformed `source_id` is rejected as invalid input. Pure
/// (no I/O), so the both/neither/malformed branches are unit-testable.
fn classify_preview_selector<'a>(
    source_id: Option<&'a str>,
    local_path_token: Option<&'a str>,
) -> CommandResult<PreviewRootSelector<'a>> {
    match (source_id, local_path_token) {
        (Some(_), Some(_)) => Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "preview takes either a source id OR a dialog token, not both",
        )),
        (Some(source_id_str), None) => {
            let id: SourceId = source_id_str.parse().map_err(|e| {
                CommandError::with_code(
                    ErrorCode::InvalidInput,
                    format!("invalid source id for preview: {e}"),
                )
            })?;
            Ok(PreviewRootSelector::Source(id))
        }
        (None, Some(token)) => Ok(PreviewRootSelector::Token(token)),
        (None, None) => Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "preview requires a dialog token (new folder) or a source id (existing source)",
        )),
    }
}

/// Max chars for a source `display_name` (a short human label, not free text).
const MAX_DISPLAY_NAME_LEN: usize = 256;
/// Max chars for a Drive folder id (a Google Drive id is ~33 chars; cap well
/// above that but bounded so a junk renderer value cannot bloat the row).
const MAX_DRIVE_FOLDER_ID_LEN: usize = 512;
/// Max chars for a Drive folder breadcrumb path.
const MAX_DRIVE_FOLDER_PATH_LEN: usize = 4096;

/// R4-P2-3: validate the renderer-supplied source METADATA before it lands in
/// SQLite. `add_source` trusted `display_name` / `drive_folder_id` /
/// `drive_folder_path` verbatim, so a buggy / hostile renderer could write a
/// control-char-laden or unbounded string into `backup_sources`. This enforces:
/// - `display_name`: non-empty (after trim), no control chars, length-capped.
/// - `drive_folder_id`: non-empty, no control chars / whitespace, length-capped.
/// - `drive_folder_path`: optional (empty = My Drive root), no control chars,
///   length-capped (it is a `/`-joined breadcrumb, NOT a local path - the local
///   path is validated separately via the dialog token).
///
/// All rejections map to the stable s24 `internal.invalid_input` code so the
/// wizard shows a "check your input" message. Shared by `add_source` and the
/// `update_source` display-name patch.
/// Issue #7: validate the optional Shared Drive id carried by `add_source`. A
/// My Drive destination (`None` / "" / "my-drive") is always valid; a Shared
/// Drive id must be a bounded, printable, whitespace-free token (a Google
/// `driveId` is ~19 chars). Rejections map to the stable s24
/// `internal.invalid_input` code.
fn validate_drive_id(drive_id: Option<&str>) -> CommandResult<()> {
    let invalid = |msg: &str| CommandError::with_code(ErrorCode::InvalidInput, msg.to_string());
    // My Drive sentinels need no validation.
    if !DriveContext::from_stored(drive_id).is_shared_drive() {
        return Ok(());
    }
    // Safe to unwrap: `is_shared_drive()` is true only for a concrete id.
    let id = drive_id.unwrap_or("").trim();
    if id.chars().count() > MAX_DRIVE_FOLDER_ID_LEN {
        return Err(invalid("Shared Drive id is too long"));
    }
    if id.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(invalid("Shared Drive id has invalid characters"));
    }
    Ok(())
}

fn validate_source_metadata(
    display_name: &str,
    drive_folder_id: &str,
    drive_folder_path: &str,
) -> CommandResult<()> {
    let invalid = |msg: &str| CommandError::with_code(ErrorCode::InvalidInput, msg.to_string());

    let name = display_name.trim();
    if name.is_empty() {
        return Err(invalid("display name must not be empty"));
    }
    if name.chars().count() > MAX_DISPLAY_NAME_LEN {
        return Err(invalid("display name is too long"));
    }
    if display_name.chars().any(|c| c.is_control()) {
        return Err(invalid("display name must not contain control characters"));
    }

    if drive_folder_id.trim().is_empty() {
        return Err(invalid("a Drive destination folder must be selected"));
    }
    if drive_folder_id.chars().count() > MAX_DRIVE_FOLDER_ID_LEN {
        return Err(invalid("Drive folder id is too long"));
    }
    if drive_folder_id
        .chars()
        .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(invalid("Drive folder id has invalid characters"));
    }

    // The breadcrumb path is optional (empty = My Drive root). When present it
    // must be a printable, length-bounded string (it is display metadata, not a
    // filesystem path).
    if drive_folder_path.chars().count() > MAX_DRIVE_FOLDER_PATH_LEN {
        return Err(invalid("Drive folder path is too long"));
    }
    if drive_folder_path.chars().any(|c| c.is_control()) {
        return Err(invalid(
            "Drive folder path must not contain control characters",
        ));
    }
    Ok(())
}

/// R4-P2-3: validate just a `display_name` (the `update_source` patch path,
/// where the Drive folder fields are not editable). Reuses the metadata rules.
fn validate_display_name(display_name: &str) -> CommandResult<()> {
    let invalid = |msg: &str| CommandError::with_code(ErrorCode::InvalidInput, msg.to_string());
    let name = display_name.trim();
    if name.is_empty() {
        return Err(invalid("display name must not be empty"));
    }
    if name.chars().count() > MAX_DISPLAY_NAME_LEN {
        return Err(invalid("display name is too long"));
    }
    if display_name.chars().any(|c| c.is_control()) {
        return Err(invalid("display name must not contain control characters"));
    }
    Ok(())
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

/// R4-P1-1: the account owning `source`'s DURABLE pending recovery-ack record,
/// if one exists. Reads the `recovery_phrase_acks` table (the gate source of
/// truth) so a reveal works even after a restart mid-onboarding. `None` for a
/// source with no durable pending record (unknown / already acked + enabled).
async fn find_pending_recovery_ack_account(
    state: &dyn StateRepo,
    source: SourceId,
) -> CommandResult<Option<AccountId>> {
    let rows = state
        .list_pending_recovery_acks()
        .await
        .map_err(CommandError::from)?;
    Ok(rows
        .into_iter()
        .find(|r| r.source_id == source)
        .map(|r| r.account_id))
}

/// R6-P1-1 (DATA-SAFETY): reject an encrypted `add_source` on `account` while ANY
/// recovery-phrase ack is still pending for it. The first encrypted source stamps
/// the account master key and is held DISABLED + pending-ack; without this guard a
/// SECOND encrypted add takes the already-provisioned path (`newly_generated =
/// false`), persists ENABLED, and arms encrypted backups before the user ever
/// revealed/acked the recovery phrase - an unrestorable backup. Reading the durable
/// `recovery_phrase_acks` table (the gate source of truth, so this holds across a
/// restart) under the per-account encrypted lock, any pending row for the account
/// rejects the add with the stable s24 `internal.invalid_input` code. The user must
/// finish revealing + acking the pending source first.
async fn reject_encrypted_add_while_ack_pending(
    state: &dyn StateRepo,
    account: AccountId,
) -> CommandResult<()> {
    let any_pending = state
        .list_pending_recovery_acks()
        .await
        .map_err(CommandError::from)?
        .into_iter()
        .any(|r| r.account_id == account);
    if any_pending {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "another encrypted backup on this account is still waiting for its \
             recovery phrase to be revealed and saved; finish that step first, \
             then add this encrypted source",
        ));
    }
    Ok(())
}

/// R4-P1-2 (DATA-SAFETY): reject ENABLING `source` while it still has a DURABLE
/// pending recovery-phrase ack. `enabling == false` (a disable, or no change of
/// state) is always allowed; only flipping a pending source to `enabled=true` is
/// rejected (the `ack_recovery_phrase_saved` flow is the ONLY enable path for a
/// pending source). The durable `recovery_phrase_acks` record is the source of
/// truth, so this holds across a restart. Returns the stable s24
/// `internal.invalid_input` code on rejection.
async fn reject_enable_of_pending_encrypted_source(
    state: &dyn StateRepo,
    source: SourceId,
    enabling: bool,
) -> CommandResult<()> {
    if !enabling {
        return Ok(());
    }
    if state
        .recovery_ack_revealed(source)
        .await
        .map_err(CommandError::from)?
        .is_some()
    {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "this encrypted source cannot be enabled until you reveal and save its \
             recovery phrase; finish that step first",
        ));
    }
    Ok(())
}

/// The prepared account master key for an encrypted source add (R1-P1-1).
struct PreparedMasterKey {
    /// The account master key (loaded or freshly generated).
    master: MasterKey,
    /// The one-time BIP39 recovery phrase words, `Some` ONLY when this call
    /// generated a brand-new master key (B3 - shown once to the user).
    phrase: Option<Vec<String>>,
    /// `true` when THIS call generated + stored a new master key in the
    /// keychain, so the caller knows it must stamp the account row atomically AND
    /// roll the keychain entry back if the DB write fails (R1-P1-1).
    newly_generated: bool,
}

/// Prepare `account`'s master key for an encrypted source (DESIGN s7.1).
///
/// On the FIRST encrypted source (no `encryption_master_key_id`) this GENERATES
/// the master key, stores it in the keychain, encodes the BIP39 recovery phrase,
/// and returns `newly_generated = true` with the phrase words (B3). It does NOT
/// stamp the account row - that stamp now happens ATOMICALLY with the source
/// insert in the caller (R1-P1-1), so a source-insert failure cannot leave the
/// account provisioned-but-phraseless.
///
/// On a SUBSEQUENT encrypted source the existing key is loaded and the returned
/// phrase is `None` (`newly_generated = false`).
///
/// B3 safety: if the master key was just generated but its phrase cannot be
/// encoded, this is a HARD ERROR (`crypto.key_missing`) - an encrypted backup
/// with no revealable phrase is unrestorable; the just-stored key is rolled back
/// (keychain entry deleted) so a retry can regenerate from a clean state.
fn prepare_master_key(account: &AccountRow) -> CommandResult<PreparedMasterKey> {
    let keystore = Keystore::open(&account.id.to_string()).map_err(|e| {
        CommandError::with_code(
            ErrorCode::CryptoKeyMissing,
            format!("failed to open keystore for account: {e}"),
        )
    })?;

    // Already provisioned: load + return it, no phrase (shown once already).
    if account.encryption_master_key_id.is_some() {
        let master = keystore.load_master_key().map_err(|e| {
            CommandError::with_code(
                ErrorCode::CryptoKeyMissing,
                format!("account master key unavailable: {e}"),
            )
        })?;
        return Ok(PreparedMasterKey {
            master,
            phrase: None,
            newly_generated: false,
        });
    }

    // First encrypted source: generate + persist the master key.
    let master = MasterKey::generate();
    keystore.store_master_key(&master).map_err(|e| {
        CommandError::with_code(
            ErrorCode::CryptoKeyMissing,
            format!("failed to store account master key: {e}"),
        )
    })?;

    // B3: encode the recovery phrase BEFORE returning. If it cannot be encoded we
    // must NOT proceed (an encrypted backup with no revealable phrase is
    // unrestorable); roll back the just-stored key and error.
    let phrase = match master_key_to_phrase(&master) {
        Ok(phrase) => phrase,
        Err(e) => {
            let _ = keystore.delete_master_key();
            return Err(CommandError::with_code(
                ErrorCode::CryptoKeyMissing,
                format!("failed to encode recovery phrase; refusing to create an unrestorable encrypted source: {e}"),
            ));
        }
    };
    // Split the space-joined Zeroizing phrase into the 24 words for the UI; the
    // words are returned once then dropped by the caller, never persisted.
    let words: Vec<String> = phrase.split_whitespace().map(|w| w.to_string()).collect();

    Ok(PreparedMasterKey {
        master,
        phrase: Some(words),
        newly_generated: true,
    })
}

/// Delete `account_id`'s master key from the keychain (R1-P1-1 rollback). Used to
/// undo a freshly-generated master key when the atomic source insert fails, so
/// the account is left unprovisioned and a retry re-reveals the phrase.
fn delete_master_key(account_id: &AccountId) -> anyhow::Result<()> {
    let keystore = Keystore::open(&account_id.to_string())?;
    keystore.delete_master_key()?;
    Ok(())
}

/// R1-P2-2 (DESIGN s5.2.2): reject a candidate source root that OVERLAPS any
/// existing source root - i.e. the candidate is an ancestor of, a descendant of,
/// or identical to an existing root. Sibling / disjoint roots are allowed.
/// Applied GLOBALLY across all accounts (DESIGN s5.2.2 does not scope it
/// per-account).
///
/// R4-P2-6 (fail-CLOSED): every `backup_sources.local_path` is already stored in
/// CANONICAL form (`add_source` persists `dunce::canonicalize(dialog_path)`), so
/// the overlap check compares the canonical candidate against the STORED
/// canonical value DIRECTLY - it does NOT re-canonicalise the existing root.
/// Re-canonicalising at check time was fail-OPEN: a temporarily-missing root
/// (the folder moved / a drive briefly unmounted) failed to resolve and was
/// SKIPPED, letting a new source be created that overlaps it; when the root
/// reappeared the two sources were in a nested state. Comparing the stored
/// canonical path needs no filesystem access, so a missing root is still
/// compared (fail-closed: a would-be overlap is rejected even while the existing
/// root is offline). A stored path that is somehow not absolute (legacy / bad
/// data) is treated as un-comparable and, to stay fail-closed, rejects the add
/// with a clear error rather than being silently skipped.
async fn reject_overlapping_root(state: &dyn StateRepo, candidate: &Path) -> CommandResult<()> {
    let existing = state.list_sources().await.map_err(CommandError::from)?;
    for src in &existing {
        // The stored path is canonical from add time; compare it directly (no
        // re-canonicalise, so a temporarily-missing root is still compared).
        let other = Path::new(&src.local_path);
        // Defence-in-depth: a non-absolute stored path cannot be compared by
        // prefix safely. Rather than fail-open (skip), fail-closed: refuse the
        // add so a corrupted row can be investigated instead of silently
        // bypassing the overlap guard.
        if !other.is_absolute() {
            return Err(CommandError::with_code(
                ErrorCode::LocalIoError,
                "an existing backup source has an unexpected (non-absolute) stored \
                 path; cannot safely check for overlap - please remove and re-add it",
            ));
        }
        if candidate == other || candidate.starts_with(other) || other.starts_with(candidate) {
            return Err(CommandError::with_code(
                ErrorCode::LocalIoError,
                "this folder overlaps a folder that is already being backed up \
                 (one is inside the other); pick one or the other, or split them",
            ));
        }
    }
    Ok(())
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

    // B2: REFRESH the live crypto provider with the account's CURRENT sources so
    // a just-added / toggled encrypted source's key resolves on the next tick
    // (the boot snapshot would otherwise fail it closed until restart). Read the
    // current rows for this account from the strongly-consistent state DB.
    match state.state().list_sources().await {
        Ok(rows) => {
            let account_sources: Vec<SourceRow> = rows
                .into_iter()
                .filter(|s| s.account_id == account_id)
                .collect();
            handle.crypto.refresh(account_sources);
            tracing::debug!(target: TARGET, account_id = %account_id, "crypto provider refreshed after source change");
        }
        Err(err) => {
            tracing::warn!(target: TARGET, account_id = %account_id, %err, "failed to read sources to refresh crypto provider; it keeps its prior snapshot");
        }
    }

    let config = crate::commands::settings::load_orchestrator_config(state.state().as_ref())
        .await
        .unwrap_or_default();
    handle.orchestrator.reconfigure(config).await;
    tracing::debug!(target: TARGET, account_id = %account_id, "orchestrator reconfigured after source change");
}

/// Build a [`RemoteStore`] for `account_id` to DOWNLOAD from on restore (M8).
///
/// The mode-aware store is exactly the one the picker / uploader use (via
/// [`select_picker_store`]). In fake mode it is the account's SHARED
/// [`InMemoryRemoteStore`] so restore reads the same objects the orchestrator
/// uploaded; in real mode it is a live `GoogleDriveStore` from the account's
/// keychain refresh token. Restore only needs the store handle (it downloads by
/// the Drive file id carried in `file_state`), so the root id the picker also
/// returns is dropped.
pub(crate) fn build_restore_store(
    state: &AppState,
    account_id: AccountId,
    ca: &CustomCaConfig,
) -> CommandResult<Arc<dyn RemoteStore>> {
    select_picker_store(state, account_id, ca).map(|(store, _root)| store)
}

/// R1-P1-3 / R2-P1-2: select the Drive-folder-picker store + its root id.
///
/// - [`RemoteMode::Fake`] (dev / e2e / fake-remote wizard acceptance): return
///   the account's SHARED [`InMemoryRemoteStore`] from [`AppState`] (R2-P1-2),
///   NOT a throwaway instance - so a folder id this picker mints is visible to
///   the orchestrator's uploader, which holds the SAME shared store. NO real
///   Google store is built and NO keychain creds are read - the fake-remote
///   wizard completes end-to-end without real credentials.
/// - [`RemoteMode::RealGoogleDrive`]: build the live [`GoogleDriveStore`] from
///   the account's keychain refresh token and use Drive's `"root"` alias.
fn select_picker_store(
    state: &AppState,
    account_id: AccountId,
    ca: &CustomCaConfig,
) -> CommandResult<(Arc<dyn RemoteStore>, String)> {
    match state.remote_mode() {
        RemoteMode::Fake => {
            // R2-P1-2: the SAME per-account fake store the orchestrator uploads
            // into (the picker and the uploader must agree on folder ids). The
            // fake never builds an HTTP client, so `ca` is unused here.
            let _ = ca;
            let fake = state.fake_remote_store(account_id);
            let root = fake.root_id().to_string();
            Ok((Arc::new(fake), root))
        }
        RemoteMode::RealGoogleDrive => {
            Ok((build_account_store(account_id, ca)?, "root".to_string()))
        }
    }
}

/// Build a one-off real [`GoogleDriveStore`] for `account_id` from its keychain
/// refresh token (the assembly `build_remote` pattern), for the Drive-folder
/// picker. An account with no stored refresh token surfaces `auth.invalid_grant`
/// (it needs re-consent before its Drive can be listed).
fn build_account_store(
    account_id: AccountId,
    ca: &CustomCaConfig,
) -> CommandResult<Arc<dyn RemoteStore>> {
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

    // A1: use the account's persisted BYO client creds (the client that minted
    // this refresh token), falling back to env/default only if none stored.
    let (client_id, client_secret) = crate::assembly::resolve_account_oauth_creds(account_id);
    let token_source = RefreshingTokenSource::from_stored_refresh_token(
        refresh_token,
        client_id,
        client_secret,
        ca,
    )
    .map_err(CommandError::from)?
    .with_store(token_store);
    let store =
        GoogleDriveStore::with_default_clients(token_source, ca).map_err(CommandError::from)?;
    Ok(Arc::new(store))
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
            drive_id: None,
            drive_folder_path: String::new(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore,
            include_patterns: include.iter().map(|s| s.to_string()).collect(),
            exclude_patterns: exclude.iter().map(|s| s.to_string()).collect(),
            placeholder_policy: Default::default(),
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
            drive_id: None,
            drive_folder_path: "/Backups/Docs".to_string(),
            encryption_enabled: true,
            wrapped_source_key: Some(vec![1, 2, 3]),
            respect_gitignore: true,
            include_patterns: vec!["*.md".to_string()],
            exclude_patterns: vec!["*.log".to_string()],
            placeholder_policy: Default::default(),
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

    /// Open a temp SQLite state repo + return it with the temp dir holding it
    /// alive for the duration of the test.
    async fn temp_repo() -> (
        driven_core::state::sqlite::SqliteStateRepo,
        std::path::PathBuf,
    ) {
        let dir = tempdir();
        let repo = driven_core::state::sqlite::SqliteStateRepo::open(&dir.join("state.db"))
            .await
            .expect("open state repo");
        (repo, dir)
    }

    /// A persisted source row rooted at `root` (an existing dir) for the overlap
    /// tests. Inserts the owning account first so the FK is satisfied.
    async fn persist_source_at(state: &dyn StateRepo, root: &Path) -> AccountId {
        let account_id = AccountId::new_v4();
        let account = AccountRow {
            id: account_id,
            email: "u@example.com".to_string(),
            display_name: None,
            state: driven_core::types::AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        };
        state
            .upsert_account(&account)
            .await
            .expect("upsert account");
        let row = SourceRow {
            id: SourceId::new_v4(),
            account_id,
            display_name: "existing".to_string(),
            enabled: true,
            local_path: root.to_string_lossy().to_string(),
            drive_folder_id: String::new(),
            drive_id: None,
            drive_folder_path: String::new(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore: true,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: default_deep_verify_secs(),
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 0,
        };
        state.upsert_source(&row).await.expect("upsert source");
        account_id
    }

    #[tokio::test]
    async fn reject_overlapping_root_rejects_nested_ancestor_identical_allows_sibling() {
        // R1-P2-2 (DESIGN s5.2.2): nested / ancestor / identical roots are
        // rejected; a sibling is allowed.
        let (repo, dir) = temp_repo().await;
        // Existing source root: <dir>/parent
        let parent = dir.join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let nested = parent.join("child");
        std::fs::create_dir_all(&nested).unwrap();
        let sibling = dir.join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();

        let canon_parent = dunce::canonicalize(&parent).unwrap();
        let canon_nested = dunce::canonicalize(&nested).unwrap();
        let canon_sibling = dunce::canonicalize(&sibling).unwrap();

        persist_source_at(&repo, &canon_parent).await;

        // Identical root -> rejected.
        let err = reject_overlapping_root(&repo, &canon_parent)
            .await
            .expect_err("identical root must be rejected");
        assert_eq!(err.code, ErrorCode::LocalIoError);
        // Descendant (nested under the existing root) -> rejected.
        let err = reject_overlapping_root(&repo, &canon_nested)
            .await
            .expect_err("nested root must be rejected");
        assert_eq!(err.code, ErrorCode::LocalIoError);
        // Ancestor (the existing root is nested under the candidate) -> rejected.
        let canon_dir = dunce::canonicalize(&dir).unwrap();
        let err = reject_overlapping_root(&repo, &canon_dir)
            .await
            .expect_err("ancestor root must be rejected");
        assert_eq!(err.code, ErrorCode::LocalIoError);
        // A sibling (disjoint) -> allowed.
        reject_overlapping_root(&repo, &canon_sibling)
            .await
            .expect("a sibling root must be allowed");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn reject_overlapping_root_fails_closed_when_existing_root_is_missing() {
        // R4-P2-6: the existing source's stored path is canonical from add time.
        // The overlap check compares it DIRECTLY (no re-canonicalise), so a root
        // that no longer exists on disk is STILL compared - a nested candidate is
        // rejected (fail-closed) instead of being silently allowed because the
        // root failed to canonicalise.
        let (repo, dir) = temp_repo().await;
        // Persist an existing source at a canonical absolute path, then DELETE the
        // directory so it cannot canonicalise at check time.
        let parent = dir.join("gone-parent");
        std::fs::create_dir_all(&parent).unwrap();
        let canon_parent = dunce::canonicalize(&parent).unwrap();
        persist_source_at(&repo, &canon_parent).await;
        std::fs::remove_dir_all(&parent).unwrap();

        // A candidate nested under the (now-missing) stored root must be rejected
        // (fail-closed): the path comparison still fires even though the root is
        // gone from disk.
        let nested = canon_parent.join("child");
        let err = reject_overlapping_root(&repo, &nested)
            .await
            .expect_err("a candidate nested under a missing existing root must be rejected");
        assert_eq!(err.code, ErrorCode::LocalIoError);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_source_metadata_enforces_printable_nonempty_bounded() {
        // R4-P2-3: the renderer-supplied source metadata is validated before it
        // lands in SQLite. A valid set passes.
        validate_source_metadata("My Docs", "drive-folder-id", "Backups/Docs")
            .expect("a clean metadata set is accepted");
        // An empty Drive folder PATH is allowed (My Drive root).
        validate_source_metadata("My Docs", "root", "").expect("empty path = My Drive root");

        // Empty / whitespace display name -> rejected.
        assert_eq!(
            validate_source_metadata("   ", "fid", "")
                .expect_err("empty display name")
                .code,
            ErrorCode::InvalidInput
        );
        // Control char in display name -> rejected.
        assert_eq!(
            validate_source_metadata("bad\u{0007}name", "fid", "")
                .expect_err("control char in display name")
                .code,
            ErrorCode::InvalidInput
        );
        // Empty Drive folder id -> rejected.
        assert_eq!(
            validate_source_metadata("ok", "  ", "")
                .expect_err("empty drive folder id")
                .code,
            ErrorCode::InvalidInput
        );
        // Whitespace inside the Drive folder id -> rejected.
        assert_eq!(
            validate_source_metadata("ok", "has space", "")
                .expect_err("whitespace in drive folder id")
                .code,
            ErrorCode::InvalidInput
        );
        // Over-long display name -> rejected.
        let long_name = "x".repeat(MAX_DISPLAY_NAME_LEN + 1);
        assert_eq!(
            validate_source_metadata(&long_name, "fid", "")
                .expect_err("over-long display name")
                .code,
            ErrorCode::InvalidInput
        );
        // Over-long Drive folder path -> rejected.
        let long_path = "a/".repeat(MAX_DRIVE_FOLDER_PATH_LEN);
        assert_eq!(
            validate_source_metadata("ok", "fid", &long_path)
                .expect_err("over-long drive folder path")
                .code,
            ErrorCode::InvalidInput
        );
        // Control char in the Drive folder path -> rejected.
        assert_eq!(
            validate_source_metadata("ok", "fid", "Backups/\u{0000}/Docs")
                .expect_err("control char in drive folder path")
                .code,
            ErrorCode::InvalidInput
        );
    }

    #[test]
    fn classify_preview_selector_requires_exactly_one_of_source_or_token() {
        // R4-P2-1: exactly one selector. Both-set / neither-set / malformed id
        // are rejected; a single valid selector resolves.
        let sid = SourceId::new_v4();
        match classify_preview_selector(Some(&sid.to_string()), None)
            .expect("a lone source id resolves")
        {
            PreviewRootSelector::Source(got) => assert_eq!(got, sid),
            PreviewRootSelector::Token(_) => panic!("expected a source selector"),
        }
        match classify_preview_selector(None, Some("tok")).expect("a lone token resolves") {
            PreviewRootSelector::Token(t) => assert_eq!(t, "tok"),
            PreviewRootSelector::Source(_) => panic!("expected a token selector"),
        }
        // Both set -> rejected.
        assert_eq!(
            classify_preview_selector(Some(&sid.to_string()), Some("tok"))
                .expect_err("both selectors set must be rejected")
                .code,
            ErrorCode::InvalidInput
        );
        // Neither set -> rejected.
        assert_eq!(
            classify_preview_selector(None, None)
                .expect_err("no selector must be rejected")
                .code,
            ErrorCode::InvalidInput
        );
        // Malformed source id -> rejected as invalid input (not an internal bug).
        assert_eq!(
            classify_preview_selector(Some("not-a-uuid"), None)
                .expect_err("a malformed source id must be rejected")
                .code,
            ErrorCode::InvalidInput
        );
    }

    #[test]
    fn validate_display_name_enforces_printable_nonempty_bounded() {
        // R4-P2-3: the update_source display-name patch reuses the same rules.
        validate_display_name("Renamed").expect("a clean display name is accepted");
        assert_eq!(
            validate_display_name("")
                .expect_err("empty display name")
                .code,
            ErrorCode::InvalidInput
        );
        assert_eq!(
            validate_display_name("line\nbreak")
                .expect_err("control char display name")
                .code,
            ErrorCode::InvalidInput
        );
        let long = "y".repeat(MAX_DISPLAY_NAME_LEN + 1);
        assert_eq!(
            validate_display_name(&long)
                .expect_err("over-long display name")
                .code,
            ErrorCode::InvalidInput
        );
    }

    /// Build a Fake-mode [`AppState`] with no running orchestrators, backed by a
    /// temp state repo, for the picker-store tests. Returns the temp dir so the
    /// caller can clean it up.
    async fn fake_app_state() -> (AppState, std::path::PathBuf) {
        use std::collections::HashMap;
        let (repo, dir) = temp_repo().await;
        let state: Arc<dyn StateRepo> = Arc::new(repo);
        let app_state = AppState::new(
            state,
            HashMap::new(),
            RemoteMode::Fake,
            Arc::new(std::sync::Mutex::new(HashMap::new())),
        );
        (app_state, dir)
    }

    #[tokio::test]
    async fn reject_enable_of_pending_encrypted_source_gate() {
        // R4-P1-2 (DATA-SAFETY): enabling a source that still has a durable pending
        // recovery-phrase ack is REJECTED until the ack succeeds; after the durable
        // ack (the record is gone) the enable is allowed. A non-pending source
        // toggles freely. This is the exact predicate `update_source` enforces.
        let (repo, dir) = temp_repo().await;

        // Seed a pending first-encrypted source (disabled + durable pending-ack).
        let account_id = AccountId::new_v4();
        let account = AccountRow {
            id: account_id,
            email: "u@example.com".to_string(),
            display_name: None,
            state: driven_core::types::AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        };
        repo.upsert_account(&account).await.expect("upsert account");
        let source_id = SourceId::new_v4();
        let row = SourceRow {
            id: source_id,
            account_id,
            display_name: "Secret".to_string(),
            enabled: false,
            local_path: "/home/u/secret".to_string(),
            drive_folder_id: String::new(),
            drive_id: None,
            drive_folder_path: String::new(),
            encryption_enabled: true,
            wrapped_source_key: Some(vec![1, 2, 3]),
            respect_gitignore: true,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: default_deep_verify_secs(),
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 0,
        };
        repo.insert_first_encrypted_source_pending_ack(&row, account_id.to_string(), 0)
            .await
            .expect("seed pending source");

        // Disabling / no-enable is always allowed.
        reject_enable_of_pending_encrypted_source(&repo, source_id, false)
            .await
            .expect("disabling a pending source is allowed");

        // Enabling the pending source is REJECTED (s24 invalid-input).
        let err = reject_enable_of_pending_encrypted_source(&repo, source_id, true)
            .await
            .expect_err("enabling a pending encrypted source must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);

        // After the durable ack (reveal + atomic enable+clear), the record is gone
        // and enabling is allowed.
        repo.mark_recovery_phrase_revealed(source_id).await.unwrap();
        repo.enable_source_and_clear_recovery_ack(source_id)
            .await
            .unwrap();
        reject_enable_of_pending_encrypted_source(&repo, source_id, true)
            .await
            .expect("after the durable ack, enabling is allowed");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn reject_encrypted_add_while_ack_pending_blocks_second_encrypted_add() {
        // R6-P1-1 (DATA-SAFETY gate bypass): while the FIRST encrypted source is
        // still pending its recovery-phrase ack, a SECOND encrypted add on the same
        // account must be REJECTED (s24 invalid-input) - otherwise it would take the
        // already-provisioned path, persist ENABLED, and arm encrypted backups
        // before any recovery phrase was acked (unrestorable backups). A different
        // account is unaffected; once the pending ack clears (durable ack), a
        // further encrypted add is allowed. This is the exact predicate `add_source`
        // enforces under the per-account encrypted lock.
        let (repo, dir) = temp_repo().await;

        // Account A with a pending first encrypted source (disabled + durable ack).
        let account_a = AccountId::new_v4();
        let account = AccountRow {
            id: account_a,
            email: "a@example.com".to_string(),
            display_name: None,
            state: driven_core::types::AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        };
        repo.upsert_account(&account).await.expect("upsert account");
        let first_source = SourceId::new_v4();
        let row = SourceRow {
            id: first_source,
            account_id: account_a,
            display_name: "First".to_string(),
            enabled: false,
            local_path: "/home/u/first".to_string(),
            drive_folder_id: String::new(),
            drive_id: None,
            drive_folder_path: String::new(),
            encryption_enabled: true,
            wrapped_source_key: Some(vec![1, 2, 3]),
            respect_gitignore: true,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: default_deep_verify_secs(),
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 0,
        };
        repo.insert_first_encrypted_source_pending_ack(&row, account_a.to_string(), 0)
            .await
            .expect("seed pending source");

        // A SECOND encrypted add on account A is rejected while the ack is pending.
        let err = reject_encrypted_add_while_ack_pending(&repo, account_a)
            .await
            .expect_err("second encrypted add must be rejected while ack pending");
        assert_eq!(err.code, ErrorCode::InvalidInput);

        // A DIFFERENT account with no pending ack is unaffected.
        let account_b = AccountId::new_v4();
        let account = AccountRow {
            id: account_b,
            email: "b@example.com".to_string(),
            display_name: None,
            state: driven_core::types::AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        };
        repo.upsert_account(&account)
            .await
            .expect("upsert account b");
        reject_encrypted_add_while_ack_pending(&repo, account_b)
            .await
            .expect("a different account with no pending ack is allowed");

        // The first source is DISABLED until the ack clears, so it never runs
        // encrypted backups in the pending window (the scheduler filters on
        // `enabled`).
        let first = find_source(&repo, first_source)
            .await
            .expect("first source");
        assert!(
            !first.enabled,
            "the pending first encrypted source must stay disabled (no backups) until acked"
        );

        // After the durable ack (reveal + atomic enable+clear), the pending row is
        // gone and a further encrypted add on account A is allowed.
        repo.mark_recovery_phrase_revealed(first_source)
            .await
            .unwrap();
        repo.enable_source_and_clear_recovery_ack(first_source)
            .await
            .unwrap();
        reject_encrypted_add_while_ack_pending(&repo, account_a)
            .await
            .expect("after the pending ack clears, a further encrypted add is allowed");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn discard_pending_encrypted_source_clears_stamp_then_next_add_reveals_fresh_phrase() {
        // R5-P1-1 (DATA-SAFETY): the remove path for a pending first encrypted source
        // routes through `discard_pending_encrypted_source`, which clears the
        // account's master-key stamp when it was the only encrypted source. A
        // SUBSEQUENT encrypted source then takes the FRESH-key path (newly_generated
        // = true, a recovery phrase is revealed) rather than the silent
        // already-provisioned path. We exercise the StateRepo + prepare_master_key
        // seam directly (no keychain side effects): after a discard the stamp is
        // NULL, so `prepare_master_key` would mint + reveal a new phrase.
        let (repo, dir) = temp_repo().await;

        // Seed a pending first-encrypted source (disabled + durable pending-ack +
        // stamped master key).
        let account_id = AccountId::new_v4();
        let account = AccountRow {
            id: account_id,
            email: "u@example.com".to_string(),
            display_name: None,
            state: driven_core::types::AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        };
        repo.upsert_account(&account).await.expect("upsert account");
        let source_id = SourceId::new_v4();
        let row = SourceRow {
            id: source_id,
            account_id,
            display_name: "Secret".to_string(),
            enabled: false,
            local_path: "/home/u/secret-discard".to_string(),
            drive_folder_id: String::new(),
            drive_id: None,
            drive_folder_path: String::new(),
            encryption_enabled: true,
            wrapped_source_key: Some(vec![7, 7, 7]),
            respect_gitignore: true,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: default_deep_verify_secs(),
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 0,
        };
        repo.insert_first_encrypted_source_pending_ack(&row, "kc:u-master".into(), 0)
            .await
            .expect("seed pending source");
        // The source has a durable pending ack (so remove_source routes to discard).
        assert!(repo
            .recovery_ack_revealed(source_id)
            .await
            .unwrap()
            .is_some());

        // Discard the pending source (the only encrypted source).
        let outcome = repo
            .discard_pending_encrypted_source(source_id)
            .await
            .expect("discard");
        assert!(
            outcome.master_key_cleared,
            "discarding the only encrypted source must clear the master key"
        );

        // The account stamp is now NULL: a subsequent encrypted source takes the
        // fresh-key path. `prepare_master_key` reflects that decision (the account is
        // unprovisioned again, so `newly_generated` would be true and a phrase
        // returned). We assert the gating column rather than calling the keychain.
        let after = find_account(&repo, account_id).await.expect("account");
        assert_eq!(
            after.encryption_master_key_id, None,
            "after a discard the next encrypted source must re-provision (fresh phrase), not reuse a silent key"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn find_pending_recovery_ack_account_resolves_durable_record() {
        // R4-P1-1: the reveal path resolves the owning account from the DURABLE
        // pending-ack record (so a reveal works even after a restart). An unknown
        // / non-pending source resolves to None.
        let (repo, dir) = temp_repo().await;
        let account_id = AccountId::new_v4();
        let account = AccountRow {
            id: account_id,
            email: "u@example.com".to_string(),
            display_name: None,
            state: driven_core::types::AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        };
        repo.upsert_account(&account).await.expect("upsert account");
        let source_id = SourceId::new_v4();
        let row = SourceRow {
            id: source_id,
            account_id,
            display_name: "Secret".to_string(),
            enabled: false,
            local_path: "/home/u/secret2".to_string(),
            drive_folder_id: String::new(),
            drive_id: None,
            drive_folder_path: String::new(),
            encryption_enabled: true,
            wrapped_source_key: Some(vec![9]),
            respect_gitignore: true,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: default_deep_verify_secs(),
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 0,
        };
        repo.insert_first_encrypted_source_pending_ack(&row, account_id.to_string(), 0)
            .await
            .expect("seed pending source");

        assert_eq!(
            find_pending_recovery_ack_account(&repo, source_id)
                .await
                .unwrap(),
            Some(account_id)
        );
        assert_eq!(
            find_pending_recovery_ack_account(&repo, SourceId::new_v4())
                .await
                .unwrap(),
            None
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn select_picker_store_fake_mode_lists_without_real_creds() {
        // R1-P1-3: under RemoteMode::Fake the picker store is the in-memory fake
        // (no keychain creds touched) and lists its root successfully. A random
        // account id with NO keychain entry would FAIL build_account_store in
        // real mode; here it must succeed because the fake path is taken.
        let (app_state, dir) = fake_app_state().await;
        let account_id = AccountId::new_v4();
        let (store, root) = select_picker_store(&app_state, account_id, &CustomCaConfig::none())
            .expect("fake store builds");
        assert!(!root.is_empty(), "fake root id must be non-empty");
        // The fresh fake root lists (zero child folders) without error / creds.
        let children = store
            .list_folder(&root, &DriveContext::MyDrive)
            .await
            .expect("fake list_folder");
        assert!(
            children.is_empty(),
            "a fresh fake remote has no child folders"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn fake_picker_and_uploader_share_one_store_round_trips_parent_id() {
        // R2-P1-2: the picker's fake store and the orchestrator's fake store must
        // be the SAME instance per account, so a folder id the picker resolves is
        // visible to the uploader. Model that here: the picker store
        // (`select_picker_store`) and the orchestrator store
        // (`AppState::fake_remote_store`) for the same account must share backing
        // objects - a folder created via one is listed via the other, and the
        // picker's root id is the same id the uploader would target.
        let (app_state, dir) = fake_app_state().await;
        let account_id = AccountId::new_v4();

        // The picker resolves the account's fake store + root.
        let (picker_store, picker_root) =
            select_picker_store(&app_state, account_id, &CustomCaConfig::none())
                .expect("picker store");

        // The orchestrator (uploader) holds the SAME shared store for the account.
        let uploader_store = app_state.fake_remote_store(account_id);

        // The picker's root id is the uploader store's root id (one instance).
        assert_eq!(
            picker_root,
            uploader_store.root_id(),
            "picker root id must equal the shared uploader store's root id"
        );

        // Create a folder via the UPLOADER store under the picker's root id; the
        // PICKER store must see it (same backing objects) - proving the parent id
        // the picker minted round-trips to the uploader and back.
        let created = uploader_store
            .ensure_folder(&picker_root, "uploaded-folder", &DriveContext::MyDrive)
            .await
            .expect("uploader create under picker root");

        // The picker store, listing the SAME root, sees the created object.
        let listed = picker_store
            .list_folder(&picker_root, &DriveContext::MyDrive)
            .await
            .expect("picker list shared root");
        assert!(
            listed.iter().any(|e| e.id == created.id),
            "the picker must see the object the uploader created (shared store, R2-P1-2)"
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
