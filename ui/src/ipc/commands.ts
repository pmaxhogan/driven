// Typed `invoke` wrappers, one per `#[tauri::command]` (SPEC s11). The frontend
// always calls these named wrappers - never `invoke("...")` directly - so the
// command name + argument shape live in one place and stay typed against the
// DTOs in `./types`. `@tauri-apps/api/core`'s `invoke` is the single seam vitest
// mocks (see the unit tests), so the stores can be tested without a backend.

import { invoke } from "@tauri-apps/api/core";

import type {
  AccountDto,
  ActivityFilterDto,
  ActivityPageDto,
  ActivitySummaryDto,
  AddAccountWizardSessionId,
  AddSourceRequest,
  AddSourceResult,
  CustomCaValidation,
  DriveFolderListing,
  ExclusionPreview,
  ExclusionPreviewRequest,
  FileSearchHitDto,
  FileVersionDto,
  GlobalSyncStatus,
  OAuthAuthUrl,
  OAuthStatus,
  PageRequestDto,
  PickedPath,
  ReauthSession,
  ReleaseDto,
  RemoteTreeDto,
  RestoreItem,
  RestoreJobId,
  RestoreJobStatus,
  SessionId,
  SettingsDto,
  SettingsPatch,
  SourceDto,
  SourcePatch,
  UpdateInfo,
  VersioningConfig,
  VssHelperStatus,
} from "./types";

// --- Accounts (SPEC s11.1) ---

export function listAccounts(): Promise<AccountDto[]> {
  return invoke("list_accounts");
}

export function beginAddAccountWizard(): Promise<AddAccountWizardSessionId> {
  return invoke("begin_add_account_wizard");
}

export function submitOauthCredentials(
  session: SessionId,
  clientId: string,
  clientSecret: string
): Promise<void> {
  return invoke("submit_oauth_credentials", {
    session,
    clientId,
    clientSecret,
  });
}

export function startOauthSignin(session: SessionId): Promise<OAuthAuthUrl> {
  return invoke("start_oauth_signin", { session });
}

export function pollOauthStatus(session: SessionId): Promise<OAuthStatus> {
  return invoke("poll_oauth_status", { session });
}

/** R4-P2-4: abandon an in-flight add-account / reauth wizard session, dropping
 * it (and its BYO creds + tokens) from the backend registry. Idempotent - safe
 * to call on close even if the session was already consumed by
 * finishAddAccount. */
export function cancelOauthWizard(session: SessionId): Promise<void> {
  return invoke("cancel_oauth_wizard", { session });
}

export function finishAddAccount(
  session: SessionId,
  displayName: string | null
): Promise<AccountDto> {
  return invoke("finish_add_account", { session, displayName });
}

export function removeAccount(accountId: string, deleteRemote: boolean): Promise<void> {
  return invoke("remove_account", { accountId, deleteRemote });
}

export function reauthAccount(accountId: string): Promise<ReauthSession> {
  return invoke("reauth_account", { accountId });
}

// --- Sources (SPEC s11.2) ---

export function listSources(): Promise<SourceDto[]> {
  return invoke("list_sources");
}

export function addSource(req: AddSourceRequest): Promise<AddSourceResult> {
  return invoke("add_source", { req });
}

export function updateSource(sourceId: string, patch: SourcePatch): Promise<SourceDto> {
  return invoke("update_source", { sourceId, patch });
}

export function removeSource(sourceId: string, deleteRemote: boolean): Promise<void> {
  return invoke("remove_source", { sourceId, deleteRemote });
}

export function pickDriveFolder(
  accountId: string,
  startFolderId: string | null,
  driveId: string | null = null
): Promise<DriveFolderListing> {
  return invoke("pick_drive_folder", { accountId, startFolderId, driveId });
}

export function previewExclusions(req: ExclusionPreviewRequest): Promise<ExclusionPreview> {
  return invoke("preview_exclusions", { req });
}

/** Issue #36: the per-source point-in-time versioning config (absent -> OFF). */
export function getSourceVersioning(sourceId: string): Promise<VersioningConfig> {
  return invoke("get_source_versioning", { sourceId });
}

/** Issue #36: enable/configure per-source point-in-time versioning. `countCap` is
 * clamped server-side to `[1, 1000]`. Returns the persisted config. */
export function setSourceVersioning(
  sourceId: string,
  config: VersioningConfig
): Promise<VersioningConfig> {
  return invoke("set_source_versioning", { sourceId, config });
}

/** M9c D4 (M6 R4-P1-1, DATA-SAFETY): backend-reveal the recovery phrase for a
 * source that is awaiting a recovery-phrase ack (the first encrypted source). The
 * backend RECORDS this reveal; `ackRecoveryPhraseSaved` is rejected unless this
 * was called. Returns the 24 BIP39 words (shown once, never persisted). */
