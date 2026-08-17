import { defineStore } from "pinia";
import { computed, ref } from "vue";
import type { UnlistenFn } from "@tauri-apps/api/event";

import * as ipc from "../ipc/commands";
import { onQueueChanged } from "../ipc/events";
import type { QueueSnapshot, WorkItem } from "../ipc/types";

/**
 * Pending-work queue store (issue #303). Owns the `queue:changed` subscription
 * (registered once at the app root in App.vue, like the progress and pause
 * stores) and exposes the flat, ordered list the top-bar panel renders plus the
 * badge count.
 *
 * Backups queue up from four places - crash recovery, watcher ticks, "Back up
 * now", and the scheduled timer - and until this existed they coalesced
 * invisibly: the user could not tell that anything was waiting, let alone cancel
 * it. The backend holds one queue PER ACCOUNT (items run one at a time per
 * account); this store merges them into one list for display, with each row
 * carrying the account it belongs to so a cancel goes to the right orchestrator.
 *
 * Source NAMES are resolved here rather than sent on the wire: the backend
 * queue deals in ids, and `list_sources` is the one place display names live.
 * A name we do not have simply renders as an unlabelled row - never a raw uuid.
 */

/** One row of the panel: a queue item, the account it belongs to, and whether
 * it is the running one. */
export interface QueueRow {
  /** The account whose orchestrator owns this item (needed to cancel it). */
  accountId: string;
  item: WorkItem;
  /** True for the item currently executing (at most one per account). */
  running: boolean;
  /** True while a cancelled running item drains its in-flight files. */
  draining: boolean;
  /** The source's display name, when we know it. */
  sourceName: string | null;
}

export const useWorkQueueStore = defineStore("workQueue", () => {
  /** The latest snapshot per account. Keyed by account id, replaced wholesale
   * per event - the backend sends the whole queue, never a delta, so a missed
   * event self-heals on the next one. */
  const snapshots = ref<Record<string, QueueSnapshot>>({});

  /** Source id -> display name, filled from `list_sources` (best-effort). */
  const sourceNames = ref<Record<string, string>>({});

  /** Fold one `queue:changed` payload (or one element of a hydrate) in. */
  function ingest(snapshot: QueueSnapshot): void {
    snapshots.value = { ...snapshots.value, [snapshot.account_id]: snapshot };
  }

  /** Replace every account's queue at once (the hydrate result, which is the
   * complete picture - an account missing from it has no queue any more). */
  function ingestAll(all: QueueSnapshot[]): void {
    const next: Record<string, QueueSnapshot> = {};
    for (const snapshot of all) next[snapshot.account_id] = snapshot;
    snapshots.value = next;
  }

  function nameOf(sourceId: string | null): string | null {
    if (sourceId === null) return null;
    return sourceNames.value[sourceId] ?? null;
  }

  /** Every account's queue, flattened for display: running items first, then
   * pending in the order they will run. Accounts are visited in a stable order
   * (their id) so rows do not jump around between events. */
  const rows = computed<QueueRow[]>(() => {
    const accountIds = Object.keys(snapshots.value).sort();
    const running: QueueRow[] = [];
    const pending: QueueRow[] = [];
    for (const accountId of accountIds) {
      const snapshot = snapshots.value[accountId];
      if (snapshot.running) {
        running.push({
          accountId,
          item: snapshot.running,
          running: true,
          draining: snapshot.running_cancelled,
          sourceName: nameOf(snapshot.running.source_id),
        });
      }
      for (const item of snapshot.pending) {
        pending.push({
          accountId,
          item,
          running: false,
          draining: false,
          sourceName: nameOf(item.source_id),
        });
      }
    }
    return [...running, ...pending];
  });

  /** The badge count: everything pending PLUS whatever is running - the number
   * you would want to know before closing the app. */
  const count = computed<number>(() => rows.value.length);

  /** Whether anything can be cleared (drives the "Clear all" affordance). */
  const clearable = computed<boolean>(() => rows.value.length > 0);

  /** The soonest armed scheduled scan across accounts (ms), or null when none
   * is armed. Drives the empty state's "next scheduled backup HH:MM". */
  const nextScheduledAt = computed<number | null>(() => {
    let soonest: number | null = null;
    for (const snapshot of Object.values(snapshots.value)) {
      const at = snapshot.next_scheduled_at;
      if (at === null) continue;
      if (soonest === null || at < soonest) soonest = at;
    }
    return soonest;
  });

  /** Cancel one item. Optimistic: the row disappears (or, for the running item,
   * flips to draining) at once, and the backend's `queue:changed` confirms it.
   * A failure re-hydrates rather than guessing, so the panel can never be left
   * showing a row the backend has already dropped. */
  async function cancel(accountId: string, itemId: number): Promise<void> {
    try {
      await ipc.cancelWorkItem(accountId, itemId);
    } catch (e) {
      console.error("cancel work item failed", e);
      await hydrate();
      throw e;
    }
  }

  /** Cancel everything pending and ask the running item to drain. */
  async function clearAll(): Promise<void> {
    try {
      await ipc.clearWorkQueue(null);
    } catch (e) {
      console.error("clear work queue failed", e);
      await hydrate();
      throw e;
    }
  }

  // --- event subscription (App.vue owns the app-lifetime registration) ------
  let unlisten: UnlistenFn | null = null;
  let desiredSubscribed = false;

  /** Subscribe to `queue:changed` (idempotent). */
  async function subscribe(): Promise<void> {
    if (desiredSubscribed) return;
    desiredSubscribed = true;
    try {
      const un = await onQueueChanged((snapshot) => ingest(snapshot));
      // unsubscribe() may have raced ahead while we awaited; honor it.
      if (!desiredSubscribed) {
        un();
        return;
      }
      unlisten = un;
    } catch (e) {
      // Reset so a later retry can re-subscribe; re-throw so the caller can log.
      desiredSubscribed = false;
      throw e;
    }
  }

  /** Seed from the backend's CURRENT queues (and the source names used to label
   * them) so work already waiting when the webview attaches shows immediately.
   * Best-effort in both halves: a failed name lookup still leaves a correct,
   * merely unlabelled, list. */
  async function hydrate(): Promise<void> {
    try {
      ingestAll(await ipc.getWorkQueue());
    } catch (e) {
      console.error("work queue hydrate failed", e);
    }
    try {
      const sources = await ipc.listSources();
      const names: Record<string, string> = {};
      for (const source of sources) names[source.id] = source.displayName;
      sourceNames.value = names;
    } catch (e) {
      console.error("work queue source-name hydrate failed", e);
    }
  }

  /** Stop the subscription. */
  function unsubscribe(): void {
    desiredSubscribed = false;
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
  }

  return {
    snapshots,
    sourceNames,
    rows,
    count,
    clearable,
    nextScheduledAt,
    ingest,
    ingestAll,
    cancel,
    clearAll,
    subscribe,
    hydrate,
    unsubscribe,
  };
});
