// Typed `listen` helpers, one per Rust -> webview event (SPEC s11.7). Each
// helper pins the canonical channel name (matching the constants in
// `src-tauri/src/events.rs`) and types the payload, returning the
// `UnlistenFn` the caller invokes on unmount. `@tauri-apps/api/event`'s
// `listen` is the seam vitest mocks.

import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type { ActivityEntry, GlobalSyncStatus, UpdateInfo } from "./types";

/** `sync:status_changed` payload: GlobalSyncStatus (SPEC s11.7). */
export function onSyncStatusChanged(
  handler: (status: GlobalSyncStatus) => void,
): Promise<UnlistenFn> {
  return listen<GlobalSyncStatus>("sync:status_changed", (e) =>
    handler(e.payload),
  );
}

/** `sync:source_progress` payload: { sourceId, progress } (SPEC s11.7). */
export interface SourceProgressPayload {
  sourceId: string;
  progress: unknown;
}

export function onSyncSourceProgress(
  handler: (payload: SourceProgressPayload) => void,
): Promise<UnlistenFn> {
  return listen<SourceProgressPayload>("sync:source_progress", (e) =>
    handler(e.payload),
  );
}

/** `activity:new` payload: ActivityEntry (SPEC s11.7). The Activity dashboard's
 * live tail subscribes to this and prepends new entries (deduped by id). */
export function onActivityNew(
  handler: (entry: ActivityEntry) => void,
): Promise<UnlistenFn> {
  return listen<ActivityEntry>("activity:new", (e) => handler(e.payload));
}

/** `account:needs_reauth` payload: { account_id, email } (SPEC s11.7). */
export interface NeedsReauthPayload {
  account_id: string;
  email: string;
}

export function onAccountNeedsReauth(
  handler: (payload: NeedsReauthPayload) => void,
): Promise<UnlistenFn> {
  return listen<NeedsReauthPayload>("account:needs_reauth", (e) =>
    handler(e.payload),
  );
}

/** `oauth:complete` payload: { session_id, status } (SPEC s11.7). */
export interface OAuthCompletePayload {
  session_id: string;
  status: unknown;
}

export function onOauthComplete(
  handler: (payload: OAuthCompletePayload) => void,
): Promise<UnlistenFn> {
  return listen<OAuthCompletePayload>("oauth:complete", (e) =>
    handler(e.payload),
  );
}

/** `updater:available` payload: UpdateInfo (SPEC s11.7). */
export function onUpdaterAvailable(
  handler: (info: UpdateInfo) => void,
): Promise<UnlistenFn> {
  return listen<UpdateInfo>("updater:available", (e) => handler(e.payload));
}

/** `updater:downloaded` payload: UpdateInfo (SPEC s11.7). */
export function onUpdaterDownloaded(
  handler: (info: UpdateInfo) => void,
): Promise<UnlistenFn> {
  return listen<UpdateInfo>("updater:downloaded", (e) => handler(e.payload));
}

/** `restore:progress` payload: RestoreJobStatus (typed in M8) (SPEC s11.7). */
export function onRestoreProgress(
  handler: (status: unknown) => void,
): Promise<UnlistenFn> {
  return listen<unknown>("restore:progress", (e) => handler(e.payload));
}
