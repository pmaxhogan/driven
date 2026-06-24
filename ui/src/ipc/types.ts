// DTO interfaces mirroring the Rust serde DTOs in
// `src-tauri/src/commands/dtos.rs` (SPEC s11.1 / s11.2 / s11.6). Per the M6
// decision (design/CODEX_NOTES.md M6 section) these are hand-written rather than
// generated with tauri-specta. The Rust side renders camelCase
// (`#[serde(rename_all = "camelCase")]`), so these interfaces use camelCase and
// must be kept in sync with the Rust shapes by convention.

// --- Accounts (SPEC s11.1) ---

/** `accounts.state` serialized form. */
export type AccountState = "ok" | "needs_reauth" | "disabled";

export interface AccountDto {
  id: string;
  email: string;
  displayName: string | null;
  state: AccountState;
  encryptionEnabled: boolean;
  createdAt: number;
  lastSyncedAt: number | null;
}

/** Opaque add-account wizard session id (transparent string newtype). */
export type AddAccountWizardSessionId = string;

/** Alias matching the SPEC s11.1 `SessionId` argument on the OAuth steps. */
export type SessionId = AddAccountWizardSessionId;

export interface OAuthAuthUrl {
  authUrl: string;
}

/** A3: the result of `reauth_account` - the consent URL PLUS the server-side
 * session id the UI threads back through poll/finish to complete re-consent
 * onto the EXISTING account (mirrors src-tauri ReauthSession). */
export interface ReauthSession {
  sessionId: string;
  authUrl: string;
}

/** Mirrors the Rust `OAuthStatus` (internally tagged on `kind`). */
export type OAuthStatus =
  | { kind: "openingBrowser" }
  | { kind: "awaitingCallback" }
  | { kind: "exchangingCode" }
  | { kind: "complete" }
  | { kind: "failed"; code: string };

// --- Sources (SPEC s11.2) ---

export interface SourceDto {
  id: string;
  accountId: string;
  displayName: string;
  enabled: boolean;
  localPath: string;
  driveFolderId: string;
  driveFolderPath: string;
  encryptionEnabled: boolean;
  respectGitignore: boolean;
  includePatterns: string[];
  excludePatterns: string[];
  deepVerifyIntervalSecs: number;
  lastFullScanAt: number | null;
  createdAt: number;
}

export interface AddSourceRequest {
  accountId: string;
  displayName: string;
  /** C1 (SPEC s11.6.1): the one-shot token from `pickFolderDialog` proving the
   * local path came from a backend-owned dialog. Authoritative. */
  localPathToken: string;
  /** Display echo of the chosen local path (NOT authoritative; the backend uses
   * the path bound to `localPathToken`). */
  localPath: string;
  driveFolderId: string;
  driveFolderPath: string;
  encryptionEnabled: boolean;
  respectGitignore: boolean;
  includePatterns: string[];
  excludePatterns: string[];
}

/** B3: the result of `add_source` - the created source PLUS the one-time BIP39
 * recovery phrase, present ONLY when this opt-in generated the account master
 * key (mirrors src-tauri AddSourceResult). The UI shows the phrase once and
 * never persists it. */
export interface AddSourceResult {
  source: SourceDto;
  recoveryPhrase: string[] | null;
}

/** C1: the result of a backend-owned native dialog - the chosen path plus the
 * one-shot token bound to it (mirrors src-tauri PickedPath). */
export interface PickedPath {
  path: string;
  token: string;
}

export interface SourcePatch {
  displayName?: string | null;
  enabled?: boolean | null;
  respectGitignore?: boolean | null;
  includePatterns?: string[] | null;
  excludePatterns?: string[] | null;
  deepVerifyIntervalSecs?: number | null;
}

export interface DriveFolderEntry {
  id: string;
  name: string;
}

export interface DriveFolderListing {
  currentFolderId: string | null;
  currentFolderPath: string;
  folders: DriveFolderEntry[];
}

export interface ExclusionPreviewRequest {
  // R1-P1-2 (SPEC s11.6.1): the preview root is NEVER a raw webview path. Pass
  // EITHER the one-shot dialog token (a NEW candidate folder, from
  // pickFolderDialog) OR an existing source id; the backend resolves the path
  // from the token binding / SQLite. Exactly one must be set.
  localPathToken?: string | null;
  sourceId?: string | null;
  respectGitignore: boolean;
  includePatterns: string[];
  excludePatterns: string[];
}

export interface ExclusionPreview {
  includedCount: number;
  excludedCount: number;
  includedBytes: number;
  includedSample: string[];
  excludedSample: string[];
  truncated: boolean;
}

// --- Settings & misc (SPEC s11.6, s22) ---

export interface GlobalSettings {
  autoStartOnLogin: boolean;
  defaultConcurrentUploads: number | null;
  bandwidthCapMbps: number | null;
  skipOnBattery: boolean;
  skipOnMetered: boolean;
  scanIntervalSecs: number;
  deepVerifyIntervalSecs: number;
  ioPriority: string;
  logLevel: string;
}

export interface TelemetrySettings {
  enabled: boolean;
  installId: string;
  endpoint: string;
}

export interface UpdaterSettings {
  channel: string;
  checkIntervalSecs: number;
}

export interface UiSettings {
  trayLeftClickOpens: string;
  locale: string;
  colorMode: string;
}

