// Realistic default backend data for the scripted IPC mock (./mock-backend).
//
// Everything here is FROZEN IN TIME around `FIXED_NOW`: the visual suite pins
// the page clock to that instant (playwright.config.ts) and runs in UTC, so a
// rendered timestamp is a pure function of these constants. Deriving fixture
// timestamps from the real `Date.now()` would make every screenshot differ from
// the one taken a second later, which is the single easiest way to turn a
// visual suite into noise.
//
// These are the DEFAULTS - the shape every command answers with when a scenario
// does not override it. A scenario overrides the handful of commands its
// surface actually depends on and inherits the rest, so adding a command to the
// app means adding one default here rather than touching every spec.

import type {
  AccountDto,
  ActivityEntry,
  ActivityPageDto,
  ActivitySummaryDto,
  ActivityThroughputSeriesDto,
  ApfsHelperStatus,
  BackendDto,
  DriveFolderListing,
  ExclusionPreview,
  FileSearchHitDto,
  FileVersionDto,
  GlobalSyncStatus,
  QueueSnapshot,
  ReleaseDto,
  RemoteTreeDto,
  RestoreJobStatus,
  DrillRun,
  ScrubRun,
  SettingsDto,
  SourceDto,
  TelemetryPreviewPayload,
  UpdateInfo,
  VersioningConfig,
  VssHelperStatus,
  IoThroughputSeriesDto,
  BottleneckSnapshot,
} from "../src/ipc/types";

/** The instant every fixture is anchored to: 2026-03-15T12:00:00Z.
 *
 * The visual suite pins the browser clock here, so "5 minutes ago" in the
 * fixtures renders as exactly that no matter when the suite runs. */
export const FIXED_NOW = Date.UTC(2026, 2, 15, 12, 0, 0);

const SECOND = 1000;
const MINUTE = 60 * SECOND;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** The app version reported to `plugin:app|version`.
 *
 * Deliberately a FIXED FAKE rather than the real `package.json` version:
 * release-please bumps that on every release, and the About surface renders it,
 * so a real version would invalidate the committed baselines on every release
 * PR. */
export const MOCK_APP_VERSION = "9.9.9";

// --- Accounts ---

export const ACCOUNT_DRIVE: AccountDto = {
  id: "acct-drive-1",
  email: "ada@example.com",
  displayName: "Ada's Drive",
  state: "ok",
  encryptionEnabled: true,
  createdAt: FIXED_NOW - 90 * DAY,
  lastSyncedAt: FIXED_NOW - 5 * MINUTE,
  backendKind: "google_drive",
};

export const ACCOUNT_S3: AccountDto = {
  id: "acct-s3-1",
  email: "backups@r2.example.com",
  displayName: "Offsite bucket",
  state: "ok",
  encryptionEnabled: false,
  createdAt: FIXED_NOW - 30 * DAY,
  lastSyncedAt: FIXED_NOW - 2 * HOUR,
  backendKind: "s3",
};

/** An account whose refresh token was revoked - drives the reauth affordances. */
export const ACCOUNT_NEEDS_REAUTH: AccountDto = {
  id: "acct-drive-2",
  email: "grace@example.com",
  displayName: null,
  state: "needs_reauth",
  encryptionEnabled: false,
  createdAt: FIXED_NOW - 45 * DAY,
  lastSyncedAt: FIXED_NOW - 3 * DAY,
  backendKind: "google_drive",
};

export const ACCOUNTS: AccountDto[] = [ACCOUNT_DRIVE, ACCOUNT_S3];

/** Mirrors `driven_backend::descriptors()` in picker order (Drive, S3, folder),
 * including the issue #220 `supportsVersionHistory` split - only Drive can
 * honour a point-in-time restore. */
export const BACKENDS: BackendDto[] = [
  {
    id: "google_drive",
    usesOauth: true,
    supportsFolderPicker: true,
    supportsVersionHistory: true,
    supportsRename: true,
    isDefault: true,
  },
  {
    id: "s3",
    usesOauth: false,
    supportsFolderPicker: true,
    supportsVersionHistory: false,
    supportsRename: false,
    isDefault: false,
  },
  {
    id: "local_folder",
    usesOauth: false,
    supportsFolderPicker: false,
    supportsVersionHistory: false,
    supportsRename: false,
    isDefault: false,
  },
];

