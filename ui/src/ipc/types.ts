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

/** Per-source policy for OneDrive / cloud-only placeholder files (issue #4).
 * Serialized form of the Rust `PlaceholderPolicy` enum (snake_case). `"skip"`
 * (the default) leaves cloud-only placeholders on Drive untouched; `"force_download"`
 * backs them up, letting the read hydrate each file. Windows-only in effect. */
export type PlaceholderPolicy = "skip" | "force_download";

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
  /** Issue #4: OneDrive / cloud-only placeholder policy. */
  placeholderPolicy: PlaceholderPolicy;
  deepVerifyIntervalSecs: number;
  lastFullScanAt: number | null;
  createdAt: number;
  /** R4-P1-2 (DATA-SAFETY): true when this first-encrypted source is still
   * awaiting its recovery-phrase ack (persisted disabled). The UI disables the
   * enable toggle for it (the backend update_source is the real guard). */
  pendingRecoveryAck: boolean;
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
  /** Issue #7: the Google Shared Drive id the destination folder lives in, from
   * `pickDriveFolder`. `null` (or omitted) means My Drive. */
  driveId?: string | null;
  driveFolderPath: string;
  encryptionEnabled: boolean;
  respectGitignore: boolean;
  includePatterns: string[];
  excludePatterns: string[];
  /** Issue #4: OneDrive / cloud-only placeholder policy. Optional; the backend
   * defaults an omitted value to `"skip"`. */
  placeholderPolicy?: PlaceholderPolicy;
}

/** B3: the result of `add_source` - the created source PLUS the one-time BIP39
 * recovery phrase, present ONLY when this opt-in generated the account master
 * key (mirrors src-tauri AddSourceResult). The UI shows the phrase once and
 * never persists it. */
export interface AddSourceResult {
  source: SourceDto;
  recoveryPhrase: string[] | null;
  /** M9c D4 (M6 R4-P1-1, DATA-SAFETY): true when this source was persisted
   * DISABLED and awaits a recovery-phrase ack (the first encrypted source for
   * the account). It is NOT backed up until `revealRecoveryPhrase` +
   * `ackRecoveryPhraseSaved` enable it, so no unrestorable encrypted backups can
   * run before the phrase is durably saved. False otherwise. */
  pendingRecoveryAck: boolean;
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
  /** Issue #4: change the OneDrive / cloud-only placeholder policy. */
  placeholderPolicy?: PlaceholderPolicy | null;
  deepVerifyIntervalSecs?: number | null;
}

export interface DriveFolderEntry {
  id: string;
  name: string;
  /** Issue #7: the Google Shared Drive this entry lives in - the `driveId` for a
   * Shared Drive root or a folder inside one, `null`/absent for a My Drive
   * folder. Carried back into `pickDriveFolder` when descending, and into
   * `addSource` when the folder is selected. */
  driveId?: string | null;
  /** Issue #7: true when this entry is a Shared Drive ROOT (vs an ordinary
   * folder), so the picker can badge it. */
  isSharedDrive?: boolean;
}

