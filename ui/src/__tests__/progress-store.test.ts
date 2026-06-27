import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";

// Progress store tests (issue #46). The seams are `@tauri-apps/api/event`'s
// `listen` (the `sync:status_changed` subscription) and `@tauri-apps/api/core`'s
// `invoke` (the `get_sync_status` hydrate path). Mocking both lets us drive: a
// run becoming active vs idle, the determinate percent from an `executing`
// state's byte/file totals, the indeterminate (null) percent for scan/plan, the
// per-account merge vs aggregate-replace ingest, and the subscribe + hydrate
// wiring - all against a fake backend.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

// Capture the registered event handlers so a test can fire events on demand.
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

import { useProgressStore } from "../stores/progress";
import type { ExecProgress, GlobalSyncStatus, OrchestratorState } from "../ipc/types";

// --- OrchestratorState builders (snake_case wire shapes, SPEC s5) -----------

function idle(): OrchestratorState {
  return { state: "idle", last_run_at: null };
}
function scanning(scanned = 0): OrchestratorState {
  return { state: "scanning", source_id: "src-1", scanned };
}
function planning(): OrchestratorState {
  return { state: "planning", plan: {} };
}
function verifying(): OrchestratorState {
  return { state: "verifying", sampled: 0, mismatches: 0 };
}
function backoff(): OrchestratorState {
  return { state: "backoff", until: 0 };
}
function paused(): OrchestratorState {
  return { state: "paused", reason: { kind: "user" } };
}
function errored(): OrchestratorState {
  return { state: "error", detail: { code: "drive.unknown", message: "boom" } };
}
function executing(p: Partial<ExecProgress>): OrchestratorState {
  const progress: ExecProgress = {
    files_done: 0,
    files_total: 0,
    bytes_done: 0,
    bytes_total: 0,
    trashes_done: 0,
    trashes_total: 0,
    errors: 0,
    ...p,
  };
  return { state: "executing", progress };
}

function perAccount(
  accountId: string,
  state: OrchestratorState
): { account_id: string; state: OrchestratorState } {
  return { account_id: accountId, state };
}
function global(...accounts: { account_id: string; state: OrchestratorState }[]): GlobalSyncStatus {
  return { accounts };
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  for (const k of Object.keys(handlers)) delete handlers[k];
  unlistenMock.mockReset();
  listenMock.mockClear();
});

describe("progress store - active vs idle", () => {
  it("is inactive with no accounts (percent null)", () => {
    const store = useProgressStore();
    expect(store.active).toBe(false);
    expect(store.percent).toBeNull();
  });

  it("becomes active on a working state and inactive again when idle", () => {
    const store = useProgressStore();

    store.ingest(perAccount("a", scanning(5)));
    expect(store.active).toBe(true);
    // scanning carries no total -> indeterminate
    expect(store.percent).toBeNull();

    store.ingest(perAccount("a", idle()));
    expect(store.active).toBe(false);
    expect(store.percent).toBeNull();
  });

  it("treats every working state (power_check/scanning/planning/executing/verifying) as active", () => {
    const store = useProgressStore();
    const working: OrchestratorState[] = [
      { state: "power_check" },
      scanning(),
      planning(),
      executing({ files_total: 1 }),
      verifying(),
    ];
    for (const s of working) {
      store.ingest(global(perAccount("a", s)));
      expect(store.active).toBe(true);
    }
  });

  it("does NOT treat backoff / paused / error / idle as an active run", () => {
    const store = useProgressStore();
    for (const s of [backoff(), paused(), errored(), idle()]) {
      store.ingest(global(perAccount("a", s)));
      expect(store.active).toBe(false);
      expect(store.percent).toBeNull();
    }
  });
});