// --- Sources ---

export const SOURCE_DOCUMENTS: SourceDto = {
  id: "src-documents",
  accountId: ACCOUNT_DRIVE.id,
  displayName: "Documents",
  enabled: true,
  localPath: "/Users/ada/Documents",
  driveFolderId: "folder-documents",
  driveFolderPath: "/Driven Backups/Documents",
  encryptionEnabled: true,
  respectGitignore: true,
  includePatterns: [],
  excludePatterns: ["*.tmp", "node_modules/"],
  placeholderPolicy: "skip",
  deepVerifyIntervalSecs: 7 * 24 * 3600,
  lastFullScanAt: FIXED_NOW - 40 * MINUTE,
  createdAt: FIXED_NOW - 90 * DAY,
  pendingRecoveryAck: false,
};

export const SOURCE_PHOTOS: SourceDto = {
  id: "src-photos",
  accountId: ACCOUNT_S3.id,
  displayName: "Photos",
  enabled: true,
  localPath: "/Users/ada/Pictures",
  driveFolderId: "photos",
  driveFolderPath: "photos",
  encryptionEnabled: false,
  respectGitignore: false,
  includePatterns: ["*.jpg", "*.raw"],
  excludePatterns: [],
  placeholderPolicy: "skip",
  deepVerifyIntervalSecs: 30 * 24 * 3600,
  lastFullScanAt: FIXED_NOW - 6 * HOUR,
  createdAt: FIXED_NOW - 30 * DAY,
  pendingRecoveryAck: false,
};

export const SOURCE_DISABLED: SourceDto = {
  id: "src-archive",
  accountId: ACCOUNT_DRIVE.id,
  displayName: "Archive",
  enabled: false,
  localPath: "/Volumes/Backup/Archive",
  driveFolderId: "folder-archive",
  driveFolderPath: "/Driven Backups/Archive",
  encryptionEnabled: false,
  respectGitignore: true,
  includePatterns: [],
  excludePatterns: [],
  placeholderPolicy: "skip",
  deepVerifyIntervalSecs: 7 * 24 * 3600,
  lastFullScanAt: null,
  createdAt: FIXED_NOW - 10 * DAY,
  pendingRecoveryAck: false,
};

export const SOURCES: SourceDto[] = [SOURCE_DOCUMENTS, SOURCE_PHOTOS, SOURCE_DISABLED];

// --- Activity ---

/** Newest-first activity rows spanning info / warn / error and several event
 * types, so one screenshot exercises every level pill and the localized event
 * labels rather than a single repeated row. */
