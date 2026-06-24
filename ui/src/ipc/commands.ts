// Typed `invoke` wrappers, one per `#[tauri::command]` (SPEC s11). The frontend
// always calls these named wrappers - never `invoke("...")` directly - so the
// command name + argument shape live in one place and stay typed against the
// DTOs in `./types`. `@tauri-apps/api/core`'s `invoke` is the single seam vitest
// mocks (see the unit tests), so the stores can be tested without a backend.

import { invoke } from "@tauri-apps/api/core";

import type {
  AccountDto,
  AddAccountWizardSessionId,
  AddSourceRequest,
  DriveFolderListing,
  ExclusionPreview,
  ExclusionPreviewRequest,
  GlobalSyncStatus,
  OAuthAuthUrl,
  OAuthStatus,
  ReleaseDto,
  SessionId,
  SettingsDto,
  SettingsPatch,
  SourceDto,
  SourcePatch,
  UpdateInfo,
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
  clientSecret: string,
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

export function finishAddAccount(
  session: SessionId,
  displayName: string | null,
): Promise<AccountDto> {
  return invoke("finish_add_account", { session, displayName });
}

export function removeAccount(
  accountId: string,
  deleteRemote: boolean,
): Promise<void> {
  return invoke("remove_account", { accountId, deleteRemote });
}

export function reauthAccount(accountId: string): Promise<OAuthAuthUrl> {
  return invoke("reauth_account", { accountId });
}

// --- Sources (SPEC s11.2) ---

export function listSources(): Promise<SourceDto[]> {
  return invoke("list_sources");
}

export function addSource(req: AddSourceRequest): Promise<SourceDto> {
  return invoke("add_source", { req });
}

export function updateSource(
  sourceId: string,
  patch: SourcePatch,
): Promise<SourceDto> {
  return invoke("update_source", { sourceId, patch });
}

export function removeSource(
  sourceId: string,
  deleteRemote: boolean,
): Promise<void> {
  return invoke("remove_source", { sourceId, deleteRemote });
}

export function pickDriveFolder(
  accountId: string,
  startFolderId: string | null,
): Promise<DriveFolderListing> {
  return invoke("pick_drive_folder", { accountId, startFolderId });
}

export function previewExclusions(
  req: ExclusionPreviewRequest,
): Promise<ExclusionPreview> {
  return invoke("preview_exclusions", { req });
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

export function exportDiagnosticBundle(dest: string): Promise<string> {
  return invoke("export_diagnostic_bundle", { dest });
}

export function checkForUpdates(): Promise<UpdateInfo | null> {
  return invoke("check_for_updates");
}

export function listReleases(page: number): Promise<ReleaseDto[]> {
  return invoke("list_releases", { page });
}
