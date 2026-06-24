import { defineStore } from "pinia";
import { computed, ref } from "vue";
import type { UnlistenFn } from "@tauri-apps/api/event";

import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
import { onActivityNew, onActivityLagged } from "../ipc/events";
import type {
  ActivityEntry,
  ActivityFilterDto,
  ActivityLevel,
  ActivitySummaryDto,
} from "../ipc/types";

/** Rows fetched per history page (SPEC s11.4; <= the backend's per-page cap). The
 * Activity dashboard accumulates pages client-side so it can scroll back through
 * 1000+ events WITHOUT re-querying earlier pages (M7 acceptance). */
export const ACTIVITY_PAGE_SIZE = 100;

/** M7-P2-2 (DESIGN s8.3 "last 1000 events"): the live tail is capped to this many
 * live-only entries; an error storm evicts the oldest live entry rather than
 * growing the store / DOM without bound. Explicitly loaded history pages are
 * NOT subject to this cap (they live in a separate list). */
export const LIVE_TAIL_CAP = 1000;

/** M7-P2-5: the recent-throughput window the header summary uses (ms). The
 * backend sums `activity_log.bytes` over this window; the UI divides by the
 * window seconds for a current bytes/sec rate. */
export const THROUGHPUT_WINDOW_MS = 60_000;

/** Severity rank for the `minLevel` client-side filter applied to live events
 * (info < warn < error), mirroring the backend `activity_level_rank`. */
const LEVEL_RANK: Record<ActivityLevel, number> = {
  info: 0,
  warn: 1,
  error: 2,
};

/**
 * Activity dashboard store (SPEC s11.4; DESIGN s8.3).
 *
 * Two separate lists feed the rendered, newest-first, de-duplicated view:
 * - `historyEntries`: persisted history paged in via `query_activity`. Pages
 *   APPEND (no re-query of earlier pages; M7 acceptance).
 * - `liveEntries`: the event-driven live tail (`activity:new` PREPENDS, reflected
 *   within 500ms; M7 acceptance), capped to `LIVE_TAIL_CAP` (M7-P2-2) by evicting
 *   the oldest live entry on overflow - so an error storm can never grow the
 *   store / DOM unbounded, while loaded history pages are preserved.
 *
 * The rendered `entries` is `liveEntries` (newest, deduped against history)
 * followed by `historyEntries`; both ingestion paths dedup by row id so a row
 * that arrives live and is later paged in (or vice versa) appears exactly once.
 *
 * M7-P1-1: on `activity:lagged` (the bounded broadcast dropped events) the store
 * RECONCILES from the durable `activity_log` - it re-queries page 0 and merges
 * the rows into the live tail (dedup by id), so no durable row is lost when the
 * live broadcast lags.
 *
 * M7-P2-1: `loadInitial` / `loadMore` / `applyFilter` carry a request token +
 * filter snapshot; a response is committed ONLY if the token + filter still match
 * the current state, so a filter change mid-flight discards the stale response.
 *
 * M7-P2-6: command errors are normalized to the stable `{ code }` shape (SPEC
 * s24) and exposed as `errorCode` for the view to localize via `t()`.
 */