export const ACTIVITY_ENTRIES: ActivityEntry[] = [
  {
    id: 1042,
    ts: FIXED_NOW - 2 * MINUTE,
    sourceId: SOURCE_DOCUMENTS.id,
    level: "info",
    eventType: "upload_done",
    fileCount: 12,
    bytes: 48_234_496,
    message: null,
  },
  {
    id: 1041,
    ts: FIXED_NOW - 7 * MINUTE,
    sourceId: SOURCE_PHOTOS.id,
    level: "info",
    eventType: "bundle_upload",
    fileCount: 340,
    bytes: 12_582_912,
    message: null,
  },
  {
    id: 1040,
    ts: FIXED_NOW - 11 * MINUTE,
    sourceId: SOURCE_DOCUMENTS.id,
    level: "warn",
    eventType: "local.invalid_filename",
    fileCount: 2,
    bytes: null,
    message: "2 files skipped: name contains a reserved character",
  },
  {
    id: 1039,
    ts: FIXED_NOW - 18 * MINUTE,
    sourceId: SOURCE_PHOTOS.id,
    level: "error",
    eventType: "error",
    fileCount: 1,
    bytes: null,
    message: "Upload failed after 5 attempts: connection reset by peer",
  },
  {
    id: 1038,
    ts: FIXED_NOW - 24 * MINUTE,
    sourceId: SOURCE_DOCUMENTS.id,
    level: "info",
    eventType: "trash_done",
    fileCount: 3,
    bytes: null,
    message: null,
  },
  {
    id: 1037,
    ts: FIXED_NOW - 40 * MINUTE,
    sourceId: SOURCE_DOCUMENTS.id,
    level: "info",
    eventType: "scan_done",
    fileCount: 18_442,
    bytes: null,
    message: null,
  },
  {
    id: 1036,
    ts: FIXED_NOW - 55 * MINUTE,
    sourceId: null,
    level: "info",
    eventType: "backup_done",
    fileCount: 355,
    bytes: 60_817_408,
    message: null,
  },
  {
    id: 1035,
    ts: FIXED_NOW - 2 * HOUR,
    sourceId: SOURCE_PHOTOS.id,
    level: "info",
    eventType: "scrub_done",
    fileCount: 500,
    bytes: null,
    message: null,
  },
  {
    id: 1034,
    ts: FIXED_NOW - 5 * HOUR,
    sourceId: SOURCE_DOCUMENTS.id,
    level: "warn",
    eventType: "scrub_drift_found",
    fileCount: 1,
    bytes: null,
    message: "1 object re-uploaded after a size mismatch",
  },
  {
    id: 1033,
    ts: FIXED_NOW - 9 * HOUR,
    sourceId: null,
    level: "info",
    eventType: "paused",
    fileCount: null,
    bytes: null,
    message: null,
  },
  {
    id: 1032,
    ts: FIXED_NOW - 20 * HOUR,
    sourceId: SOURCE_DOCUMENTS.id,
    level: "info",
    eventType: "deep_verify_done",
    fileCount: 18_442,
    bytes: null,
    message: null,
  },
  {
    id: 1031,
    ts: FIXED_NOW - 26 * HOUR,
    sourceId: null,
    level: "info",
    eventType: "update_applied",
    fileCount: null,
    bytes: null,
    message: "Updated to 9.9.9",
  },
];

/** A full first page. `hasMore: false` on purpose: a second page could resolve
 * mid-capture and shift the table under the screenshot. Specs that want the
 * "load more" affordance override this. */
export const ACTIVITY_PAGE: ActivityPageDto = {
  entries: ACTIVITY_ENTRIES,
  total: ACTIVITY_ENTRIES.length,
  limit: 100,
  hasMore: false,
  nextBeforeTs: ACTIVITY_ENTRIES[ACTIVITY_ENTRIES.length - 1].ts,
  nextBeforeId: ACTIVITY_ENTRIES[ACTIVITY_ENTRIES.length - 1].id,
};

export const ACTIVITY_PAGE_EMPTY: ActivityPageDto = {
  entries: [],
  total: 0,
  limit: 100,
  hasMore: false,
  nextBeforeTs: null,
  nextBeforeId: null,
};

export const ACTIVITY_EVENT_TYPES: string[] = [
  "backup_done",
  "bundle_upload",
  "deep_verify_done",
  "error",
  "local.invalid_filename",
  "paused",
  "restore.drill_failed",
  "scan_done",
  "scrub_done",
  "scrub_drift_found",
  "trash_done",
  "update_applied",
  "upload_done",
];

export const ACTIVITY_SUMMARY: ActivitySummaryDto = {
  bytesToday: 1_073_741_824,
  bytesWeek: 7_516_192_768,
  fileStatusCounts: [
    { status: "synced", count: 18_402 },
    { status: "pending", count: 34 },
    { status: "locked", count: 4 },
    { status: "error", count: 2 },
  ],
  throughputWindowBytes: 314_572_800,
  throughputWindowFiles: 128,
  throughputWindowMs: 5 * MINUTE,
};

export const ACTIVITY_SUMMARY_EMPTY: ActivitySummaryDto = {
  bytesToday: 0,
  bytesWeek: 0,
  fileStatusCounts: [],
  throughputWindowBytes: 0,
  throughputWindowFiles: 0,
  throughputWindowMs: 5 * MINUTE,
};

/** A hand-written sparkline series with a visible shape (ramp, peak, decay), so
 * the chart is a real chart in the screenshot rather than a flat line. */
export const ACTIVITY_THROUGHPUT: ActivityThroughputSeriesDto = {
  bytes: [
    0, 0, 1_048_576, 4_194_304, 12_582_912, 25_165_824, 41_943_040, 60_817_408, 52_428_800,
    31_457_280, 18_874_368, 20_971_520, 35_651_584, 47_185_920, 29_360_128, 10_485_760, 4_194_304,
    2_097_152, 6_291_456, 1_048_576,
  ],
  files: [0, 0, 1, 3, 8, 14, 21, 28, 24, 15, 9, 10, 17, 22, 14, 6, 3, 1, 4, 1],
};

