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

/** R1-P2-1: a byte-carrying live event refreshes the DESIGN s8.3 header
 * aggregates, debounced by this many ms so an upload burst fires ONE reload
 * (its trailing edge) rather than one query per row. */
export const SUMMARY_REFRESH_DEBOUNCE_MS = 750;

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
  // The count of history pages loaded so far, zero-based (-1 = none yet): 0
  // after loadInitial, +1 per loadMore. R2-P1-2: this is now just a progress
  // counter (the actual walk is by KEYSET cursor, not a page index), kept so the
  // view + tests have a stable "how many pages in" signal.
  const loadedPage = ref(-1);
  // R2-P1-2: the KEYSET cursor for the NEXT history page - the `(ts, id)` of the
  // OLDEST history row loaded so far. `null` before any load / when history is
  // exhausted. loadMore pages strictly older than this cursor, so a row
  // prepended to activity_log between fetches can never shift / skip a page.
  const oldestCursor = ref<{ ts: number; id: number } | null>(null);
  // Total matching rows reported by the last query (for the count display).
  const total = ref(0);
  // Whether more history pages remain after the current cursor.
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
    () => !loading.value && entries.value.length === 0 && errorCode.value === null
  );

  /** Reset all accumulated state (entries, dedup index, paging, live tail). */
  function reset(): void {
    historyEntries.value = [];
    liveEntries.value = [];
    seenIds.clear();
    loadedPage.value = -1;
    oldestCursor.value = null;
    total.value = 0;
    hasMore.value = false;
    errorCode.value = null;
  }

  /** Insert `rows` at the END of history (older) honoring the dedup index, and
   * advance the keyset cursor / high-water mark. */
  function appendHistoryUnique(rows: ActivityEntry[]): void {
    for (const row of rows) {
      if (seenIds.has(row.id)) continue;
      seenIds.add(row.id);
      historyEntries.value.push(row);
    }
    // R2-P1-2: the cursor for the next page is the OLDEST row now held. Rows
    // arrive newest-first, so the last appended row is the oldest of this page.
    const last = rows.at(-1);
    if (last != null) oldestCursor.value = { ts: last.ts, id: last.id };
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

  /** Evict oldest live entries until the tail is within `LIVE_TAIL_CAP`,
   * dropping each evicted id from the dedup index unless it is also in loaded
   * history. The live tail is kept newest-first, so eviction is from the END. */
  function capLiveTail(): void {
    while (liveEntries.value.length > LIVE_TAIL_CAP) {
      const evicted = liveEntries.value.pop();
      if (evicted && !historyEntries.value.some((e) => e.id === evicted.id)) {
        seenIds.delete(evicted.id);
      }
    }
  }

  /** Prepend a live entry (it is the newest, from `activity:new`), capping the
   * live tail to `LIVE_TAIL_CAP`. */
  function pushLive(entry: ActivityEntry): void {
    seenIds.add(entry.id);
    liveEntries.value.unshift(entry);
    capLiveTail();
  }

  /** M7-R3-P2 (recheck-3): record an event type seen on a live / recovered row
   * into the filter dropdown source if it is not already there. Without this a
   * NEW event type that first appears live (or via lag reconcile) shows up in
   * the table but cannot be selected in the filter until a reload re-fetches
   * the distinct set. Keeps the option list sorted (the dropdown renders it as
   * the backend does). */
  function noteEventType(eventType: string): void {
    if (eventTypeOptions.value.includes(eventType)) return;
    eventTypeOptions.value = [...eventTypeOptions.value, eventType].sort();
  }

  /** R2-P1-1: merge reconciled `rows` (recovered durable rows that the live
   * broadcast dropped) into the live tail, keeping it strictly newest-first.
   * Unlike `pushLive`, recovered rows can be OLDER than rows already in the tail
   * (a ring-buffer drop evicts the OLDEST of a burst, so the recovered rows sit
   * below the latest delivered), so they must be INSERTED in sort order, not
   * blindly prepended. Dedup by id; then re-sort + cap. */
  function mergeRecoveredLive(rows: ActivityEntry[]): void {
    let added = false;
    for (const row of rows) {
      if (seenIds.has(row.id)) continue;
      seenIds.add(row.id);
      liveEntries.value.push(row);
      added = true;
    }
    if (!added) return;
    // Newest-first: ts desc, then id desc (the same total order the backend
    // keyset uses), so the rendered tail stays globally ordered.
    liveEntries.value.sort((a, b) => b.ts - a.ts || b.id - a.id);
    capLiveTail();
  }

  /** Load the first history page for the current filter (resets accumulation).
   * M7-P2-1: tokened so a concurrent filter change discards this response. */
  async function loadInitial(): Promise<void> {
    reset();
    const token = ++requestToken;
    const snapshot = { ...filter.value };
    loading.value = true;
    try {
      // R2-P1-2: the first page carries NO cursor (newest rows).
      const pageDto = await ipc.queryActivity(
        { ...snapshot },
        { beforeTs: null, beforeId: null, limit: ACTIVITY_PAGE_SIZE }
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
    const cursor = oldestCursor.value;
    // No cursor yet means loadInitial has not run; nothing older to page.
    if (cursor == null) return;
    const token = ++requestToken;
    const snapshot = { ...filter.value };
    loading.value = true;
    // M7-R3-P2 (recheck-3): clear any prior page-load error before a new attempt
    // so a successful retry does not keep showing a stale error banner. A fresh
    // failure below re-sets it; a success leaves it cleared.
    errorCode.value = null;
    try {
      // R2-P1-2: page strictly OLDER than the oldest row we hold (the cursor),
      // so a row prepended to activity_log between fetches never shifts a page.
      const pageDto = await ipc.queryActivity(
        { ...snapshot },
        { beforeTs: cursor.ts, beforeId: cursor.id, limit: ACTIVITY_PAGE_SIZE }
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
   * stays accurate for the live tail.
   *
   * R1-P2-1: a byte-carrying live event (an upload) feeds the DESIGN s8.3 header
   * aggregates, which would otherwise go stale while actively backing up. Such
   * an event schedules a debounced `loadSummary()` so "Uploaded today / this
   * week" + throughput refresh without a query per row. Done regardless of the
   * filter match (the summary aggregates ALL rows, not the filtered view). */
  function onLiveEvent(entry: ActivityEntry): void {
    if (entry.bytes != null && entry.bytes > 0) {
      scheduleSummaryRefresh();
    }
    // M7-R3-P2 (recheck-3): expose a brand-new event type to the filter dropdown
    // even when the row itself is filtered out of the current view, so the user
    // can then select it. Done before the dedup / filter gate below.
    noteEventType(entry.eventType);
    if (seenIds.has(entry.id)) return;
    if (!matchesFilter(entry)) return;
    pushLive(entry);
    total.value += 1;
  }

  /** M7-P1-1 / R1-P1-2 / R2-P1-1: reconcile from the durable `activity_log`
   * after a broadcast lag. Re-query the durable history (KEYSET) for the CURRENT
   * filter from the newest row forward and merge each NEW row into the live tail
   * (dedup by id), so rows dropped by the bounded broadcast are recovered into
   * the visible tail.
   *
   * R2-P1-1: walk by the keyset CURSOR over a bounded SCAN BUDGET and do NOT
   * early-stop on a zero-new page. The dropped rows can sit DEEPER than an
   * already-seen newest page (e.g. the newest rows arrived live but a middle
   * burst was dropped), so stopping on "this page recovered nothing new" (the
   * old recheck-2 P1 bug) could miss them. Instead page strictly older each step
   * and stop only when:
   *   - the durable history is exhausted (a short / empty page), OR
   *   - the scan budget is spent (sized from the dropped count, capped at
   *     LIVE_TAIL_CAP - the tail's hard bound).
   * Dedup is by id so a row already present (live or history) is never
   * double-counted.
   *
   * This does not disturb the paged history below; it only backfills the tail.
   * `total` is re-synced from the authoritative page total (NOT per-row bumped),
   * so reconciling rows that were already counted does not inflate the count.
   * R1-P2-1: a reconcile also refreshes the header aggregates, which can have
   * drifted during the lag burst.
   *
   * `skipped` (the dropped count) sizes the SCAN BUDGET: we page back enough
   * rows to cover the burst plus a page of overlap, capped at LIVE_TAIL_CAP, so
   * the walk recovers the whole burst (even deeper-than-newest drops) while
   * staying bounded. */
  async function reconcileFromHistory(skipped = 0): Promise<void> {
    const snapshot = { ...filter.value };
    // Scan budget (rows), bounded by the live-tail cap so a pathological lag
    // cannot loop unbounded: cover the dropped burst plus a page of overlap.
    const maxScan = Math.min(
      LIVE_TAIL_CAP,
      Math.max(ACTIVITY_PAGE_SIZE, skipped + ACTIVITY_PAGE_SIZE)
    );

    // Collect all NEW (not-yet-held, filter-matching) rows across the pages
    // FIRST, preserving global newest-first order. They are pushed at the end in
    // reverse (oldest first) so the newest overall lands at the FRONT of the live
    // tail - pushLive prepends, so pushing oldest-first yields newest-first.
    const recovered: ActivityEntry[] = [];
    let lastTotal: number | null = null;
    let cursor: { ts: number; id: number } | null = null;
    let scanned = 0;
    for (;;) {
      // Stop when the bounded scan budget is spent (defence-in-depth + the
      // small-lag fast path).
      if (scanned >= maxScan) break;

      let pageDto;
      try {
        pageDto = await ipc.queryActivity(
          { ...snapshot },
          {
            beforeTs: cursor?.ts ?? null,
            beforeId: cursor?.id ?? null,
            limit: ACTIVITY_PAGE_SIZE,
          }
        );
      } catch {
        // A failed reconcile is non-fatal: the next live event or a manual
        // reload re-syncs. Do not surface it as a page-load error.
        break;
      }
      // Drop the result if the filter changed while the reconcile was in flight.
      if (!sameFilter(snapshot, filter.value)) return;
      lastTotal = pageDto.total;
      scanned += pageDto.entries.length;

      for (const entry of pageDto.entries) {
        // M7-R3-P2 (recheck-3): record the event type for the filter dropdown
        // BEFORE the dedup / filter gate, so a type recovered via lag reconcile
        // becomes selectable even if this row is filtered out / already held.
        noteEventType(entry.eventType);
        // Skip rows already held (live or history), rows already staged this
        // reconcile, and rows outside the active filter.
        if (seenIds.has(entry.id)) continue;
        if (recovered.some((r) => r.id === entry.id)) continue;
        if (!matchesFilter(entry)) continue;
        recovered.push(entry);
      }

      // Advance the cursor to this page's oldest row for the next iteration.
      const oldest = pageDto.entries.at(-1);
      if (oldest != null) cursor = { ts: oldest.ts, id: oldest.id };

      // Stop once the durable history is exhausted (a short / empty page).
      if (!pageDto.hasMore || pageDto.entries.length === 0) break;
      // R2-P1-1: CRUCIALLY we do NOT stop on a zero-new page. The dropped rows
      // can sit DEEPER than an already-seen newest page, so the walk MUST
      // continue THROUGH all-seen pages until the scan budget (sized from the
      // dropped `skipped` count, capped at LIVE_TAIL_CAP) is spent or history is
      // exhausted. The old `recoveredThisPage === 0` early-break was the
      // recheck-2 P1 bug: it stopped before reaching the deeper dropped rows.
    }

    // R2-P1-1: merge the recovered rows in SORTED order (a recovered row can be
    // older than rows already in the tail - a ring-buffer drop evicts the oldest
    // of a burst - so they must be inserted in newest-first position, not
    // blindly prepended).
    mergeRecoveredLive(recovered);

    // Re-sync the count from the authoritative page total (the full match count
    // does not change across keyset pages).
    if (lastTotal != null) total.value = lastTotal;
    // R1-P2-1: refresh the header aggregates after a lag burst (best-effort).
    if (recovered.length > 0) void loadSummary();
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
    const dayStart = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
    // M7-R3-P2 (recheck-3): the week start MUST be local CALENDAR arithmetic,
    // not `dayStart - dayOfWeek * 24h`. Subtracting fixed 24h blocks crosses a
    // DST boundary wrong (a spring-forward / fall-back week is 23h or 25h on one
    // day), shifting "this week" off local midnight. Constructing the Date with
    // `getDate() - getDay()` lets the engine normalize to the correct local
    // midnight `dayOfWeek` days back (Sunday = 0), DST included.
    const weekStart = new Date(
      now.getFullYear(),
      now.getMonth(),
      now.getDate() - now.getDay()
    ).getTime();
    try {
      summary.value = await ipc.activitySummary(dayStart, weekStart, THROUGHPUT_WINDOW_MS);
    } catch {
      // Non-fatal: the header simply renders no aggregates if the summary fails.
      summary.value = null;
    }
  }

  // --- R1-P2-1: debounced header-aggregate refresh on live byte events ------
  // A burst of uploads must not fire one `loadSummary()` per row. Coalesce them
  // into a single trailing refresh `SUMMARY_REFRESH_DEBOUNCE_MS` after the last
  // byte-carrying live event.
  let summaryRefreshTimer: ReturnType<typeof setTimeout> | null = null;

  function scheduleSummaryRefresh(): void {
    if (summaryRefreshTimer != null) clearTimeout(summaryRefreshTimer);
    summaryRefreshTimer = setTimeout(() => {
      summaryRefreshTimer = null;
      void loadSummary();
    }, SUMMARY_REFRESH_DEBOUNCE_MS);
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
      onActivityLagged((payload) => {
        // R1-P1-2: pass the dropped count so the reconcile covers the gap.
        void reconcileFromHistory(payload.skipped);
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
    // R1-P2-1: cancel any pending debounced summary refresh on teardown.
    if (summaryRefreshTimer != null) {
      clearTimeout(summaryRefreshTimer);
      summaryRefreshTimer = null;
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
