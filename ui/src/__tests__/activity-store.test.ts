import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { watch } from "vue";

// Activity store tests (SPEC s11.4; DESIGN s8.3). The seams are
// `@tauri-apps/api/core`'s `invoke` (every typed IPC wrapper routes through it)
// and `@tauri-apps/api/event`'s `listen` (the live-tail subscription). Mocking
// both lets us drive the store against a fake backend + manually fire live
// events, asserting: pagination appends without re-querying earlier pages, a
// live event prepends + dedups, filters re-query from page 0, and the empty
// state renders.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

// The live-tail seam: capture the handlers `onActivityNew` / `onActivityLagged`
// register so the test can fire `activity:new` + `activity:lagged` on demand.
// `listen` returns an unlisten fn. M7-P2-3: each `listen` call resolves to a
// DISTINCT unlisten so the unsubscribe-before-resolve test can assert teardown.
let liveHandler: ((payload: unknown) => void) | null = null;
let laggedHandler: ((payload: { skipped: number }) => void) | null = null;
const unlistenNewMock = vi.fn();
const unlistenLaggedMock = vi.fn();
// Allows a test to defer `listen` resolution (the leak-on-unmount race). Each
// blocked `listen` call parks its own resolver; `flushListen()` releases all.
let pendingResolvers: Array<() => void> = [];
let blockListen = false;
function flushListen(): void {
  const resolvers = pendingResolvers;
  pendingResolvers = [];
  for (const r of resolvers) r();
}
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, cb: (e: { payload: unknown }) => void) => {
    if (blockListen) {
      await new Promise<void>((res) => {
        pendingResolvers.push(res);
      });
    }
    if (event === "activity:new") {
      liveHandler = (payload: unknown) => cb({ payload });
      return unlistenNewMock;
    }
    if (event === "activity:lagged") {
      laggedHandler = (payload: { skipped: number }) => cb({ payload });
      return unlistenLaggedMock;
    }
    return vi.fn();
  }),
}));

import {
  useActivityStore,
  ACTIVITY_PAGE_SIZE,
  ACTIVITY_RENDER_WINDOW,
  LIVE_TAIL_CAP,
} from "../stores/activity";
import type { ActivityEntry } from "../ipc/types";

function makeEntry(over: Partial<ActivityEntry> = {}): ActivityEntry {
  return {
    id: 1,
    ts: 1000,
    sourceId: null,
    level: "info",
    eventType: "upload_done",
    fileCount: null,
    bytes: null,
    message: null,
    ...over,
  };
}

/** Build a KEYSET query_activity page DTO for `entries` (R2-P1-2). `hasMore`
 * follows the backend keyset rule (a full page MAY have more); the next cursor
 * is the last (oldest) row's `(ts, id)`. The `page` arg is kept only as a
 * caller-readability hint and is NOT part of the DTO. */
function makePage(
  entries: ActivityEntry[],
  _page: number,
  total: number,
  hasMoreOverride?: boolean
): {
  entries: ActivityEntry[];
  total: number;
  limit: number;
  hasMore: boolean;
  nextBeforeTs: number | null;
  nextBeforeId: number | null;
} {
  const last = entries.at(-1) ?? null;
  return {
    entries,
    total,
    limit: ACTIVITY_PAGE_SIZE,
    hasMore: hasMoreOverride ?? entries.length === ACTIVITY_PAGE_SIZE,
    nextBeforeTs: last?.ts ?? null,
    nextBeforeId: last?.id ?? null,
  };
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  unlistenNewMock.mockReset();
  unlistenLaggedMock.mockReset();
  liveHandler = null;
  laggedHandler = null;
  pendingResolvers = [];
  blockListen = false;
});