export const ACTIVITY_THROUGHPUT_EMPTY: ActivityThroughputSeriesDto = {
  bytes: new Array(20).fill(0),
  files: new Array(20).fill(0),
};

/** 2026-08-14 follow-up: a FIXED trailing window for the live disk/network
 * tiles. Deterministic (pinned to FIXED_NOW, and the mock never emits
 * `sync:io_throughput` events) so the visual baselines cannot drift: a
 * disk-read burst racing ahead of a steadier upload - the shape a reconcile
 * resume actually produces. */
export const IO_THROUGHPUT: IoThroughputSeriesDto = {
  bucketMs: 1000,
  samples: Array.from({ length: 60 }, (_, i) => ({
    tsMs: FIXED_NOW - (60 - i) * 1000,
    diskBytes: i < 10 ? 0 : i < 40 ? 180_000_000 + (i % 5) * 8_000_000 : i < 50 ? 60_000_000 : 0,
    netBytes: i < 5 ? 0 : 15_000_000 + (i % 7) * 1_500_000,
  })),
};

/** issue #308: a deterministic "network-bound upload" bottleneck reading for
 * the Activity dashboard's Bottleneck stat tile, matching the shape
 * `IO_THROUGHPUT` implies (a steadier upload trailing a disk-read burst). The
 * store adopts the FIRST snapshot immediately (no debounce on a hydration
 * read), so this renders straight away in the visual baselines. */
export const BOTTLENECK_STATUS: BottleneckSnapshot = {
  tsMs: FIXED_NOW,
  state: "network",
  rateBytesPerSec: 42_000_000,
  backend: null,
  backoffRemainingMs: null,
};

/**
 * Recent restore-drill runs, newest first. Deliberately covers all three
 * outcomes: a pass, a run that could not restore a file, and an INCONCLUSIVE
 * run that verified nothing - the last one exists so a test can prove the UI
 * never renders "verified nothing" as a pass.
 */
export const DRILL_RUNS: DrillRun[] = [
  {
    id: 12,
    sourceId: SOURCE_DOCUMENTS.id,
    startedAt: FIXED_NOW - 3 * HOUR,
    finishedAt: FIXED_NOW - 3 * HOUR + 9 * SECOND,
    sampled: 3,
    verified: 3,
    skipped: 0,
    failed: 0,
    failureCodes: [],
    outcome: "passed",
  },
  {
    id: 11,
    sourceId: SOURCE_PHOTOS.id,
    startedAt: FIXED_NOW - 30 * HOUR,
    finishedAt: FIXED_NOW - 30 * HOUR + 21 * SECOND,
    sampled: 3,
    verified: 1,
    skipped: 0,
    failed: 2,
    failureCodes: [{ code: "crypto.decrypt_failed", count: 2 }],
    outcome: "failed",
  },
  {
    id: 10,
    sourceId: SOURCE_DOCUMENTS.id,
    startedAt: FIXED_NOW - 60 * HOUR,
    finishedAt: FIXED_NOW - 60 * HOUR + 2 * SECOND,
    sampled: 3,
    verified: 0,
    skipped: 3,
    failed: 0,
    failureCodes: [],
    outcome: "inconclusive",
  },
];

export const SCRUB_RUNS: ScrubRun[] = [
  {
    id: 7,
    sourceId: SOURCE_DOCUMENTS.id,
    startedAt: FIXED_NOW - 5 * HOUR,
    finishedAt: FIXED_NOW - 5 * HOUR + 42 * SECOND,
    checked: 500,
    ok: 499,
    missing: 0,
    sizeMismatch: 1,
    hashMismatch: 0,
    unverifiable: 0,
    healed: 1,
    healedBundleMembers: 0,
    unrecoverable: 0,
    deepChecked: 0,
    deepFailed: 0,
    wrapped: false,
    outcome: "drift",
  },
  {
    id: 6,
    sourceId: SOURCE_PHOTOS.id,
    startedAt: FIXED_NOW - 2 * HOUR,
    finishedAt: FIXED_NOW - 2 * HOUR + 18 * SECOND,
    checked: 500,
    ok: 500,
    missing: 0,
    sizeMismatch: 0,
    hashMismatch: 0,
    unverifiable: 0,
    healed: 0,
    healedBundleMembers: 0,
    unrecoverable: 0,
    deepChecked: 10,
    deepFailed: 0,
    wrapped: true,
    outcome: "clean",
  },
];

