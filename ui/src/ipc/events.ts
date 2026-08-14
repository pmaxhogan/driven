// Typed `listen` helpers, one per Rust -> webview event (SPEC s11.7). Each
// helper pins the canonical channel name (matching the constants in
// `src-tauri/src/events.rs`) and types the payload, returning the
// `UnlistenFn` the caller invokes on unmount. `@tauri-apps/api/event`'s
// `listen` is the seam vitest mocks.

import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type {
  AccountSyncStatus,
  ActivityEntry,
  ExclusionPreviewBatch,
  ExclusionPreviewDone,
  ExclusionPreviewError,
  ExecProgress,
  GlobalSyncStatus,
  PauseState,
  RestoreJobStatus,
  UpdateInfo,
  IoSample,
} from "./types";

/** The live payload of `sync:status_changed`. SPEC s11.7 documents the aggregate
 * `GlobalSyncStatus`, but the backend CURRENTLY emits a SINGLE-account snapshot
 * per orchestrator transition (src-tauri/src/assembly.rs `AccountSyncStatusEvent`
 * = `{ account_id, state }`, i.e. an `AccountSyncStatus`); the aggregate shape is
 * reserved for a later milestone. Consumers must handle BOTH and discriminate on
 * the presence of the `accounts` array. */
export type SyncStatusChangedPayload = GlobalSyncStatus | AccountSyncStatus;

/** `sync:status_changed` payload: a per-account `AccountSyncStatus` snapshot today
 * (the aggregate `GlobalSyncStatus` is reserved; see `SyncStatusChangedPayload`). */
export function onSyncStatusChanged(
  handler: (status: SyncStatusChangedPayload) => void
): Promise<UnlistenFn> {
  return listen<SyncStatusChangedPayload>("sync:status_changed", (e) => handler(e.payload));
}

/** `sync:pause_changed` payload: the active `PauseState`, or null once sync is
 * no longer manually paused. Emitted on pause, on resume, when a timed pause
 * auto-expires, and once at boot when a persisted pause is re-applied - so the
 * paused banner appears and disappears with no refresh. */
export function onPauseChanged(handler: (pause: PauseState | null) => void): Promise<UnlistenFn> {
  return listen<PauseState | null>("sync:pause_changed", (e) => handler(e.payload));
}

/** `sync:source_progress` payload: `{ account_id, source_id, progress }` (SPEC
 * s11.7; mirrors src-tauri/src/assembly.rs `SourceProgressEvent`). snake_case on
 * the wire like the rest of the M5 sync DTOs.
 *
 * One throttled execution-progress tick. The orchestrator enters `executing`
 * with a ZEROED `ExecProgress` and never re-emits that state for the source, so
 * these ticks are the ONLY carrier of the moving counters - the progress store
 * folds them over the state's embedded snapshot to keep the top-of-app bar
 * determinate. */
export interface SourceProgressPayload {
  account_id: string;
  source_id: string;
  progress: ExecProgress;
}

export function onSyncSourceProgress(
  handler: (payload: SourceProgressPayload) => void
): Promise<UnlistenFn> {
  return listen<SourceProgressPayload>("sync:source_progress", (e) => handler(e.payload));
}

/** `sync:io_throughput` payload: one live throughput sample (camelCase, the
 * same `IoSample` shape `io_throughput_series` returns). Emitted ~1/s while
 * any disk/network backup work is moving; suppressed (after one trailing
 * zero) while fully idle. */
export function onSyncIoThroughput(handler: (sample: IoSample) => void): Promise<UnlistenFn> {
  return listen<IoSample>("sync:io_throughput", (e) => handler(e.payload));
}

/** `activity:new` payload: ActivityEntry (SPEC s11.7). The Activity dashboard's
 * live tail subscribes to this and prepends new entries (deduped by id). */
export function onActivityNew(handler: (entry: ActivityEntry) => void): Promise<UnlistenFn> {
  return listen<ActivityEntry>("activity:new", (e) => handler(e.payload));
}

/** `activity:lagged` payload (mirrors src-tauri `emit_activity_lagged`): the
 * number of `activity:new` events the bounded broadcast dropped. */
export interface ActivityLaggedPayload {
  skipped: number;
}