export function revealRecoveryPhrase(sourceId: string): Promise<string[]> {
  return invoke("reveal_recovery_phrase", { sourceId });
}

/** M9c D4 (M6 R4-P1-1, DATA-SAFETY): acknowledge that the recovery phrase was
 * saved, which ENABLES the (until-now disabled) first encrypted source so backups
 * can begin. REJECTED by the backend unless a real `revealRecoveryPhrase` was
 * recorded first. Returns the now-enabled source. */
export function ackRecoveryPhraseSaved(sourceId: string): Promise<SourceDto> {
  return invoke("ack_recovery_phrase_saved", { sourceId });
}

// --- Backend-owned native dialogs (SPEC s11.6.1, C1) ---

/** Open the backend-owned native folder picker; returns the chosen path + a
 * one-shot token to pass to `addSource` (SPEC s11.6.1). */
export function pickFolderDialog(): Promise<PickedPath> {
  return invoke("pick_folder_dialog");
}

/** Open the backend-owned native save-file picker for the diagnostic `.zip`;
 * returns the chosen path + a one-shot token to pass to
 * `exportDiagnosticBundle` (SPEC s11.6.1, C2). */
export function pickSaveZipDialog(): Promise<PickedPath> {
  return invoke("pick_save_zip_dialog");
}

// --- Sync (SPEC s11.3) ---

export function syncNow(sourceId: string | null): Promise<void> {
  return invoke("sync_now", { sourceId });
}

export function pauseSync(durationSecs: number | null): Promise<void> {
  return invoke("pause_sync", { durationSecs });
}

export function resumeSync(): Promise<void> {
  return invoke("resume_sync");
}

export function getSyncStatus(): Promise<GlobalSyncStatus> {
  return invoke("get_sync_status");
}

// --- Settings & misc (SPEC s11.6) ---

export function getSettings(): Promise<SettingsDto> {
  return invoke("get_settings");
}

export function updateSettings(patch: SettingsPatch): Promise<SettingsDto> {
  return invoke("update_settings", { patch });
}

/** Least-privilege locked-file backup status for the Settings banner (DESIGN s5.3.1). */
export function getVssHelperStatus(): Promise<VssHelperStatus> {
  return invoke("get_vss_helper_status");
}

export function exportDiagnosticBundle(token: string): Promise<string> {
  return invoke("export_diagnostic_bundle", { token });
}

export function checkForUpdates(): Promise<UpdateInfo | null> {
  return invoke("check_for_updates");
}

export function listReleases(page: number): Promise<ReleaseDto[]> {
  return invoke("list_releases", { page });
}

/** Issue #34: validate a candidate custom root CA PEM file before saving it.
 * Resolves with the certificate count, or rejects with the parse/read error. */
export function validateCustomCa(path: string): Promise<CustomCaValidation> {
  return invoke("validate_custom_ca", { path });
}

// --- In-app updater (SPEC s15.2; M9a) ---

/** Manually check the active channel's signed `update.json` manifest for a newer
 * release (SPEC s15.2). Distinct from `checkForUpdates` (the GitHub-releases
 * About-tab check): this records the pending update so `installUpdate` can apply
 * it, and emits `updater:available`. Returns the update info or null when up to
 * date. */
export function checkForUpdate(): Promise<UpdateInfo | null> {
  return invoke("check_for_update");
}

/** Download + apply the pending update (from the most recent check) and relaunch
 * (SPEC s15.2). Progress arrives on `updater:download_progress`; `updater:downloaded`
 * fires right before the relaunch. On success the app restarts and this never
 * resolves; a missing pending update / signature failure rejects with an s24 code. */
export function installUpdate(): Promise<void> {
  return invoke("install_update");
}

/** The active updater channel (`stable` | `dev`), from settings (SPEC s15.2). */
export function getUpdateChannel(): Promise<string> {
  return invoke("get_update_channel");
}

/** Switch the active updater channel and persist it (SPEC s15.2). Returns the
 * stored channel. The next check uses the new channel. */
export function setUpdateChannel(channel: string): Promise<string> {
  return invoke("set_update_channel", { channel });
}

/** The pending available update recorded by the most recent check (R2-P1-3,
 * SPEC s15.2), or null when none. Used by the app-root updater boot to HYDRATE
 * the store on startup: the startup periodic check can record an update + emit
 * `updater:available` before the webview attaches its listeners, so this catches
 * a missed startup emit. Non-consuming - `installUpdate` still finds it. */
export function getPendingUpdateInfo(): Promise<UpdateInfo | null> {
  return invoke("get_pending_update_info");
}

// --- Anonymous usage telemetry (SPEC s16; M9b) ---

/** Whether anonymous usage stats are sent (SPEC s16). DEFAULT ON; a fresh
 * install reports `true`. The Settings toggle binds to this. */
export function getTelemetryEnabled(): Promise<boolean> {
  return invoke("get_telemetry_enabled");
}