describe("progress store - determinate percent", () => {
  it("computes the byte fraction while executing", () => {
    const store = useProgressStore();
    store.ingest(
      perAccount(
        "a",
        executing({ bytes_done: 512, bytes_total: 1024, files_total: 4, files_done: 2 })
      )
    );
    expect(store.active).toBe(true);
    expect(store.percent).toBeCloseTo(0.5, 5);
    expect(store.filesDone).toBe(2);
    expect(store.filesTotal).toBe(4);
  });

  it("falls back to op counts when the plan moves no bytes (delete-only)", () => {
    const store = useProgressStore();
    // No upload bytes; 3 of 4 trash ops done -> 0.75 from op counts.
    store.ingest(perAccount("a", executing({ trashes_done: 3, trashes_total: 4 })));
    expect(store.percent).toBeCloseTo(0.75, 5);
  });

  it("is indeterminate (null) while executing with no measurable total yet", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", executing({})));
    expect(store.active).toBe(true);
    expect(store.percent).toBeNull();
  });

  it("clamps a bogus over-100% fraction to 1", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", executing({ bytes_done: 2048, bytes_total: 1024 })));
    expect(store.percent).toBe(1);
  });

  it("aggregates byte progress across multiple executing accounts", () => {
    const store = useProgressStore();
    store.ingest(
      global(
        perAccount(
          "a",
          executing({ bytes_done: 100, bytes_total: 400, files_done: 1, files_total: 2 })
        ),
        perAccount(
          "b",
          executing({ bytes_done: 100, bytes_total: 100, files_done: 3, files_total: 3 })
        )
      )
    );
    // (100 + 100) / (400 + 100) = 0.4
    expect(store.percent).toBeCloseTo(0.4, 5);
    expect(store.filesDone).toBe(4);
    expect(store.filesTotal).toBe(5);
  });

  it("ignores non-executing accounts when one account is executing", () => {
    const store = useProgressStore();
    store.ingest(
      global(
        perAccount("a", executing({ bytes_done: 250, bytes_total: 1000 })),
        perAccount("b", idle())
      )
    );
    expect(store.active).toBe(true);
    expect(store.percent).toBeCloseTo(0.25, 5);
  });
});

describe("progress store - ingest shapes", () => {
  it("MERGES a per-account payload but REPLACES on an aggregate payload", () => {
    const store = useProgressStore();
    // Two accounts working via per-account merges.
    store.ingest(perAccount("a", executing({ bytes_done: 50, bytes_total: 100 })));
    store.ingest(perAccount("b", scanning()));
    expect(Object.keys(store.states).sort()).toEqual(["a", "b"]);
    expect(store.active).toBe(true);

    // An aggregate payload listing only idle accounts replaces the whole map.
    store.ingest(global(perAccount("a", idle()), perAccount("b", idle())));
    expect(store.active).toBe(false);
    expect(Object.keys(store.states).sort()).toEqual(["a", "b"]);
  });
});

describe("progress store - subscribe + hydrate", () => {
  it("subscribes and updates from a live sync:status_changed event", async () => {
    const store = useProgressStore();
    await store.subscribe();
    expect(handlers["sync:status_changed"]).toBeTypeOf("function");

    handlers["sync:status_changed"](perAccount("a", executing({ bytes_done: 1, bytes_total: 2 })));
    expect(store.active).toBe(true);
    expect(store.percent).toBeCloseTo(0.5, 5);

    handlers["sync:status_changed"](perAccount("a", idle()));
    expect(store.active).toBe(false);
  });

  it("is idempotent: a second subscribe does not register a second listener", async () => {
    const store = useProgressStore();
    await store.subscribe();
    await store.subscribe();
    expect(listenMock).toHaveBeenCalledTimes(1);
  });

  it("hydrates the map from get_sync_status (a run already underway at boot)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_sync_status") {
        return Promise.resolve(
          global(perAccount("a", executing({ bytes_done: 3, bytes_total: 4 })))
        );
      }
      return Promise.resolve(null);
    });

    const store = useProgressStore();
    await store.hydrate();
    expect(invokeMock).toHaveBeenCalledWith("get_sync_status", undefined);
    expect(store.active).toBe(true);
    expect(store.percent).toBeCloseTo(0.75, 5);
  });

  it("hydrate swallows a get_sync_status failure (best-effort)", async () => {
    invokeMock.mockRejectedValue(new Error("backend not ready"));
    const store = useProgressStore();
    await expect(store.hydrate()).resolves.toBeUndefined();
    expect(store.active).toBe(false);
  });

  it("unsubscribe tears down the listener", async () => {
    const store = useProgressStore();
    await store.subscribe();
    store.unsubscribe();
    expect(unlistenMock).toHaveBeenCalledTimes(1);
    expect(handlers["sync:status_changed"]).toBeUndefined();
  });
});
