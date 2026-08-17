import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";

import type { QueueSnapshot, WorkItem, WorkKind } from "../ipc/types";

// Work-queue store tests (issue #303). The seams are `@tauri-apps/api/event`'s
// `listen` (the `queue:changed` subscription) and `@tauri-apps/api/core`'s
// `invoke` (the `get_work_queue` + `list_sources` hydrate and the cancel /
// clear calls). Mocking both drives the whole store against a fake backend: the
// merge across accounts, the display ordering, the badge count, source-name
// resolution, and the cancel/clear paths including their failure recovery.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

const handlers: Record<string, (payload: unknown) => void> = {};
const unlistenMock = vi.fn();
const listenMock = vi.fn(async (event: string, cb: (e: { payload: unknown }) => void) => {
  handlers[event] = (payload: unknown) => cb({ payload });
  return vi.fn(() => {
    delete handlers[event];
    unlistenMock();
  });
});
vi.mock("@tauri-apps/api/event", () => ({
  listen: (event: string, cb: (e: { payload: unknown }) => void) => listenMock(event, cb),
}));

import { useWorkQueueStore } from "../stores/workQueue";

const QUEUE_EVENT = "queue:changed";

function item(id: number, kind: WorkKind, sourceId: string | null = null): WorkItem {
  return { id, kind, source_id: sourceId, enqueued_at: 1_000 + id, tick: "manual" };
}

function snapshot(accountId: string, partial: Partial<QueueSnapshot> = {}): QueueSnapshot {
  return {
    account_id: accountId,
    running: null,
    running_cancelled: false,
    pending: [],
    next_scheduled_at: null,
    ...partial,
  };
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  listenMock.mockClear();
  unlistenMock.mockClear();
  for (const key of Object.keys(handlers)) delete handlers[key];
});