/** `activity:lagged` - the live-tail broadcast lagged and dropped one or more
 * `activity:new` events (M7-P1-1 / R1-P1-2, SPEC s11.7). The payload carries the
 * dropped `skipped` count so the store can reconcile ENOUGH pages to cover the
 * gap (not just page 0): the activity store reconciles by re-querying the
 * durable `activity_log` and dedup-merging, so no durable row is lost. */
export function onActivityLagged(
  handler: (payload: ActivityLaggedPayload) => void
): Promise<UnlistenFn> {
  return listen<ActivityLaggedPayload>("activity:lagged", (e) => handler(e.payload));
}

/** `account:needs_reauth` payload: { account_id, email } (SPEC s11.7). */
export interface NeedsReauthPayload {
  account_id: string;
  email: string;
}

export function onAccountNeedsReauth(
  handler: (payload: NeedsReauthPayload) => void
): Promise<UnlistenFn> {
  return listen<NeedsReauthPayload>("account:needs_reauth", (e) => handler(e.payload));
}

/** `oauth:complete` payload: { session_id, status } (SPEC s11.7). */
export interface OAuthCompletePayload {
  session_id: string;
  status: unknown;
}

export function onOauthComplete(
  handler: (payload: OAuthCompletePayload) => void
): Promise<UnlistenFn> {
  return listen<OAuthCompletePayload>("oauth:complete", (e) => handler(e.payload));
}

/** `updater:available` payload: UpdateInfo (SPEC s11.7). */
export function onUpdaterAvailable(handler: (info: UpdateInfo) => void): Promise<UnlistenFn> {
  return listen<UpdateInfo>("updater:available", (e) => handler(e.payload));
}

/** `updater:download_progress` payload: { downloaded, total } (SPEC s15.2). M9a:
 * the in-app banner subscribes to render a progress bar while `installUpdate`
 * stages the update. `total` is null until the server reports a content length. */
export interface UpdaterDownloadProgressPayload {
  downloaded: number;
  total: number | null;
}

export function onUpdaterDownloadProgress(
  handler: (payload: UpdaterDownloadProgressPayload) => void
): Promise<UnlistenFn> {
  return listen<UpdaterDownloadProgressPayload>("updater:download_progress", (e) =>
    handler(e.payload)
  );
}

/** `updater:downloaded` payload: UpdateInfo (SPEC s11.7). */
export function onUpdaterDownloaded(handler: (info: UpdateInfo) => void): Promise<UnlistenFn> {
  return listen<UpdateInfo>("updater:downloaded", (e) => handler(e.payload));
}

/** `restore:progress` payload: RestoreJobStatus (SPEC s11.7). The Restore view's
 * store subscribes to this for live per-file + overall progress, errors, and the
 * terminal `done` state. */
export function onRestoreProgress(
  handler: (status: RestoreJobStatus) => void
): Promise<UnlistenFn> {
  return listen<RestoreJobStatus>("restore:progress", (e) => handler(e.payload));
}

/** `exclusion_preview:batch` payload: a slice of the streaming exclusion
 * preview's walk - the nodes discovered since the last batch plus the running
 * totals. The exclusion editor's tree subscribes to this and DISCARDS any batch
 * whose `previewId` is not the generation it is currently showing. */
export function onExclusionPreviewBatch(
  handler: (batch: ExclusionPreviewBatch) => void
): Promise<UnlistenFn> {
  return listen<ExclusionPreviewBatch>("exclusion_preview:batch", (e) => handler(e.payload));
}

/** `exclusion_preview:done` payload: the exact final totals of a streaming
 * exclusion preview, plus whether it was cancelled rather than completed. */
export function onExclusionPreviewDone(
  handler: (done: ExclusionPreviewDone) => void
): Promise<UnlistenFn> {
  return listen<ExclusionPreviewDone>("exclusion_preview:done", (e) => handler(e.payload));
}

/** `exclusion_preview:error` payload: a streaming preview that had already been
 * handed a generation id failed to set itself up (see `ExclusionPreviewError`).
 * Discarded, like a batch, when it is not the generation on screen. */
export function onExclusionPreviewError(
  handler: (error: ExclusionPreviewError) => void
): Promise<UnlistenFn> {
  return listen<ExclusionPreviewError>("exclusion_preview:error", (e) => handler(e.payload));
}