// --- Sync ---

export const SYNC_STATUS_IDLE: GlobalSyncStatus = {
  accounts: [
    { account_id: ACCOUNT_DRIVE.id, state: { state: "idle", last_run_at: FIXED_NOW - 5 * MINUTE } },
    { account_id: ACCOUNT_S3.id, state: { state: "idle", last_run_at: FIXED_NOW - 2 * HOUR } },
  ],
};

/** A run in flight, roughly 40% done - enough for the global progress bar to
 * render a determinate, clearly-partial fill. */
export const SYNC_STATUS_RUNNING: GlobalSyncStatus = {
  accounts: [
    {
      account_id: ACCOUNT_DRIVE.id,
      state: {
        state: "executing",
        progress: {
          files_done: 128,
          files_total: 320,
          bytes_done: 314_572_800,
          bytes_total: 786_432_000,
          trashes_done: 0,
          trashes_total: 3,
          errors: 0,
        },
      },
    },
    { account_id: ACCOUNT_S3.id, state: { state: "idle", last_run_at: FIXED_NOW - 2 * HOUR } },
  ],
};

// --- Pending-work queue (issue #303) ---

/** The common case: nothing queued, with the next scheduled scan armed - the
 * panel's empty state ("No pending work - next scheduled backup HH:MM"). */
export const WORK_QUEUE_IDLE: QueueSnapshot[] = [
  {
    account_id: ACCOUNT_DRIVE.id,
    running: null,
    running_cancelled: false,
    pending: [],
    next_scheduled_at: FIXED_NOW + 30 * MINUTE,
  },
];

/** A busy queue: one item running plus one of every pending kind, so a single
 * screenshot exercises every row glyph, title, and subtitle. */
export const WORK_QUEUE_BUSY: QueueSnapshot[] = [
  {
    account_id: ACCOUNT_DRIVE.id,
    running: {
      id: 1,
      kind: "manual",
      source_id: SOURCE_DOCUMENTS.id,
      enqueued_at: FIXED_NOW - 4 * MINUTE,
      tick: "manual",
    },
    running_cancelled: false,
    pending: [
      {
        id: 2,
        kind: "recovery",
        source_id: SOURCE_PHOTOS.id,
        enqueued_at: FIXED_NOW - 3 * MINUTE,
        tick: "scheduled",
      },
      {
        id: 3,
        kind: "watcher",
        source_id: SOURCE_PHOTOS.id,
        enqueued_at: FIXED_NOW - 2 * MINUTE,
        tick: "watcher",
      },
      {
        id: 4,
        kind: "scheduled",
        source_id: null,
        enqueued_at: FIXED_NOW - 1 * MINUTE,
        tick: "scheduled",
      },
    ],
    next_scheduled_at: FIXED_NOW + 30 * MINUTE,
  },
];

// --- Settings ---

/** `windows` and `macos` are null, matching what the backend reports on Linux -
 * the platform the authoritative baselines are generated on. Specs that want
 * the Windows or macOS panels override `get_settings` along with the user
 * agent. */