/** Toggle anonymous usage stats (SPEC s16). Persisted immediately and honored by
 * the next ping tick (and the in-flight loop makes no further network call when
 * turned OFF). Returns the stored value. */
export function setTelemetryEnabled(enabled: boolean): Promise<boolean> {
  return invoke("set_telemetry_enabled", { enabled });
}

/** The stable anonymous install id (SPEC s16), minting one on first read if
 * absent. Anonymous - not linkable to a user; safe to show on the privacy note. */
export function getTelemetryInstallId(): Promise<string> {
  return invoke("get_telemetry_install_id");
}

// --- Activity (SPEC s11.4) ---

/** Query a paginated, filtered page of the activity log (SPEC s11.4). The
 * frontend accumulates pages client-side for the history view; the live tail is
 * event-driven via `onActivityNew` (SPEC s11.7), not polled here. */
export function queryActivity(
  filter: ActivityFilterDto,
  page: PageRequestDto
): Promise<ActivityPageDto> {
  return invoke("query_activity", { filter, page });
}

/** Prune activity-log rows older than `beforeTs` (Unix ms); returns the count
 * deleted (SPEC s11.4). */
export function clearActivityOlderThan(beforeTs: number): Promise<number> {
  return invoke("clear_activity_older_than", { beforeTs });
}

/** The DISTINCT set of activity event types in the durable log, sorted (M7-P2-4).
 * Backs the event-type filter dropdown so the user can filter for a type present
 * in history but not in the currently-loaded rows. */
export function distinctActivityEventTypes(): Promise<string[]> {
  return invoke("distinct_activity_event_types");
}

/** The Activity dashboard header aggregates (M7-P2-5; DESIGN s8.3). The day /
 * week boundaries are computed by the caller from the LOCAL `Date` (so the day
 * boundary honours the user's timezone); the backend derives the throughput
 * window start from `now - throughputWindowMs`. */
export function activitySummary(
  dayStartMs: number,
  weekStartMs: number,
  throughputWindowMs: number
): Promise<ActivitySummaryDto> {
  return invoke("activity_summary", {
    dayStartMs,
    weekStartMs,
    throughputWindowMs,
  });
}

// --- Restore (SPEC s11.5; DESIGN s8.4) ---

/** List the immediate children (sub-folders + files) of `prefix` in the backed-up
 * tree (SPEC s11.5). Reads file_state (local metadata), never Drive; names are
 * plaintext even for encrypted sources. `prefix` is a Drive-relative plaintext
 * path (empty string = the source root). */
export function listRemoteTree(sourceId: string, prefix: string): Promise<RemoteTreeDto> {
  return invoke("list_remote_tree", { sourceId, prefix });
}

/** Search backed-up files by filename / glob (SPEC s11.5). A query with a glob
 * metacharacter (`*`, `?`, `[`) routes to the wildcard path; otherwise to the
 * FTS5 prefix/term path. `sourceId === null` searches across all sources. */
export function searchFiles(
  sourceId: string | null,
  query: string,
  limit: number
): Promise<FileSearchHitDto[]> {
  return invoke("search_files", { sourceId, query, limit });
}

/** Restore selected files to the dialog-approved destination folder (SPEC s11.5).
 * `destToken` is a one-shot token from `pickFolderDialog` (the webview never
 * supplies a raw path). Returns the spawned job id; progress arrives on
 * `restore:progress` (subscribe via `onRestoreProgress`). */
export function restoreFiles(
  items: RestoreItem[],
  destToken: string,
  /** Issue #36: optional point-in-time (Unix ms). When set, each file is
   * restored as it was backed up as of that instant (the current bytes if they
   * were already in place, else the retained version whose window covers it). */
  asOf?: number | null
): Promise<RestoreJobId> {
  return invoke("restore_files", { items, destToken, asOf: asOf ?? null });
}

/** Issue #36: the retained point-in-time versions of one file, newest-first.
 * Reads local `file_versions` metadata (never Drive). Powers the version-history
 * view so the user can see which dates have a restorable version. */
export function listFileVersions(
  sourceId: string,
  relativePath: string
): Promise<FileVersionDto[]> {
  return invoke("list_file_versions", { sourceId, relativePath });
}

/** The current status snapshot of a restore job (SPEC s11.5), for a late /
 * reconnected subscriber that missed the live `restore:progress` stream. */
export function getRestoreJob(job: RestoreJobId): Promise<RestoreJobStatus> {
  return invoke("get_restore_job", { job });
}

/** Cancel a running restore job (SPEC s11.5; M8-P1-1). The backend stops the job
 * between frames, DELETES any in-flight temp file (no partial left), and emits a
 * terminal CANCELLED status on `restore:progress`. Idempotent: cancelling an
 * unknown / already-finished job is a no-op. */
export function cancelRestoreJob(job: RestoreJobId): Promise<void> {
  return invoke("cancel_restore_job", { job });
}
