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

// The live-tail seam: capture the handler `onActivityNew` registers so the test
// can fire `activity:new` events on demand. `listen` returns an unlisten fn.
let liveHandler: ((payload: unknown) => void) | null = null;
const unlistenMock = vi.fn();
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn((event: string, cb: (e: { payload: unknown }) => void) => {
    if (event === "activity:new") {
      liveHandler = (payload: unknown) => cb({ payload });
    }
    return Promise.resolve(unlistenMock);
  }),
}));

import { useActivityStore, ACTIVITY_PAGE_SIZE } from "../stores/activity";
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
  unlistenMock.mockReset();
  liveHandler = null;
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

  it("unsubscribeLive calls the unlisten fn", async () => {
    const store = useActivityStore();
    await store.subscribeLive();
    store.unsubscribeLive();
    expect(unlistenMock).toHaveBeenCalledTimes(1);
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
    expect(store.error).toBeNull();
  });

  it("isEmpty is false when an error occurred", async () => {
    invokeMock.mockRejectedValueOnce(new Error("db locked"));
    const store = useActivityStore();
    await store.loadInitial();
    expect(store.error).toContain("db locked");
    expect(store.isEmpty).toBe(false);
  });
});