describe("work queue store", () => {
  it("is empty with no snapshots", () => {
    const store = useWorkQueueStore();
    expect(store.rows).toEqual([]);
    expect(store.count).toBe(0);
    expect(store.clearable).toBe(false);
    expect(store.nextScheduledAt).toBe(null);
  });

  it("counts the running item as outstanding, not just the pending ones", () => {
    const store = useWorkQueueStore();
    store.ingest(
      snapshot("a", {
        running: item(1, "manual"),
        pending: [item(2, "watcher"), item(3, "scheduled")],
      })
    );
    // The badge is what a user checks before closing the app, so the item that
    // is mid-upload has to be in it.
    expect(store.count).toBe(3);
    expect(store.clearable).toBe(true);
  });

  it("lists running items before pending ones, pending in run order", () => {
    const store = useWorkQueueStore();
    store.ingest(
      snapshot("a", {
        running: item(9, "manual"),
        pending: [item(10, "recovery"), item(11, "watcher")],
      })
    );
    expect(store.rows.map((r) => r.item.id)).toEqual([9, 10, 11]);
    expect(store.rows[0].running).toBe(true);
    expect(store.rows[1].running).toBe(false);
  });

  it("merges every account's queue and keeps each row attributable", () => {
    const store = useWorkQueueStore();
    store.ingest(snapshot("b", { pending: [item(2, "watcher")] }));
    store.ingest(snapshot("a", { running: item(1, "manual") }));

    // Accounts are visited in a stable order so rows do not jump between events.
    expect(store.rows.map((r) => [r.accountId, r.item.id])).toEqual([
      ["a", 1],
      ["b", 2],
    ]);
  });

  it("replaces an account's queue wholesale on each event", () => {
    const store = useWorkQueueStore();
    store.ingest(snapshot("a", { pending: [item(1, "manual"), item(2, "watcher")] }));
    store.ingest(snapshot("a", { pending: [item(2, "watcher")] }));
    // A whole-snapshot payload means a cancelled item disappears without the
    // store having to diff anything.
    expect(store.rows.map((r) => r.item.id)).toEqual([2]);
  });

  it("flags a cancelled running item as draining rather than removing it", () => {
    const store = useWorkQueueStore();
    store.ingest(snapshot("a", { running: item(1, "manual"), running_cancelled: true }));
    expect(store.rows[0].draining).toBe(true);
    expect(store.count).toBe(1);
  });

  it("reports the SOONEST armed scheduled scan across accounts", () => {
    const store = useWorkQueueStore();
    store.ingest(snapshot("a", { next_scheduled_at: 5_000 }));
    store.ingest(snapshot("b", { next_scheduled_at: 3_000 }));
    store.ingest(snapshot("c", { next_scheduled_at: null }));
    expect(store.nextScheduledAt).toBe(3_000);
  });

  it("hydrates the queues and the source names that label them", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_work_queue") {
        return [snapshot("a", { running: item(1, "manual", "src-1") })];
      }
      if (cmd === "list_sources") {
        return [{ id: "src-1", displayName: "Documents" }];
      }
      return undefined;
    });

    const store = useWorkQueueStore();
    await store.hydrate();

    expect(store.rows).toHaveLength(1);
    expect(store.rows[0].sourceName).toBe("Documents");
  });

  it("still lists work when the source names cannot be read", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "get_work_queue")
        return [snapshot("a", { pending: [item(1, "watcher", "src-1")] })];
      if (cmd === "list_sources") throw new Error("db busy");
      return undefined;
    });
    vi.spyOn(console, "error").mockImplementation(() => undefined);

    const store = useWorkQueueStore();
    await store.hydrate();

    // A missing name costs a label, never the row: an unlabelled queue is still
    // the truth about what is running.
    expect(store.rows).toHaveLength(1);
    expect(store.rows[0].sourceName).toBe(null);
  });

  it("hydration REPLACES the per-account map so a drained account disappears", async () => {
    const store = useWorkQueueStore();
    store.ingest(snapshot("stale", { pending: [item(1, "manual")] }));
    invokeMock.mockImplementation(async (cmd: string) => (cmd === "get_work_queue" ? [] : []));
    await store.hydrate();
    expect(store.rows).toEqual([]);
  });

  it("subscribes to queue:changed and folds live snapshots in", async () => {
    const store = useWorkQueueStore();
    await store.subscribe();
    expect(listenMock).toHaveBeenCalledWith(QUEUE_EVENT, expect.any(Function));

    handlers[QUEUE_EVENT](snapshot("a", { pending: [item(4, "scheduled")] }));
    expect(store.count).toBe(1);

    // Idempotent: a second subscribe must not double-register listeners.
    await store.subscribe();
    expect(listenMock).toHaveBeenCalledTimes(1);

    store.unsubscribe();
    expect(unlistenMock).toHaveBeenCalledTimes(1);
  });

  it("cancels one item by (account, id)", async () => {
    invokeMock.mockResolvedValue(true);
    const store = useWorkQueueStore();
    await store.cancel("acct-1", 7);
    expect(invokeMock).toHaveBeenCalledWith("cancel_work_item", {
      accountId: "acct-1",
      itemId: 7,
    });
  });

  it("re-hydrates and re-throws when a cancel fails, so the panel never lies", async () => {
    vi.spyOn(console, "error").mockImplementation(() => undefined);
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "cancel_work_item") throw new Error("no orchestrator");
      if (cmd === "get_work_queue") return [];
      return [];
    });
    const store = useWorkQueueStore();
    store.ingest(snapshot("a", { pending: [item(1, "manual")] }));

    await expect(store.cancel("a", 1)).rejects.toThrow("no orchestrator");
    expect(invokeMock).toHaveBeenCalledWith("get_work_queue", undefined);
    expect(store.rows).toEqual([]);
  });

  it("clears every account with one call", async () => {
    invokeMock.mockResolvedValue(2);
    const store = useWorkQueueStore();
    await store.clearAll();
    expect(invokeMock).toHaveBeenCalledWith("clear_work_queue", { accountId: null });
  });
});
