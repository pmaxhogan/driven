import { defineStore } from "pinia";
import { computed, ref } from "vue";
import type { UnlistenFn } from "@tauri-apps/api/event";

import * as ipc from "../ipc/commands";
import { onActivityNew } from "../ipc/events";
import type {
  ActivityEntry,
  ActivityFilterDto,
  ActivityLevel,
} from "../ipc/types";

/** Rows fetched per history page (SPEC s11.4; <= the backend's per-page cap). The
 * Activity dashboard accumulates pages client-side so it can scroll back through
 * 1000+ events WITHOUT re-querying earlier pages (M7 acceptance). */
export const ACTIVITY_PAGE_SIZE = 100;

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
 * Holds the accumulated, newest-first, de-duplicated entry list plus the active
 * filter and paging cursor. Two ingestion paths converge into the SAME list:
 * - `loadInitial` / `loadMore` page the persisted history via `query_activity`
 *   and APPEND each page (no re-query of earlier pages);
 * - `onLiveEvent` PREPENDS a live `activity:new` entry (event-driven live tail,
 *   M7 acceptance: reflected within 500ms) - both dedup by row id so a row that
 *   arrives live and is later paged in (or vice versa) appears exactly once.
 *
 * `applyFilter` swaps the filter and reloads from page 0; a live event that does
 * not match the active filter is dropped so the tail stays consistent with the
 * history below it.
 */
export const useActivityStore = defineStore("activity", () => {
  // Accumulated entries, newest-first. The single source of truth the view
  // renders; both history pages and live events land here.
  const entries = ref<ActivityEntry[]>([]);
  // The active filter (empty = match all).
  const filter = ref<ActivityFilterDto>({});
  // The highest zero-based history page index loaded so far (-1 = none yet).
  const loadedPage = ref(-1);
  // Total matching rows reported by the last query (for the count display).
  const total = ref(0);
  // Whether more history pages remain after `loadedPage`.
  const hasMore = ref(false);
  const loading = ref(false);
  const error = ref<string | null>(null);

  // Membership index by row id so dedup is O(1) on both ingestion paths.
  const seenIds = new Set<number>();

  const isEmpty = computed(
    () => !loading.value && entries.value.length === 0 && error.value === null,
  );

  /** Reset all accumulated state (entries, dedup index, paging). */
  function reset(): void {
    entries.value = [];
    seenIds.clear();
    loadedPage.value = -1;
    total.value = 0;
    hasMore.value = false;
    error.value = null;
  }

  /** Insert `rows` at the END (older history) honoring the dedup index. */
  function appendUnique(rows: ActivityEntry[]): void {
    for (const row of rows) {
      if (seenIds.has(row.id)) continue;
      seenIds.add(row.id);
      entries.value.push(row);
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

  /** Load the first history page for the current filter (resets accumulation). */
  async function loadInitial(): Promise<void> {
    reset();
    loading.value = true;
    try {
      const pageDto = await ipc.queryActivity(
        { ...filter.value },
        { page: 0, limit: ACTIVITY_PAGE_SIZE },
      );
      appendUnique(pageDto.entries);
      loadedPage.value = 0;
      total.value = pageDto.total;
      hasMore.value = pageDto.hasMore;
    } catch (e) {
      error.value = String(e);
    } finally {
      loading.value = false;
    }
  }

  /** Load the NEXT history page and append it (no re-query of earlier pages).
   * A no-op when there is nothing more or a load is already in flight. */
  async function loadMore(): Promise<void> {
    if (loading.value || !hasMore.value) return;
    const next = loadedPage.value + 1;
    loading.value = true;
    try {
      const pageDto = await ipc.queryActivity(
        { ...filter.value },
        { page: next, limit: ACTIVITY_PAGE_SIZE },
      );
      appendUnique(pageDto.entries);
      loadedPage.value = next;
      total.value = pageDto.total;
      hasMore.value = pageDto.hasMore;
    } catch (e) {
      error.value = String(e);
    } finally {
      loading.value = false;
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
    seenIds.add(entry.id);
    entries.value.unshift(entry);
    total.value += 1;
  }

  // The live-tail unlisten handle; held so the view can stop the subscription on
  // unmount.
  let unlisten: UnlistenFn | null = null;

  /** Subscribe to the `activity:new` live tail (idempotent). */
  async function subscribeLive(): Promise<void> {
    if (unlisten) return;
    unlisten = await onActivityNew((entry) => onLiveEvent(entry));
  }

  /** Stop the live-tail subscription. */
  function unsubscribeLive(): void {
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
  }

  return {
    entries,
    filter,
    loadedPage,
    total,
    hasMore,
    loading,
    error,
    isEmpty,
    loadInitial,
    loadMore,
    applyFilter,
    onLiveEvent,
    subscribeLive,
    unsubscribeLive,
  };
});