describe("activity store: pagination", () => {
  it("loadInitial fetches the first page (no cursor) and records paging metadata", async () => {
    const rows = [makeEntry({ id: 3 }), makeEntry({ id: 2 }), makeEntry({ id: 1 })];
    invokeMock.mockResolvedValueOnce(makePage(rows, 0, 250, true));
    const store = useActivityStore();
    await store.loadInitial();
    // R2-P1-2: the first page carries a null cursor.
    expect(invokeMock).toHaveBeenCalledWith("query_activity", {
      filter: {},
      page: { beforeTs: null, beforeId: null, limit: ACTIVITY_PAGE_SIZE },
    });
    expect(store.entries).toHaveLength(3);
    expect(store.entries[0].id).toBe(3);
    expect(store.total).toBe(250);
    expect(store.hasMore).toBe(true);
    expect(store.loadedPage).toBe(0);
  });

  it("loadMore pages by the oldest (ts,id) CURSOR, appending without re-querying", async () => {
    // Page 0: ids 200, 199 (oldest = id 199 @ ts 199).
    const page0 = [makeEntry({ id: 200, ts: 200 }), makeEntry({ id: 199, ts: 199 })];
    const page1 = [makeEntry({ id: 100, ts: 100 }), makeEntry({ id: 99, ts: 99 })];
    invokeMock.mockResolvedValueOnce(makePage(page0, 0, 250, true));
    const store = useActivityStore();
    await store.loadInitial();
    expect(invokeMock).toHaveBeenCalledTimes(1);

    invokeMock.mockResolvedValueOnce(makePage(page1, 1, 250, false));
    await store.loadMore();

    // Exactly ONE additional fetch, carrying the cursor = oldest row of page 0.
    expect(invokeMock).toHaveBeenCalledTimes(2);
    expect(invokeMock).toHaveBeenNthCalledWith(2, "query_activity", {
      filter: {},
      page: { beforeTs: 199, beforeId: 199, limit: ACTIVITY_PAGE_SIZE },
    });
    // Pages accumulated client-side, in order.
    expect(store.entries.map((e) => e.id)).toEqual([200, 199, 100, 99]);
    expect(store.loadedPage).toBe(1);
  });

  it("loadMore is a no-op when no more pages remain", async () => {
    invokeMock.mockResolvedValueOnce(makePage([makeEntry({ id: 1 })], 0, 1));
    const store = useActivityStore();
    await store.loadInitial();
    expect(store.hasMore).toBe(false);
    await store.loadMore();
    // No second fetch fired.
    expect(invokeMock).toHaveBeenCalledTimes(1);
  });

  it("loadMore dedups a row already present from the live tail", async () => {
    const page0 = [makeEntry({ id: 200, ts: 200 })];
    invokeMock.mockResolvedValueOnce(makePage(page0, 0, 250, true));
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();

    // A live event arrives for id 150 before it is paged in. Issue #45: live
    // events are buffered + coalesced, so flush to apply the burst before
    // asserting the rendered tail.
    liveHandler?.(makeEntry({ id: 150, ts: 900 }));
    store.flushLive();
    expect(store.entries.map((e) => e.id)).toEqual([150, 200]);

    // Page 1 includes id 150 again - it must NOT be duplicated.
    invokeMock.mockResolvedValueOnce(
      makePage([makeEntry({ id: 150, ts: 900 }), makeEntry({ id: 149, ts: 149 })], 1, 250, false)
    );
    await store.loadMore();
    const ids = store.entries.map((e) => e.id);
    expect(ids.filter((i) => i === 150)).toHaveLength(1);
    expect(ids).toEqual([150, 200, 149]);
  });
});

describe("activity store: live tail", () => {
  it("prepends a live event newest-first and bumps total", async () => {
    invokeMock.mockResolvedValueOnce(makePage([makeEntry({ id: 1 })], 0, 1));
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();
    expect(store.total).toBe(1);

    liveHandler?.(makeEntry({ id: 2, ts: 2000 }));
    // Issue #45: the burst is buffered; flush to apply it in one update.
    store.flushLive();
    expect(store.entries[0].id).toBe(2);
    expect(store.entries.map((e) => e.id)).toEqual([2, 1]);
    expect(store.total).toBe(2);
  });

  it("dedups a live event whose id is already present", async () => {
    invokeMock.mockResolvedValueOnce(makePage([makeEntry({ id: 5 })], 0, 1));
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();
    // Same id fired live: ignored.
    liveHandler?.(makeEntry({ id: 5 }));
    expect(store.entries).toHaveLength(1);
    expect(store.total).toBe(1);
  });

  it("drops a live event that does not match the active filter", async () => {
    invokeMock.mockResolvedValueOnce(makePage([], 0, 0));
    const store = useActivityStore();
    await store.subscribeLive();
    await store.applyFilter({ minLevel: "error" });
    // An info-level live event must be dropped under a min-level=error filter
    // (filtered out at ingest, never buffered).
    liveHandler?.(makeEntry({ id: 9, level: "info" }));
    expect(store.entries).toHaveLength(0);
    // A matching error-level event is kept (buffered, applied on flush).
    liveHandler?.(makeEntry({ id: 10, level: "error" }));
    store.flushLive();
    expect(store.entries.map((e) => e.id)).toEqual([10]);
  });

  it("unsubscribeLive calls BOTH unlisten fns (new + lagged)", async () => {
    const store = useActivityStore();
    await store.subscribeLive();
    store.unsubscribeLive();
    expect(unlistenNewMock).toHaveBeenCalledTimes(1);
    expect(unlistenLaggedMock).toHaveBeenCalledTimes(1);
  });

  // M7-P2-3: unsubscribe-before-resolve must not leak a listener.
  it("tears down listeners that resolve AFTER unsubscribe (no leak)", async () => {
    blockListen = true;
    const store = useActivityStore();
    // Start subscribing; `listen` is blocked, so it has not resolved yet.
    const pending = store.subscribeLive();
    // The view unmounts before the listeners resolve.
    store.unsubscribeLive();
    // Now let the blocked `listen` calls resolve.
    blockListen = false;
    flushListen();
    await pending;
    // Both resolved unlisten fns were invoked immediately on arrival.
    expect(unlistenNewMock).toHaveBeenCalledTimes(1);
    expect(unlistenLaggedMock).toHaveBeenCalledTimes(1);
  });
});