export interface WindowsSettings {
  vssMode: string;
}

export interface SettingsDto {
  global: GlobalSettings;
  telemetry: TelemetrySettings;
  updater: UpdaterSettings;
  ui: UiSettings;
  windows: WindowsSettings | null;
}

export interface GlobalSettingsPatch {
  autoStartOnLogin?: boolean;
  defaultConcurrentUploads?: number | null;
  bandwidthCapMbps?: number | null;
  skipOnBattery?: boolean;
  skipOnMetered?: boolean;
  scanIntervalSecs?: number;
  deepVerifyIntervalSecs?: number;
  ioPriority?: string;
  logLevel?: string;
}

export interface TelemetrySettingsPatch {
  enabled?: boolean;
}

export interface UpdaterSettingsPatch {
  channel?: string;
  checkIntervalSecs?: number;
}

export interface UiSettingsPatch {
  trayLeftClickOpens?: string;
  locale?: string;
  colorMode?: string;
}

export interface WindowsSettingsPatch {
  vssMode?: string;
}

export interface SettingsPatch {
  global?: GlobalSettingsPatch;
  telemetry?: TelemetrySettingsPatch;
  updater?: UpdaterSettingsPatch;
  ui?: UiSettingsPatch;
  windows?: WindowsSettingsPatch;
}

export interface UpdateInfo {
  version: string;
  notes: string | null;
  publishedAt: string | null;
  channel: string;
}

export interface ReleaseDto {
  version: string;
  name: string;
  notes: string;
  publishedAt: string;
  url: string;
}

// --- Activity (SPEC s11.4) - mirrors driven-core ActivityEntry + the
// src-tauri activity DTOs ---

/** `activity_log.level` serialized form (mirrors driven_core ActivityLevel). */
export type ActivityLevel = "info" | "warn" | "error";

/** One activity-log entry: the per-row element of an ActivityPage AND the
 * `activity:new` event payload (mirrors driven_core::types::ActivityEntry). */
export interface ActivityEntry {
  id: number;
  ts: number;
  sourceId: string | null;
  level: ActivityLevel;
  eventType: string;
  fileCount: number | null;
  bytes: number | null;
  message: string | null;
}

/** Filter body for `query_activity` (mirrors src-tauri ActivityFilterDto). All
 * fields optional; present fields combine with AND. */
export interface ActivityFilterDto {
  sourceId?: string | null;
  sinceMs?: number | null;
  beforeMs?: number | null;
  minLevel?: ActivityLevel | null;
  eventTypes?: string[];
}

/** KEYSET page selector for `query_activity` (R2-P1-2; mirrors src-tauri
 * PageRequestDto). The activity_log is actively prepended to, so the dashboard
 * pages by the oldest loaded `(ts, id)` CURSOR instead of a shifting offset.
 * `beforeTs` / `beforeId` are both set (a continuation page) or both null/absent
 * (the first, newest page). */
export interface PageRequestDto {
  beforeTs?: number | null;
  beforeId?: number | null;
  limit: number;
}

/** One KEYSET page of activity returned by `query_activity` (R2-P1-2; mirrors
 * src-tauri ActivityPageDto): newest-first entries + the cursor metadata.
 * `nextBeforeTs` / `nextBeforeId` are the `(ts, id)` of the LAST (oldest) row in
 * this page - the cursor the store passes for the next page (both null when the
 * page is empty). `hasMore` is true when older matching rows still exist. */
export interface ActivityPageDto {
  entries: ActivityEntry[];
  total: number;
  limit: number;
  hasMore: boolean;
  nextBeforeTs: number | null;
  nextBeforeId: number | null;
}

/** `file_state.status` serialized form (mirrors driven_core FileStateStatus). */
export type FileStateStatus =
  | "synced"
  | "pending"
  | "corrupt"
  | "locked"
  | "error"
  | "excluded_orphan";

/** One per-status file count for the Activity header (M7-P2-5; mirrors src-tauri
 * FileStatusCountDto / DESIGN s8.3 "file count by status"). */
export interface FileStatusCountDto {
  status: FileStateStatus;
  count: number;
}

/** The Activity dashboard header aggregates (M7-P2-5; mirrors src-tauri
 * ActivitySummaryDto / DESIGN s8.3): bytes uploaded today / this week, file
 * count by status, and the current throughput window (bytes + window length, so
 * the UI derives a bytes/sec rate). */
export interface ActivitySummaryDto {
  bytesToday: number;
  bytesWeek: number;
  fileStatusCounts: FileStatusCountDto[];
  throughputWindowBytes: number;
  throughputWindowMs: number;
}

// --- Sync (SPEC s11.3) - mirrors src-tauri/src/commands/sync.rs ---

/** Mirrors the Rust `OrchestratorState` (driven_core::types). Carried as an
 * opaque tagged object; the UI reads the discriminant for the status pill. */
export type OrchestratorState = Record<string, unknown>;

// NOTE: the Rust GlobalSyncStatus / AccountSyncStatus (M5, sync.rs) do NOT use
// `rename_all = "camelCase"`, so this DTO is snake_case on the wire (unlike the
// M6 DTOs above). Kept faithful to the existing M5 shape.
export interface AccountSyncStatus {
  account_id: string;
  state: OrchestratorState;
}

export interface GlobalSyncStatus {
  accounts: AccountSyncStatus[];
}