export const SETTINGS: SettingsDto = {
  global: {
    autoStartOnLogin: true,
    defaultConcurrentUploads: 4,
    adaptiveParallelismEnabled: true,
    bandwidthCapMbps: null,
    skipOnBattery: true,
    skipOnMetered: true,
    scanIntervalSecs: 900,
    deepVerifyIntervalSecs: 7 * 24 * 3600,
    ioPriority: "low",
    logLevel: "info",
    schedule: {
      enabled: false,
      startMinute: 0,
      endMinute: 0,
      days: [true, true, true, true, true, true, true],
      utcOffsetMinutes: 0,
    },
    preBackupHook: null,
    postBackupHook: null,
    hookTimeoutSecs: 60,
    meteredMode: "pause",
    meteredBandwidthCapMbps: null,
    customRootCaPath: null,
    proxyMode: "system",
    proxyUrl: null,
    pauseWhenOffline: true,
  },
  telemetry: {
    enabled: true,
    installId: "00000000-0000-4000-8000-000000000000",
    endpoint: "https://telemetry.example.com/v1/ping",
  },
  updater: { channel: "stable", checkIntervalSecs: 86400 },
  ui: { trayLeftClickOpens: "window", locale: "en-US", colorMode: "system" },
  windows: null,
  macos: null,
  bundleSmallFiles: true,
  scrub: { enabled: true, intervalSecs: 604800, sliceSize: 500, deepSample: 0 },
  drill: { enabled: true, intervalSecs: 2592000, sampleSize: 3 },
};

export const VSS_HELPER_STATUS: VssHelperStatus = {
  supported: false,
  elevated: false,
  helperEnabled: false,
  helperAlive: false,
  helperLaunchable: false,
  launchPending: false,
  launchDeclined: false,
  lockedFileBackupDegraded: false,
};

export const APFS_HELPER_STATUS: ApfsHelperStatus = {
  supported: false,
  helperEnabled: false,
  helperAlive: false,
  helperLaunchable: false,
  launchPending: false,
  launchDeclined: false,
  lockedFileBackupDegraded: false,
};

export const TELEMETRY_PREVIEW: TelemetryPreviewPayload = {
  install_id: "00000000-0000-4000-8000-000000000000",
  ts: FIXED_NOW,
  version: MOCK_APP_VERSION,
  os: "linux",
  os_version: "6.8.0",
  arch: "x86_64",
  channel: "stable",
  events_24h: { backup_done: 6, upload_done: 412, error: 1 },
  latency_p50_p95_ms: { upload: [84, 412], list: [21, 78] },
};

export const UPDATE_INFO: UpdateInfo = {
  version: "10.0.0",
  notes: "- Faster incremental scans\n- Fixed a restore crash on very long paths",
  publishedAt: "2026-03-14T09:30:00Z",
  channel: "stable",
};

export const RELEASES: ReleaseDto[] = [
  {
    version: "9.9.9",
    name: "Driven 9.9.9",
    notes: "### Features\n\n- Scripted IPC mock for the visual suite\n",
    publishedAt: "2026-03-10T12:00:00Z",
    url: "https://github.com/pmaxhogan/driven/releases/tag/v9.9.9",
  },
  {
    version: "9.9.8",
    name: "Driven 9.9.8",
    notes: "### Bug Fixes\n\n- Restore no longer stalls on a cancelled job\n",
    publishedAt: "2026-03-01T12:00:00Z",
    url: "https://github.com/pmaxhogan/driven/releases/tag/v9.9.8",
  },
];

// --- Sources / wizard helpers ---

export const DRIVE_FOLDER_LISTING: DriveFolderListing = {
  currentFolderId: "root",
  driveId: null,
  currentFolderPath: "/",
  folders: [
    { id: "folder-backups", name: "Driven Backups", driveId: null, isSharedDrive: false },
    { id: "folder-documents", name: "Documents", driveId: null, isSharedDrive: false },
    { id: "folder-team", name: "Team Drive", driveId: "drive-team", isSharedDrive: true },
  ],
};

export const EXCLUSION_PREVIEW: ExclusionPreview = {
  includedCount: 18_402,
  excludedCount: 1_240,
  includedBytes: 4_294_967_296,
  includedSample: ["notes/todo.md", "reports/2026-q1.pdf", "src/main.rs"],
  excludedSample: ["node_modules/left-pad/index.js", "build/tmp.o"],
  truncated: true,
};

export const VERSIONING_CONFIG: VersioningConfig = {
  enabled: true,
  countCap: 10,
  maxBytes: 104_857_600,
};

/** The 24 BIP39 words the recovery-phrase step reveals. Fixed, obviously fake
 * words - never a real generated phrase. */
export const RECOVERY_PHRASE: string[] = [
  "abandon",
  "ability",
  "able",
  "about",
  "above",
  "absent",
  "absorb",
  "abstract",
  "absurd",
  "abuse",
  "access",
  "accident",
  "account",
  "accuse",
  "achieve",
  "acid",
  "acoustic",
  "acquire",
  "across",
  "act",
  "action",
  "actor",
  "actress",
  "actual",
];

