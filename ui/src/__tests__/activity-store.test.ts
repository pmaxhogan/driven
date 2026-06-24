import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";

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
let laggedHandler: (() => void) | null = null;
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
  listen: vi.fn(
    async (event: string, cb: (e: { payload: unknown }) => void) => {
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
        laggedHandler = () => cb({ payload: null });
        return unlistenLaggedMock;
      }
      return vi.fn();
    },
  ),
}));

import {
  useActivityStore,
  ACTIVITY_PAGE_SIZE,
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

/** Build a query_activity page DTO for `entries`, with paging metadata. */
function makePage(
  entries: ActivityEntry[],
  page: number,
  total: number,
): {
  entries: ActivityEntry[];
  total: number;
  page: number;
  limit: number;
  hasMore: boolean;
} {
  const consumed = (page + 1) * ACTIVITY_PAGE_SIZE;
  return {
    entries,
    total,
    page,
    limit: ACTIVITY_PAGE_SIZE,
    hasMore: total > consumed,
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
  it("loadInitial fetches page 0 and records paging metadata", async () => {
    const rows = [makeEntry({ id: 3 }), makeEntry({ id: 2 }), makeEntry({ id: 1 })];
    invokeMock.mockResolvedValueOnce(makePage(rows, 0, 250));
    const store = useActivityStore();
    await store.loadInitial();
    expect(invokeMock).toHaveBeenCalledWith("query_activity", {
      filter: {},
      page: { page: 0, limit: ACTIVITY_PAGE_SIZE },
    });
    expect(store.entries).toHaveLength(3);
    expect(store.entries[0].id).toBe(3);
    expect(store.total).toBe(250);
    expect(store.hasMore).toBe(true);
    expect(store.loadedPage).toBe(0);
  });

  it("loadMore appends the next page WITHOUT re-querying earlier pages", async () => {
    const page0 = [makeEntry({ id: 200 }), makeEntry({ id: 199 })];
    const page1 = [makeEntry({ id: 100 }), makeEntry({ id: 99 })];
    invokeMock.mockResolvedValueOnce(makePage(page0, 0, 250));
    const store = useActivityStore();
    await store.loadInitial();
    expect(invokeMock).toHaveBeenCalledTimes(1);

    invokeMock.mockResolvedValueOnce(makePage(page1, 1, 250));
    await store.loadMore();

    // Exactly ONE additional fetch (page 1), never a re-fetch of page 0.
    expect(invokeMock).toHaveBeenCalledTimes(2);
    expect(invokeMock).toHaveBeenNthCalledWith(2, "query_activity", {
      filter: {},
      page: { page: 1, limit: ACTIVITY_PAGE_SIZE },
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
    const page0 = [makeEntry({ id: 200 })];
    invokeMock.mockResolvedValueOnce(makePage(page0, 0, 250));
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();

    // A live event arrives for id 150 before it is paged in.
    liveHandler?.(makeEntry({ id: 150, ts: 900 }));
    expect(store.entries.map((e) => e.id)).toEqual([150, 200]);

    // Page 1 includes id 150 again - it must NOT be duplicated.
    invokeMock.mockResolvedValueOnce(
      makePage([makeEntry({ id: 150, ts: 900 }), makeEntry({ id: 149 })], 1, 250),
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
    // An info-level live event must be dropped under a min-level=error filter.
    liveHandler?.(makeEntry({ id: 9, level: "info" }));
    expect(store.entries).toHaveLength(0);
    // A matching error-level event is kept.
    liveHandler?.(makeEntry({ id: 10, level: "error" }));
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
      makePage([makeEntry({ id: 5, ts: 500 }), makeEntry({ id: 4, ts: 400 })], 0, 2),
    );
    const store = useActivityStore();
    await store.subscribeLive();
    await store.loadInitial();
    expect(store.entries.map((e) => e.id)).toEqual([5, 4]);

    // A burst happened and the live broadcast lagged: the durable log now also
    // has rows 7 and 6 (dropped from the live tail). The reconcile re-query
    // returns the newest page including the already-present 5 + the new 7, 6.
    invokeMock.mockResolvedValueOnce(
      makePage(
        [
          makeEntry({ id: 7, ts: 700 }),
          makeEntry({ id: 6, ts: 600 }),
          makeEntry({ id: 5, ts: 500 }),
        ],
        0,
        4,
      ),
    );
    laggedHandler?.();
    // Let the async reconcile settle.
    await Promise.resolve();
    await Promise.resolve();

    const ids = store.entries.map((e) => e.id);
    // No duplicate of id 5; the dropped 7 + 6 are recovered, newest-first.
    expect(ids).toEqual([7, 6, 5, 4]);
    expect(ids.filter((i) => i === 5)).toHaveLength(1);
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
    // Live tail capped at CAP, but the loaded history row survives at the tail.
    expect(store.entries.length).toBe(LIVE_TAIL_CAP + 1);
    expect(store.entries[store.entries.length - 1].id).toBe(1);
  });
});

describe("activity store: filters", () => {
  it("applyFilter re-queries from page 0 with the new filter and resets state", async () => {
    invokeMock.mockResolvedValueOnce(
      makePage([makeEntry({ id: 1 }), makeEntry({ id: 2 })], 0, 2),
    );
    const store = useActivityStore();
    await store.loadInitial();
    expect(store.entries).toHaveLength(2);

    invokeMock.mockResolvedValueOnce(
      makePage([makeEntry({ id: 3, level: "error" })], 0, 1),
    );
    await store.applyFilter({ minLevel: "error", sourceId: "src-1" });

    expect(invokeMock).toHaveBeenNthCalledWith(2, "query_activity", {
      filter: { minLevel: "error", sourceId: "src-1" },
      page: { page: 0, limit: ACTIVITY_PAGE_SIZE },
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
        }),
    );
    const firstLoad = store.loadInitial();

    // While in flight, the user applies an error-only filter, which re-queries.
    invokeMock.mockResolvedValueOnce(
      makePage([makeEntry({ id: 99, level: "error" })], 0, 1),
    );
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
});

describe("activity store: backend facets + summary (M7-P2-4, P2-5)", () => {
  it("loadEventTypeOptions populates the dropdown source from the backend", async () => {
    invokeMock.mockResolvedValueOnce(["paused", "scan_done", "upload_done"]);
    const store = useActivityStore();
    await store.loadEventTypeOptions();
    expect(invokeMock).toHaveBeenCalledWith(
      "distinct_activity_event_types",
      undefined,
    );
    expect(store.eventTypeOptions).toEqual([
      "paused",
      "scan_done",
      "upload_done",
    ]);
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
      expect.objectContaining({ throughputWindowMs: 60000 }),
    );
    expect(store.summary).toEqual(summary);
  });
});