export const useActivityStore = defineStore("activity", () => {
  // Persisted history, newest-first, appended page-by-page.
  const historyEntries = ref<ActivityEntry[]>([]);
  // The capped live tail, newest-first.
  const liveEntries = ref<ActivityEntry[]>([]);
  // The active filter (empty = match all).
  const filter = ref<ActivityFilterDto>({});
  // The highest zero-based history page index loaded so far (-1 = none yet).
  const loadedPage = ref(-1);
  // Total matching rows reported by the last query (for the count display).
  const total = ref(0);
  // Whether more history pages remain after `loadedPage`.
  const hasMore = ref(false);
  const loading = ref(false);
  // M7-P2-6: stable SPEC s24 code (null = no error); the view maps it via
  // t(`errors.${code}.long`).
  const errorCode = ref<string | null>(null);

  // M7-P2-4: the DISTINCT event types from the backend (filter dropdown source).
  const eventTypeOptions = ref<string[]>([]);
  // M7-P2-5: the header aggregate summary (null until first load).
  const summary = ref<ActivitySummaryDto | null>(null);

  // Membership index by row id so dedup is O(1) across both lists.
  const seenIds = new Set<number>();

  // M7-P2-1: the request generation. Bumped on every (re)load; a response whose
  // token is stale (a newer load started) or whose filter snapshot no longer
  // matches the current filter is discarded.
  let requestToken = 0;

  // The single rendered list: live tail (deduped against history) then history.
  const entries = computed<ActivityEntry[]>(() => {
    const historyIds = new Set(historyEntries.value.map((e) => e.id));
    const liveOnly = liveEntries.value.filter((e) => !historyIds.has(e.id));
    return [...liveOnly, ...historyEntries.value];
  });

  const isEmpty = computed(
    () =>
      !loading.value && entries.value.length === 0 && errorCode.value === null,
  );

  /** Reset all accumulated state (entries, dedup index, paging, live tail). */
  function reset(): void {
    historyEntries.value = [];
    liveEntries.value = [];
    seenIds.clear();
    loadedPage.value = -1;
    total.value = 0;
    hasMore.value = false;
    errorCode.value = null;
  }

  /** Insert `rows` at the END of history (older) honoring the dedup index. */
  function appendHistoryUnique(rows: ActivityEntry[]): void {
    for (const row of rows) {
      if (seenIds.has(row.id)) continue;
      seenIds.add(row.id);
      historyEntries.value.push(row);
    }
  }

  /** True when `entry` satisfies the active filter (so a live event below the
   * current filter is not shown out of sync with the paged history). */
  function matchesFilter(entry: ActivityEntry): boolean {
    const f = filter.value;
    if (f.sourceId != null && entry.sourceId !== f.sourceId) return false;
    if (f.minLevel != null && LEVEL_RANK[entry.level] < LEVEL_RANK[f.minLevel]) {
      return false;
    }
    if (f.sinceMs != null && entry.ts < f.sinceMs) return false;
    if (f.beforeMs != null && entry.ts >= f.beforeMs) return false;
    if (
      f.eventTypes != null &&
      f.eventTypes.length > 0 &&
      !f.eventTypes.includes(entry.eventType)
    ) {
      return false;
    }
    return true;
  }

  /** Prepend a live entry, capping the live tail to `LIVE_TAIL_CAP` by evicting
   * the oldest live entry (and dropping its id from the dedup index unless the
   * id is also present in loaded history). */
  function pushLive(entry: ActivityEntry): void {
    seenIds.add(entry.id);
    liveEntries.value.unshift(entry);
    while (liveEntries.value.length > LIVE_TAIL_CAP) {
      const evicted = liveEntries.value.pop();
      if (evicted && !historyEntries.value.some((e) => e.id === evicted.id)) {
        seenIds.delete(evicted.id);
      }
    }
  }

  /** Load the first history page for the current filter (resets accumulation).
   * M7-P2-1: tokened so a concurrent filter change discards this response. */
  async function loadInitial(): Promise<void> {
    reset();
    const token = ++requestToken;
    const snapshot = { ...filter.value };
    loading.value = true;
    try {
      const pageDto = await ipc.queryActivity(
        { ...snapshot },
        { page: 0, limit: ACTIVITY_PAGE_SIZE },
      );
      // Discard a stale response: a newer load started, or the filter changed.
      if (token !== requestToken || !sameFilter(snapshot, filter.value)) return;
      appendHistoryUnique(pageDto.entries);
      loadedPage.value = 0;
      total.value = pageDto.total;
      hasMore.value = pageDto.hasMore;
    } catch (e) {
      if (token !== requestToken) return;
      errorCode.value = toErrorCode(e);
    } finally {
      if (token === requestToken) loading.value = false;
    }
  }

  /** Load the NEXT history page and append it (no re-query of earlier pages).
   * A no-op when there is nothing more or a load is already in flight.
   * M7-P2-1: tokened so a filter change mid-flight discards this response. */
  async function loadMore(): Promise<void> {
    if (loading.value || !hasMore.value) return;
    const next = loadedPage.value + 1;
    const token = ++requestToken;
    const snapshot = { ...filter.value };
    loading.value = true;
    try {
      const pageDto = await ipc.queryActivity(
        { ...snapshot },
        { page: next, limit: ACTIVITY_PAGE_SIZE },
      );
      if (token !== requestToken || !sameFilter(snapshot, filter.value)) return;
      appendHistoryUnique(pageDto.entries);
      loadedPage.value = next;
      total.value = pageDto.total;
      hasMore.value = pageDto.hasMore;
    } catch (e) {
      if (token !== requestToken) return;
      errorCode.value = toErrorCode(e);
    } finally {
      if (token === requestToken) loading.value = false;
    }
  }

  /** Replace the filter and reload from page 0 (re-query). */
  async function applyFilter(next: ActivityFilterDto): Promise<void> {
    filter.value = { ...next };
    await loadInitial();
  }

  /** Ingest a live `activity:new` entry: prepend it (newest-first) if it matches
   * the active filter and is not already present. Bumps `total` so the count
   * stays accurate for the live tail. */
  function onLiveEvent(entry: ActivityEntry): void {
    if (seenIds.has(entry.id)) return;
    if (!matchesFilter(entry)) return;
    pushLive(entry);
    total.value += 1;
  }

  /** M7-P1-1: reconcile from the durable `activity_log` after a broadcast lag.
   * Re-query page 0 for the CURRENT filter and merge each NEW row into the live
   * tail (dedup by id), so rows dropped by the bounded broadcast are recovered.
   * This does not disturb the paged history below; it only backfills the tail.
   * `total` is re-synced from the authoritative page total (NOT per-row bumped),
   * so reconciling rows that were already counted does not inflate the count. */
  async function reconcileFromHistory(): Promise<void> {
    const snapshot = { ...filter.value };
    let pageDto;
    try {
      pageDto = await ipc.queryActivity(
        { ...snapshot },
        { page: 0, limit: ACTIVITY_PAGE_SIZE },
      );
    } catch {
      // A failed reconcile is non-fatal: the next live event or a manual reload
      // re-syncs. Do not surface it as a page-load error.
      return;
    }
    // Drop the result if the filter changed while the reconcile was in flight.
    if (!sameFilter(snapshot, filter.value)) return;
    // Merge oldest-first so the relative order of recovered rows is preserved
    // when each is prepended (newest ends up at the front). Prepend directly
    // (not via onLiveEvent) so `total` is not per-row bumped here.
    for (let i = pageDto.entries.length - 1; i >= 0; i--) {
      const entry = pageDto.entries[i];
      if (seenIds.has(entry.id)) continue;
      if (!matchesFilter(entry)) continue;
      pushLive(entry);
    }
    // Re-sync the count from the authoritative page total.
    total.value = pageDto.total;
  }

  /** M7-P2-4: load the DISTINCT event-type set from the backend (filter facets),
   * so the dropdown offers types from history, not just loaded rows. */
  async function loadEventTypeOptions(): Promise<void> {
    try {
      eventTypeOptions.value = await ipc.distinctActivityEventTypes();
    } catch {
      // Non-fatal: fall back to an empty option list (the dropdown still shows
      // "all event types"). A page-load error already surfaces the real failure.
      eventTypeOptions.value = [];
    }
  }

  /** M7-P2-5: load the DESIGN s8.3 header aggregates. Day / week boundaries are
   * computed from the LOCAL `Date` so "today" honours the user's timezone. */
  async function loadSummary(): Promise<void> {
    const now = new Date();
    const dayStart = new Date(
      now.getFullYear(),
      now.getMonth(),
      now.getDate(),
    ).getTime();
    // Start of the week = local midnight `dayOfWeek` days back (Sunday = 0).
    const weekStart = dayStart - now.getDay() * 24 * 60 * 60 * 1000;
    try {
      summary.value = await ipc.activitySummary(
        dayStart,
        weekStart,
        THROUGHPUT_WINDOW_MS,
      );
    } catch {
      // Non-fatal: the header simply renders no aggregates if the summary fails.
      summary.value = null;
    }
  }

  // --- live-tail subscription (M7-P2-3: no listener leak) -------------------
  // The unlisten handles; held so the view can stop the subscriptions on
  // unmount. `desiredSubscribed` tracks intent so an unsubscribe that races a
  // not-yet-resolved `listen()` still removes the listener once it arrives.
  let unlistenNew: UnlistenFn | null = null;
  let unlistenLagged: UnlistenFn | null = null;
  let desiredSubscribed = false;

  /** Subscribe to the `activity:new` live tail + the `activity:lagged` reconcile
   * signal (idempotent). M7-P2-3: if `unsubscribeLive` runs before `listen()`
   * resolves, the resolved unlisten is invoked immediately so no listener leaks.
   */
  async function subscribeLive(): Promise<void> {
    if (desiredSubscribed) return;
    desiredSubscribed = true;
    const [newUnlisten, laggedUnlisten] = await Promise.all([
      onActivityNew((entry) => onLiveEvent(entry)),
      onActivityLagged(() => {
        void reconcileFromHistory();
      }),
    ]);
    if (!desiredSubscribed) {
      // Unsubscribed before the listeners resolved: tear them down now.
      newUnlisten();
      laggedUnlisten();
      return;
    }
    unlistenNew = newUnlisten;
    unlistenLagged = laggedUnlisten;
  }

  /** Stop the live-tail subscriptions. M7-P2-3: clears the desired flag so a
   * still-pending `subscribeLive` tears its listeners down on resolution. */
  function unsubscribeLive(): void {
    desiredSubscribed = false;
    if (unlistenNew) {
      unlistenNew();
      unlistenNew = null;
    }
    if (unlistenLagged) {
      unlistenLagged();
      unlistenLagged = null;
    }
  }

  return {
    entries,
    filter,
    loadedPage,
    total,
    hasMore,
    loading,
    errorCode,
    eventTypeOptions,
    summary,
    isEmpty,
    loadInitial,
    loadMore,
    applyFilter,
    onLiveEvent,
    reconcileFromHistory,
    loadEventTypeOptions,
    loadSummary,
    subscribeLive,
    unsubscribeLive,
  };
});

/** Structural equality for two activity filters (M7-P2-1 stale-response guard).
 * Compares the scalar fields + the event-type set (order-insensitive). */
function sameFilter(a: ActivityFilterDto, b: ActivityFilterDto): boolean {
  if ((a.sourceId ?? null) !== (b.sourceId ?? null)) return false;
  if ((a.minLevel ?? null) !== (b.minLevel ?? null)) return false;
  if ((a.sinceMs ?? null) !== (b.sinceMs ?? null)) return false;
  if ((a.beforeMs ?? null) !== (b.beforeMs ?? null)) return false;
  const aTypes = a.eventTypes ?? [];
  const bTypes = b.eventTypes ?? [];
  if (aTypes.length !== bTypes.length) return false;
  const aSet = new Set(aTypes);
  for (const t of bTypes) if (!aSet.has(t)) return false;
  return true;
}