// --- Restore ---

export const REMOTE_TREE: RemoteTreeDto = {
  entries: [
    {
      relativePath: "reports",
      name: "reports",
      isDir: true,
      size: 0,
      status: null,
      restorable: false,
    },
    {
      relativePath: "invoices",
      name: "invoices",
      isDir: true,
      size: 0,
      status: null,
      restorable: false,
    },
    {
      relativePath: "budget-2026.xlsx",
      name: "budget-2026.xlsx",
      isDir: false,
      size: 284_672,
      status: "synced",
      restorable: true,
    },
    {
      relativePath: "notes.md",
      name: "notes.md",
      isDir: false,
      size: 4_096,
      status: "synced",
      restorable: true,
    },
    {
      relativePath: "presentation.key",
      name: "presentation.key",
      isDir: false,
      size: 18_874_368,
      status: "pending",
      restorable: false,
    },
    {
      relativePath: "archive.zip",
      name: "archive.zip",
      isDir: false,
      size: 1_073_741_824,
      status: "corrupt",
      restorable: false,
    },
    {
      relativePath: "vault.kdbx",
      name: "vault.kdbx",
      isDir: false,
      size: 65_536,
      status: "locked",
      restorable: true,
    },
  ],
  truncated: false,
};

export const REMOTE_TREE_EMPTY: RemoteTreeDto = { entries: [], truncated: false };

export const SEARCH_HITS: FileSearchHitDto[] = [
  {
    sourceId: SOURCE_DOCUMENTS.id,
    relativePath: "reports/2026-q1.pdf",
    status: "synced",
    restorable: true,
  },
  {
    sourceId: SOURCE_DOCUMENTS.id,
    relativePath: "reports/2025-q4.pdf",
    status: "synced",
    restorable: true,
  },
  {
    sourceId: SOURCE_PHOTOS.id,
    relativePath: "2026/03/IMG_0042.jpg",
    status: "synced",
    restorable: true,
  },
];

export const FILE_VERSIONS: FileVersionDto[] = [
  {
    size: 284_672,
    createdAt: FIXED_NOW - 2 * DAY,
    supersededAt: FIXED_NOW - 6 * HOUR,
    trashed: false,
  },
  {
    size: 271_360,
    createdAt: FIXED_NOW - 9 * DAY,
    supersededAt: FIXED_NOW - 2 * DAY,
    trashed: true,
  },
];

/** A restore job about two thirds through, with one file already failed - so
 * one screenshot covers the progress bar, the per-file states and the error
 * row. */
export const RESTORE_JOB_RUNNING: RestoreJobStatus = {
  jobId: "job-1",
  totalFiles: 4,
  completedFiles: 2,
  failedFiles: 1,
  totalBytes: 20_971_520,
  bytesDone: 13_631_488,
  currentFile: "reports/2026-q1.pdf",
  done: false,
  cancelled: false,
  files: [
    {
      relativePath: "notes.md",
      state: "done",
      bytesDone: 4_096,
      bytesTotal: 4_096,
      errorCode: null,
    },
    {
      relativePath: "budget-2026.xlsx",
      state: "done",
      bytesDone: 284_672,
      bytesTotal: 284_672,
      errorCode: null,
    },
    {
      relativePath: "reports/2026-q1.pdf",
      state: "restoring",
      bytesDone: 13_342_720,
      bytesTotal: 18_874_368,
      errorCode: null,
    },
    {
      relativePath: "archive.zip",
      state: "failed",
      bytesDone: 0,
      bytesTotal: 1_808_384,
      errorCode: "drive.checksum_mismatch",
    },
  ],
};

export const RESTORE_JOB_DONE: RestoreJobStatus = {
  jobId: "job-1",
  totalFiles: 4,
  completedFiles: 4,
  failedFiles: 0,
  totalBytes: 20_971_520,
  bytesDone: 20_971_520,
  currentFile: null,
  done: true,
  cancelled: false,
  files: RESTORE_JOB_RUNNING.files.map((f) => ({
    ...f,
    state: "done" as const,
    bytesDone: f.bytesTotal,
    errorCode: null,
  })),
};