describe("activity store: lag reconcile (M7-P1-1)", () => {
  it("activity:lagged re-queries page 0 and merges dropped rows without duplicates", async () => {
    // Initial page 0 has rows 5 and 4.
    invokeMock.mockResolvedValueOnce(
      makePage([makeEntry({ id: 5, ts: 500 }), makeEntry({ id: 4, ts: 400 })], 0, 2)
    );
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();
    expect(store.entries.map((e) => e.id)).toEqual([5, 4]);

    // A burst happened and the live broadcast lagged: the durable log now also
    // has rows 7 and 6 (dropped from the live tail). The reconcile re-query
    // returns the newest page including the already-present 5 + the new 7, 6.
    // A small lag (2 dropped) still fits in page 0, so only ONE page is queried.
    invokeMock.mockResolvedValueOnce(
      makePage(
        [
          makeEntry({ id: 7, ts: 700 }),
          makeEntry({ id: 6, ts: 600 }),
          makeEntry({ id: 5, ts: 500 }),
        ],
        0,
        4
      )
    );
    laggedHandler?.({ skipped: 2 });
    // Let the async reconcile settle.
    await Promise.resolve();
    await Promise.resolve();

    const ids = store.entries.map((e) => e.id);
    // No duplicate of id 5; the dropped 7 + 6 are recovered, newest-first.
    expect(ids).toEqual([7, 6, 5, 4]);
    expect(ids.filter((i) => i === 5)).toHaveLength(1);
  });

  // R1-P1-2: a lag with skipped > ACTIVITY_PAGE_SIZE must reconcile MULTIPLE
  // pages (up to LIVE_TAIL_CAP), not just page 0, so every missing durable row
  // lands in the visible tail with no duplicates.
  it("recovers ALL dropped rows across pages when skipped exceeds one page", async () => {
    // Seed an empty tail (no history loaded; subscribe only).
    const store = useActivityStore();
    await store.subscribeLive();

    // The durable log holds 250 rows (ids 250..1, newest-first). A burst dropped
    // ~150 events from the live broadcast (skipped > ACTIVITY_PAGE_SIZE = 100),
    // so the reconcile must walk enough pages to cover the gap.
    const allRows: ActivityEntry[] = [];
    for (let id = 250; id >= 1; id--) {
      allRows.push(makeEntry({ id, ts: id }));
    }
    const pageOf = (p: number) =>
      makePage(
        allRows.slice(p * ACTIVITY_PAGE_SIZE, (p + 1) * ACTIVITY_PAGE_SIZE),
        p,
        allRows.length
      );
    // Queue page 0, 1, 2 in order (the reconcile pages forward until the gap is
    // covered / history exhausted).
    invokeMock.mockResolvedValueOnce(pageOf(0));
    invokeMock.mockResolvedValueOnce(pageOf(1));
    invokeMock.mockResolvedValueOnce(pageOf(2));

    // Call the reconcile directly (the lagged handler fires it as fire-and-
    // forget; awaiting it keeps the multi-page walk deterministic in the test).
    await store.reconcileFromHistory(150);

    const ids = store.entries.map((e) => e.id);
    // All 250 durable rows recovered, newest-first, no duplicates.
    expect(ids).toHaveLength(250);
    expect(new Set(ids).size).toBe(250);
    expect(ids[0]).toBe(250);
    expect(ids[ids.length - 1]).toBe(1);
    expect(store.total).toBe(250);
    // The reconcile queried at least 3 pages (skipped 150 -> target 250 rows).
    expect(
      invokeMock.mock.calls.filter((c) => c[0] === "query_activity").length
    ).toBeGreaterThanOrEqual(3);
  });

  // R2-P1-1: the recheck-2 P1 - a multi-page burst where the NEWEST page is
  // ALREADY in seenIds but the dropped rows sit DEEPER (page 1+). The old code
  // broke on `recoveredThisPage === 0` at the newest page and never reached the
  // deeper dropped rows. The keyset walk must NOT early-stop on a zero-new page.
  it("recovers DEEPER dropped rows even when the newest page is already seen", async () => {
    const store = useActivityStore();
    await store.subscribeLive();

    // The durable log holds 250 rows (ids 250..1, newest-first). The NEWEST 100
    // (ids 250..151) already arrived live, so they are all in seenIds. A burst
    // then dropped rows DEEPER in the log (ids 150..1 - page 1 and page 2).
    const allRows: ActivityEntry[] = [];
    for (let id = 250; id >= 1; id--) {
      allRows.push(makeEntry({ id, ts: id }));
    }
    // Pre-seed the newest page into the live tail (these are "already seen").
    for (const row of allRows.slice(0, ACTIVITY_PAGE_SIZE)) {
      liveHandler?.(row);
    }
    store.flushLive();
    expect(store.entries).toHaveLength(ACTIVITY_PAGE_SIZE);

    const pageOf = (p: number) =>
      makePage(
        allRows.slice(p * ACTIVITY_PAGE_SIZE, (p + 1) * ACTIVITY_PAGE_SIZE),
        p,
        allRows.length
      );
    // The reconcile re-queries from the newest page forward. Page 0 is ALL seen
    // (zero new), so a zero-new early-stop would miss pages 1 + 2.
    invokeMock.mockResolvedValueOnce(pageOf(0));
    invokeMock.mockResolvedValueOnce(pageOf(1));
    invokeMock.mockResolvedValueOnce(pageOf(2));

    await store.reconcileFromHistory(150);

    const ids = store.entries.map((e) => e.id);
    // All 250 rows present (the 100 already-seen + the 150 deeper recovered),
    // newest-first, no duplicates.
    expect(ids).toHaveLength(250);
    expect(new Set(ids).size).toBe(250);
    expect(ids[0]).toBe(250);
    expect(ids[ids.length - 1]).toBe(1);
    // It did NOT stop after the all-seen newest page: it walked >= 3 pages.
    expect(
      invokeMock.mock.calls.filter((c) => c[0] === "query_activity").length
    ).toBeGreaterThanOrEqual(3);
  });
});