export interface DriveFolderListing {
  currentFolderId: string | null;
  /** Issue #7: the Google Shared Drive the current folder lives in (`driveId`),
   * or `null` for My Drive. Persisted with `currentFolderId` on selection. */
  driveId?: string | null;
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

export interface ScheduleSettings {
  enabled: boolean;
  /** Minutes after local midnight the allowed window opens, 0..=1439. */
  startMinute: number;
  /** Minutes after local midnight the allowed window closes, 0..=1439.
   *  end < start wraps past midnight; end == start allows the whole day. */
  endMinute: number;
  /** Seven booleans, index 0=Sunday..6=Saturday (matches Date.getDay()). */
  days: boolean[];
  /** Minutes to add to UTC to reach local time (-new Date().getTimezoneOffset()). */
  utcOffsetMinutes: number;
}

export interface GlobalSettings {
  autoStartOnLogin: boolean;
  defaultConcurrentUploads: number | null;
  /** Whether the adaptive upload-parallelism controller runs (DESIGN 11.4.7). */
  adaptiveParallelismEnabled: boolean;
  bandwidthCapMbps: number | null;
  skipOnBattery: boolean;
  skipOnMetered: boolean;
  scanIntervalSecs: number;
  deepVerifyIntervalSecs: number;
  ioPriority: string;
  logLevel: string;
  schedule: ScheduleSettings;
  /** V2 pre/post backup shell hooks (null = no hook). */
  preBackupHook: string | null;
  postBackupHook: string | null;
  /** How long a hook may run before it is killed, in seconds. */
  hookTimeoutSecs: number;
  /** V2 metered behaviour: "pause" | "throttle". */
  meteredMode: string;
  /** Bandwidth cap (Mbps) while metered in throttle mode; null falls back. */
  meteredBandwidthCapMbps: number | null;
  /** Issue #34: path to a PEM file of custom root CA certificate(s) to ADD to
   * the system trust store for ALL outbound connections (corporate / TLS-
   * inspection environments). null = system trust only. Additive - never
   * replaces the OS roots, never disables verification. */
  customRootCaPath: string | null;
}

export interface TelemetrySettings {
  enabled: boolean;
  installId: string;
  endpoint: string;
}

/** The EXACT JSON payload a telemetry ping would POST (SPEC s16 preview),
 * returned verbatim by `preview_telemetry_ping` for display. Field names are
 * intentionally snake_case here (not the app's usual camelCase IPC
 * convention) - this mirrors the telemetry-worker wire schema byte-for-byte,
 * since showing the user anything reshaped would defeat the point of a
 * preview. The nested groups are left loosely typed (`Record<string,
 * unknown>`) since the UI only pretty-prints the whole payload; it does not
 * read individual fields. */
export interface TelemetryPreviewPayload {
  install_id: string;
  ts: number;
  version: string;
  os: string;
  os_version: string | null;
  arch: string;
  channel: string;
  events_24h: Record<string, unknown>;
  latency_p50_p95_ms: Record<string, unknown>;
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
  vssHelper: boolean;
}

/** Status of least-privilege locked-file backup (DESIGN s5.3.1). */
export interface VssHelperStatus {
  supported: boolean;
  elevated: boolean;
  helperEnabled: boolean;
  /** Issue #25: the elevated broker has been launched this session (lazy - it
   * only launches on the first locked file, so usually false at Settings-open). */
  helperAlive: boolean;
  /** Issue #25: the broker can be brought up on demand (sidecar present, no prior
   * launch failure), so locked-file backup is available even before the launch. */
  helperLaunchable: boolean;
  /** Issue #25: a launch is in progress - awaiting elevation (UAC) approval; the
   * Rules tab shows a "waiting for approval" hint until it resolves. */
  launchPending: boolean;
  /** Issue #25: the user declined the elevation prompt this session (memoised). */
  launchDeclined: boolean;
  lockedFileBackupDegraded: boolean;
}

export interface SettingsDto {
  global: GlobalSettings;
  telemetry: TelemetrySettings;
  updater: UpdaterSettings;
  ui: UiSettings;
  windows: WindowsSettings | null;
  /** V2 small-file bundling on/off (issue #35). A standalone advanced toggle. */
  bundleSmallFiles: boolean;
}

export interface GlobalSettingsPatch {
  autoStartOnLogin?: boolean;
  defaultConcurrentUploads?: number | null;
  adaptiveParallelismEnabled?: boolean;
  bandwidthCapMbps?: number | null;
  skipOnBattery?: boolean;
  skipOnMetered?: boolean;
  scanIntervalSecs?: number;
  deepVerifyIntervalSecs?: number;
  ioPriority?: string;
  logLevel?: string;
  /** Present = replace the whole schedule window. */
  schedule?: ScheduleSettings;
  /** Present = set; null clears the hook. */
  preBackupHook?: string | null;
  postBackupHook?: string | null;
  hookTimeoutSecs?: number;
  meteredMode?: string;
  meteredBandwidthCapMbps?: number | null;
  /** Issue #34: present = set (validated on save); null clears it back to
   * system-trust-only. */
  customRootCaPath?: string | null;
}

/** Issue #34: result of validating a candidate custom root CA PEM file. */
export interface CustomCaValidation {
  /** Number of certificates the PEM bundle contains (>= 1 on success). */
  certCount: number;
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
  vssHelper?: boolean;
}

export interface SettingsPatch {
  global?: GlobalSettingsPatch;
  telemetry?: TelemetrySettingsPatch;
  updater?: UpdaterSettingsPatch;
  ui?: UiSettingsPatch;
  windows?: WindowsSettingsPatch;
  /** Toggle V2 small-file bundling (issue #35). Absent = leave unchanged. */
  bundleSmallFiles?: boolean;
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
  "synced" | "pending" | "corrupt" | "locked" | "error" | "excluded_orphan";

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
 * opaque tagged object; the UI reads the discriminant for the status pill.
 * On the wire it is internally tagged on a snake_case `state` field (e.g.
 * `{ state: "executing", progress: ExecProgress }`, `{ state: "idle", last_run_at }`). */
export type OrchestratorState = Record<string, unknown>;

/** Mirrors the Rust `ExecProgress` (driven_core::types) - the `progress` payload
 * of an `executing` OrchestratorState. Plain snake_case on the wire (the Rust
 * struct has no `rename_all`). The global progress bar aggregates these across
 * executing accounts to compute a determinate completion percent. */
export interface ExecProgress {
  files_done: number;
  files_total: number;
  bytes_done: number;
  bytes_total: number;
  trashes_done: number;
  trashes_total: number;
  errors: number;
}

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

// --- Restore (SPEC s11.5; DESIGN s8.4) - mirrors src-tauri restore DTOs ---

/** One node in the Restore browser tree (mirrors src-tauri RemoteEntryDto):
 * either a folder (descendable) or a restorable file. Derived from file_state;
 * the name is plaintext even for encrypted sources (file_state stores the
 * plaintext path). */
export interface RemoteEntryDto {
  relativePath: string;
  name: string;
  isDir: boolean;
  size: number;
  status: FileStateStatus | null;
  restorable: boolean;
}

/** The result of `listRemoteTree` (mirrors src-tauri RemoteTreeDto, M8-P2-1):
 * the immediate children plus a `truncated` flag so the UI can tell the user the
 * listing was capped (rather than silently dropping children past the cap). */
export interface RemoteTreeDto {
  entries: RemoteEntryDto[];
  truncated: boolean;
}

/** One Restore search hit (mirrors src-tauri FileSearchHitDto). */
export interface FileSearchHitDto {
  sourceId: string;
  relativePath: string;
  status: FileStateStatus;
  restorable: boolean;
}

/** One file selected to restore (mirrors src-tauri RestoreItem): the
 * (sourceId, relativePath) file_state key. The backend re-reads the Drive id +
 * size from SQLite; the webview never supplies a local path. */
export interface RestoreItem {
  sourceId: string;
  relativePath: string;
}

/** Issue #36: per-source point-in-time versioning config (mirrors src-tauri /
 * driven-core VersioningConfig). Stored in the settings KV, not a source column. */
export interface VersioningConfig {
  /** When true, a content change creates a NEW backup version and keeps the old
   * one restorable, instead of overwriting in place. */
  enabled: boolean;
  /** Max retained versions per file (server-clamped to [1, 1000]). */
  countCap: number;
  /** Size guard in bytes: changes to files larger than this are not versioned.
   * `0` disables the guard. */
  maxBytes: number;
}

/** Issue #36: one retained point-in-time version of a file (mirrors src-tauri
 * FileVersionDto). It was the file's current content during
 * `[createdAt, supersededAt)` (Unix ms). */
export interface FileVersionDto {
  /** Plaintext size in bytes. */
  size: number;
  /** When this version first became the current backup (Unix ms). */
  createdAt: number;
  /** When it was superseded by the next version (Unix ms). */
  supersededAt: number;
  /** True once the old Drive object was moved to trash (restorable by date until
   * Drive purges its trash). */
  trashed: boolean;
}

/** The opaque id of a spawned restore job (mirrors src-tauri RestoreJobId). */
export type RestoreJobId = string;

/** Per-file lifecycle state within a restore job (mirrors src-tauri
 * RestoreFileState). `cancelled` (M8-P1-1) means the user cancelled before this
 * file finished; any partial temp was deleted (no half-written file). */
export type RestoreFileState = "pending" | "restoring" | "done" | "failed" | "cancelled";

/** Per-file progress within a restore job (mirrors src-tauri
 * RestoreFileProgress). `errorCode` is a stable SPEC s24 i18n key when failed. */
export interface RestoreFileProgress {
  relativePath: string;
  state: RestoreFileState;
  bytesDone: number;
  bytesTotal: number;
  errorCode: string | null;
}

/** The full status of a restore job (mirrors src-tauri RestoreJobStatus): the
 * `restore:progress` event payload AND the `getRestoreJob` result. Carries
 * overall progress, the current file, the per-file breakdown, and a terminal
 * `done` flag. */
export interface RestoreJobStatus {
  jobId: string;
  totalFiles: number;
  completedFiles: number;
  failedFiles: number;
  totalBytes: number;
  bytesDone: number;
  currentFile: string | null;
  done: boolean;
  /** `true` when the job's terminal state is a user CANCELLATION (M8-P1-1).
   * `done && !cancelled` is a normal finish; `done && cancelled` means the job
   * was stopped early and any in-flight temp file was deleted (no partial). */
  cancelled: boolean;
  files: RestoreFileProgress[];
}