describe("activity store: live-tail cap (M7-P2-2)", () => {
  it("caps the live tail to LIVE_TAIL_CAP, evicting oldest live events", async () => {
    invokeMock.mockResolvedValueOnce(makePage([], 0, 0));
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();

    // Push CAP + 50 live events (ids 1..CAP+50, ascending ts).
    const overflow = 50;
    for (let i = 1; i <= LIVE_TAIL_CAP + overflow; i++) {
      liveHandler?.(makeEntry({ id: i, ts: i }));
    }
    // Issue #45: apply the buffered burst, then assert the bound holds.
    store.flushLive();
    // The store is bounded to the cap (oldest live entries evicted).
    expect(store.entries).toHaveLength(LIVE_TAIL_CAP);
    // Newest is the last pushed; the oldest retained is id overflow+1.
    expect(store.entries[0].id).toBe(LIVE_TAIL_CAP + overflow);
    expect(store.entries[store.entries.length - 1].id).toBe(overflow + 1);
  });

  it("does NOT evict explicitly loaded history pages", async () => {
    // One history row (id 1). Then flood the live tail past the cap.
    invokeMock.mockResolvedValueOnce(makePage([makeEntry({ id: 1, ts: 1 })], 0, 1));
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();

    for (let i = 2; i <= LIVE_TAIL_CAP + 100; i++) {
      liveHandler?.(makeEntry({ id: i, ts: i }));
    }
    store.flushLive();
    // Live tail capped at CAP, but the loaded history row survives at the tail.
    expect(store.entries.length).toBe(LIVE_TAIL_CAP + 1);
    expect(store.entries[store.entries.length - 1].id).toBe(1);
  });
});

describe("activity store: filters", () => {
  it("applyFilter re-queries from page 0 with the new filter and resets state", async () => {
    invokeMock.mockResolvedValueOnce(makePage([makeEntry({ id: 1 }), makeEntry({ id: 2 })], 0, 2));
    const store = useActivityStore();
    await store.loadInitial();
    expect(store.entries).toHaveLength(2);

    invokeMock.mockResolvedValueOnce(makePage([makeEntry({ id: 3, level: "error" })], 0, 1));
    await store.applyFilter({ minLevel: "error", sourceId: "src-1" });

    expect(invokeMock).toHaveBeenNthCalledWith(2, "query_activity", {
      filter: { minLevel: "error", sourceId: "src-1" },
      page: { beforeTs: null, beforeId: null, limit: ACTIVITY_PAGE_SIZE },
    });
    // Old rows cleared; only the re-queried page remains.
    expect(store.entries.map((e) => e.id)).toEqual([3]);
    expect(store.loadedPage).toBe(0);
  });
});

describe("activity store: empty state", () => {
  it("isEmpty is true after loading an empty page", async () => {
    invokeMock.mockResolvedValueOnce(makePage([], 0, 0));
    const store = useActivityStore();
    await store.loadInitial();
    expect(store.entries).toHaveLength(0);
    expect(store.isEmpty).toBe(true);
    expect(store.errorCode).toBeNull();
  });

  it("isEmpty is false when an error occurred", async () => {
    invokeMock.mockRejectedValueOnce(new Error("db locked"));
    const store = useActivityStore();
    await store.loadInitial();
    // M7-P2-6: a plain Error (no `.code`) normalizes to internal.bug.
    expect(store.errorCode).toBe("internal.bug");
    expect(store.isEmpty).toBe(false);
  });
});

describe("activity store: coded errors (M7-P2-6)", () => {
  it("surfaces the stable SPEC s24 code from a Tauri object error", async () => {
    invokeMock.mockRejectedValueOnce({
      code: "state.db_locked",
      message: "Driven's database is briefly locked",
    });
    const store = useActivityStore();
    await store.loadInitial();
    expect(store.errorCode).toBe("state.db_locked");
  });
});

describe("activity store: request token (M7-P2-1)", () => {
  it("discards a stale page response after the filter changed mid-flight", async () => {
    const store = useActivityStore();
    await store.subscribeLive();

    // First load (default filter) is slow to resolve; capture its resolver.
    let resolveFirst: (v: unknown) => void = () => {};
    invokeMock.mockImplementationOnce(
      () =>
        new Promise((res) => {
          resolveFirst = res;
        })
    );
    const firstLoad = store.loadInitial();

    // While in flight, the user applies an error-only filter, which re-queries.
    invokeMock.mockResolvedValueOnce(makePage([makeEntry({ id: 99, level: "error" })], 0, 1));
    await store.applyFilter({ minLevel: "error" });
    expect(store.entries.map((e) => e.id)).toEqual([99]);

    // Now the STALE first response (default-filter rows) resolves - it must be
    // discarded, not appended over the current filtered result.
    resolveFirst(makePage([makeEntry({ id: 1 }), makeEntry({ id: 2 })], 0, 250));
    await firstLoad;

    expect(store.entries.map((e) => e.id)).toEqual([99]);
    expect(store.total).toBe(1);
    expect(store.loadedPage).toBe(0);
  });

  it("does NOT double-count total when a live event arrives during loadInitial (issue #45 codex P2)", async () => {
    const store = useActivityStore();
    await store.subscribeLive();

    // loadInitial's query is slow; capture its resolver so we can inject a live
    // event while the page is still in flight.
    let resolvePage: (v: unknown) => void = () => {};
    invokeMock.mockImplementationOnce(
      () =>
        new Promise((res) => {
          resolvePage = res;
        })
    );
    const load = store.loadInitial();

    // A live `activity:new` lands while the page query is awaiting; its durable
    // row is already part of the backend's authoritative total below.
    liveHandler!(makeEntry({ id: 7, ts: 5000 }));

    // The page shows 2 of 10 total rows; the live row (id 7) is one of the other
    // 8 the backend already counted in `total: 10`.
    resolvePage(makePage([makeEntry({ id: 6, ts: 600 }), makeEntry({ id: 5, ts: 500 })], 0, 10));
    await load;
    store.flushLive();

    // The authoritative server total must win - NOT total + the live delta (11).
    expect(store.total).toBe(10);
    // ...and the buffered live row is not lost from the tail.
    expect(store.entries.map((e) => e.id)).toContain(7);
  });
});

describe("activity store: backend facets + summary (M7-P2-4, P2-5)", () => {
  it("loadEventTypeOptions populates the dropdown source from the backend", async () => {
    invokeMock.mockResolvedValueOnce(["paused", "scan_done", "upload_done"]);
    const store = useActivityStore();
    await store.loadEventTypeOptions();
    expect(invokeMock).toHaveBeenCalledWith("distinct_activity_event_types", undefined);
    expect(store.eventTypeOptions).toEqual(["paused", "scan_done", "upload_done"]);
  });

  it("loadSummary stores the header aggregates", async () => {
    const summary = {
      bytesToday: 1024,
      bytesWeek: 4096,
      fileStatusCounts: [{ status: "synced", count: 3 }],
      throughputWindowBytes: 512,
      throughputWindowMs: 60000,
    };
    invokeMock.mockResolvedValueOnce(summary);
    const store = useActivityStore();
    await store.loadSummary();
    expect(invokeMock).toHaveBeenCalledWith(
      "activity_summary",
      expect.objectContaining({ throughputWindowMs: 60000 })
    );
    expect(store.summary).toEqual(summary);
  });

  // R1-P2-1: a byte-carrying live event (an upload) refreshes the header
  // aggregates (debounced), so "Uploaded today / this week" + throughput do not
  // go stale while actively backing up. A burst coalesces into ONE reload.
  it("refreshes the summary (debounced) on a byte-carrying live event", async () => {
    vi.useFakeTimers();
    try {
      const store = useActivityStore();
      await store.subscribeLive();

      const summary = {
        bytesToday: 2048,
        bytesWeek: 2048,
        fileStatusCounts: [],
        throughputWindowBytes: 2048,
        throughputWindowMs: 60000,
      };
      // Every activity_summary call resolves with the same summary.
      invokeMock.mockImplementation((cmd: string) =>
        cmd === "activity_summary" ? Promise.resolve(summary) : Promise.resolve(undefined)
      );

      // Fire a burst of byte-carrying upload events.
      for (let i = 1; i <= 5; i++) {
        liveHandler?.(makeEntry({ id: i, ts: i, eventType: "upload_done", bytes: 512 }));
      }
      // No summary call yet (debounce pending).
      expect(invokeMock.mock.calls.filter((c) => c[0] === "activity_summary").length).toBe(0);

      // Advance past the debounce window: exactly ONE summary reload fires.
      await vi.advanceTimersByTimeAsync(1000);
      await Promise.resolve();
      const summaryCalls = invokeMock.mock.calls.filter((c) => c[0] === "activity_summary").length;
      expect(summaryCalls).toBe(1);
      expect(store.summary).toEqual(summary);
    } finally {
      vi.useRealTimers();
    }
  });

  // R1-P2-1: a NON-byte live event (e.g. an error row) must NOT trigger a
  // summary refresh (it does not change the byte aggregates).
  it("does NOT refresh the summary on a live event without bytes", async () => {
    vi.useFakeTimers();
    try {
      const store = useActivityStore();
      await store.subscribeLive();
      invokeMock.mockResolvedValue(undefined);

      liveHandler?.(makeEntry({ id: 1, ts: 1, eventType: "drive.checksum_mismatch", bytes: null }));
      await vi.advanceTimersByTimeAsync(2000);
      expect(invokeMock.mock.calls.filter((c) => c[0] === "activity_summary").length).toBe(0);
      void store;
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("activity store: recheck-3 polish (M7-R3-P2)", () => {
  it("computes the week start by local calendar arithmetic (DST-safe)", async () => {
    // M7-R3-P2: weekStart must be local-midnight `dayOfWeek` days back via Date
    // construction, NOT `dayStart - dayOfWeek * 24h` (which crosses DST wrong).
    // Assert the passed weekStart equals the calendar value for a fixed `now`.
    const fixedNow = new Date(2026, 2, 11, 15, 30, 0); // Wed 2026-03-11 (US DST week)
    vi.useFakeTimers();
    vi.setSystemTime(fixedNow);
    try {
      invokeMock.mockResolvedValueOnce({
        bytesToday: 0,
        bytesWeek: 0,
        fileStatusCounts: [],
        throughputWindowBytes: 0,
        throughputWindowMs: 60000,
      });
      const store = useActivityStore();
      await store.loadSummary();

      const dayStart = new Date(2026, 2, 11).getTime();
      const weekStart = new Date(2026, 2, 11 - fixedNow.getDay()).getTime();
      expect(invokeMock).toHaveBeenCalledWith("activity_summary", {
        dayStartMs: dayStart,
        weekStartMs: weekStart,
        throughputWindowMs: 60000,
      });
      // The calendar weekStart is NOT necessarily dayStart - day*24h across a
      // DST boundary; assert it is a true local midnight (no sub-day remainder).
      const wk = new Date(weekStart);
      expect(wk.getHours()).toBe(0);
      expect(wk.getMinutes()).toBe(0);
      expect(wk.getSeconds()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it("adds a new event type seen on a live row to the filter dropdown source", async () => {
    invokeMock.mockResolvedValueOnce(["upload_done"]);
    const store = useActivityStore();
    await store.loadEventTypeOptions();
    await store.subscribeLive();
    expect(store.eventTypeOptions).toEqual(["upload_done"]);

    // A brand-new event type arrives live; it must appear in the dropdown source
    // (sorted) so the user can filter for it without a reload.
    liveHandler?.(makeEntry({ id: 2, ts: 2000, eventType: "trash_done" }));
    expect(store.eventTypeOptions).toEqual(["trash_done", "upload_done"]);

    // A live row of an already-known type does not duplicate the option.
    liveHandler?.(makeEntry({ id: 3, ts: 3000, eventType: "upload_done" }));
    expect(store.eventTypeOptions).toEqual(["trash_done", "upload_done"]);
  });

  it("exposes a new event type even when the live row is filtered out", async () => {
    invokeMock.mockResolvedValueOnce(makePage([], 0, 0));
    const store = useActivityStore();
    // Filter to errors only.
    await store.applyFilter({ minLevel: "error" });
    await store.subscribeLive();

    // An INFO row of a new type is filtered out of the view but its type must
    // still become selectable in the dropdown.
    liveHandler?.(makeEntry({ id: 9, ts: 9000, level: "info", eventType: "deep_verify_done" }));
    expect(store.entries).toHaveLength(0);
    expect(store.eventTypeOptions).toContain("deep_verify_done");
  });

  it("clears a prior loadMore error after a successful retry", async () => {
    // Page 0 loads, hasMore=true.
    invokeMock.mockResolvedValueOnce(makePage([makeEntry({ id: 200, ts: 200 })], 0, 250, true));
    const store = useActivityStore();
    await store.loadInitial();
    expect(store.errorCode).toBeNull();

    // First loadMore FAILS -> errorCode set.
    invokeMock.mockRejectedValueOnce({ code: "state.db_locked" });
    await store.loadMore();
    expect(store.errorCode).toBe("state.db_locked");
    // hasMore is unchanged by a failed load, so a retry is possible.
    expect(store.hasMore).toBe(true);

    // Retry SUCCEEDS -> the stale error must be cleared.
    invokeMock.mockResolvedValueOnce(makePage([makeEntry({ id: 100, ts: 100 })], 1, 250, false));
    await store.loadMore();
    expect(store.errorCode).toBeNull();
    expect(store.entries.map((e) => e.id)).toEqual([200, 100]);
  });
});

describe("activity store: batched live ingestion (issue #45)", () => {
  it("buffers a burst and applies it in ONE reactive update to entries AND total", async () => {
    invokeMock.mockResolvedValueOnce(makePage([], 0, 0));
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();

    // Count how many times the rendered list and the total actually change.
    // `flush: "sync"` fires the watcher on every reactive mutation, so a per-event
    // mutation (the pre-fix behavior) would push N entries here, not 1.
    const entriesUpdates: number[] = [];
    const totalUpdates: number[] = [];
    const stopEntries = watch(
      () => store.entries.length,
      (len) => entriesUpdates.push(len),
      { flush: "sync" }
    );
    const stopTotal = watch(
      () => store.total,
      (t) => totalUpdates.push(t),
      { flush: "sync" }
    );

    const N = 50;
    for (let i = 1; i <= N; i++) {
      liveHandler?.(makeEntry({ id: i, ts: i, eventType: "upload_done", bytes: null }));
    }

    // The whole burst is buffered: NOT yet reflected in the reactive state.
    expect(entriesUpdates).toHaveLength(0);
    expect(totalUpdates).toHaveLength(0);
    expect(store.entries).toHaveLength(0);

    // A single coalesced flush applies the burst as exactly ONE update each.
    store.flushLive();
    expect(entriesUpdates).toEqual([N]);
    expect(totalUpdates).toEqual([N]);

    // Ordering preserved (newest-first) and no rows dropped.
    expect(store.entries).toHaveLength(N);
    expect(store.entries[0].id).toBe(N);
    expect(store.entries[N - 1].id).toBe(1);
    expect(store.total).toBe(N);

    stopEntries();
    stopTotal();
  });

  it("dedups within a buffered burst (no duplicate rows, total counts once)", async () => {
    invokeMock.mockResolvedValueOnce(makePage([], 0, 0));
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();

    // id 7 arrives twice in the same burst; the second is dropped at ingest.
    liveHandler?.(makeEntry({ id: 7, ts: 700 }));
    liveHandler?.(makeEntry({ id: 8, ts: 800 }));
    liveHandler?.(makeEntry({ id: 7, ts: 700 }));
    store.flushLive();

    expect(store.entries.map((e) => e.id)).toEqual([8, 7]);
    expect(store.total).toBe(2);
  });

  it("auto-flushes the buffer on the next frame via the scheduler", async () => {
    vi.useFakeTimers();
    try {
      invokeMock.mockResolvedValue(undefined);
      const store = useActivityStore();
      await store.subscribeLive();

      for (let i = 1; i <= 10; i++) {
        liveHandler?.(makeEntry({ id: i, ts: i, bytes: null }));
      }
      // No frame has elapsed yet: still buffered.
      expect(store.entries).toHaveLength(0);

      // The node test env has no requestAnimationFrame, so the store falls back to
      // a ~1-frame setTimeout; advancing past it flushes the burst once.
      await vi.advanceTimersByTimeAsync(20);
      expect(store.entries).toHaveLength(10);
      expect(store.entries[0].id).toBe(10);
      expect(store.total).toBe(10);
    } finally {
      vi.useRealTimers();
    }
  });

  it("eagerly flushes when the buffer reaches the cap, keeping newest-first + bound", async () => {
    invokeMock.mockResolvedValueOnce(makePage([], 0, 0));
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();

    // Fire a burst LARGER than the cap WITHOUT any manual flush: the eager
    // at-cap flush keeps memory bounded even if no frame fires mid-burst.
    const overflow = 25;
    for (let i = 1; i <= LIVE_TAIL_CAP + overflow; i++) {
      liveHandler?.(makeEntry({ id: i, ts: i, bytes: null }));
    }
    // Drain the trailing partial buffer (the last < cap events).
    store.flushLive();

    expect(store.entries).toHaveLength(LIVE_TAIL_CAP);
    // Newest retained at the front, oldest `overflow` evicted.
    expect(store.entries[0].id).toBe(LIVE_TAIL_CAP + overflow);
    expect(store.entries[store.entries.length - 1].id).toBe(overflow + 1);
    // total counts every ingested event (eviction does not decrement it).
    expect(store.total).toBe(LIVE_TAIL_CAP + overflow);
  });

  it("flushes the buffer on unsubscribe so state stays consistent", async () => {
    invokeMock.mockResolvedValueOnce(makePage([], 0, 0));
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();

    liveHandler?.(makeEntry({ id: 1, ts: 1, bytes: null }));
    liveHandler?.(makeEntry({ id: 2, ts: 2, bytes: null }));
    // Not yet applied (buffered).
    expect(store.entries).toHaveLength(0);

    store.unsubscribeLive();
    // Teardown drains the buffer so entries / total stay in sync.
    expect(store.entries.map((e) => e.id)).toEqual([2, 1]);
    expect(store.total).toBe(2);
  });
});

describe("activity store: render window (issue #45)", () => {
  it("ACTIVITY_RENDER_WINDOW bounds the rendered slice below the live-tail cap", () => {
    // The view renders entries.slice(0, ACTIVITY_RENDER_WINDOW); that window must
    // be well under the live-tail cap so the mounted DOM never grows to ~1000.
    expect(ACTIVITY_RENDER_WINDOW).toBeGreaterThan(0);
    expect(ACTIVITY_RENDER_WINDOW).toBeLessThan(LIVE_TAIL_CAP);
  });

  it("the windowed slice keeps the newest rows and is capped at the window size", async () => {
    invokeMock.mockResolvedValueOnce(makePage([], 0, 0));
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();

    const burst = ACTIVITY_RENDER_WINDOW + 80;
    for (let i = 1; i <= burst; i++) {
      liveHandler?.(makeEntry({ id: i, ts: i, bytes: null }));
    }
    store.flushLive();

    // The store holds more than one window of entries...
    expect(store.entries.length).toBe(burst);
    // ...but a render window slices only the newest ACTIVITY_RENDER_WINDOW rows.
    const windowed = store.entries.slice(0, ACTIVITY_RENDER_WINDOW);
    expect(windowed).toHaveLength(ACTIVITY_RENDER_WINDOW);
    expect(windowed[0].id).toBe(burst);
    expect(windowed[windowed.length - 1].id).toBe(burst - ACTIVITY_RENDER_WINDOW + 1);
  });
});
