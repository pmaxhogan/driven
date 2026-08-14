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
function planning(uploads = 0, trashes = 0): OrchestratorState {
  return { state: "planning", plan: { uploads, trashes, bytes: 0 } };
}
function verifying(sampled = 0): OrchestratorState {
  return { state: "verifying", sampled, mismatches: 0 };
}
function powerCheck(): OrchestratorState {
  return { state: "power_check" };
}
function recovering(bytesDone: number, bytesTotal: number): OrchestratorState {
  return {
    state: "recovering",
    source_id: "src-1",
    path: "dev-drives/dev.vhdx",
    bytes_done: bytesDone,
    bytes_total: bytesTotal,
  };
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

  it("does NOT report 100% on a mixed upload+delete plan while deletes are pending", () => {
    const store = useProgressStore();
    // Uploads fully done (bytes 1000/1000, files 2/2) but 0 of 2 trash ops done.
    // A pure byte fraction would read 100%; op counts keep the bar honest.
    store.ingest(
      perAccount(
        "a",
        executing({
          bytes_done: 1000,
          bytes_total: 1000,
          files_done: 2,
          files_total: 2,
          trashes_done: 0,
          trashes_total: 2,
        })
      )
    );
    // (2 files + 0 trashes) / (2 files + 2 trashes) = 0.5, NOT 1.0.
    expect(store.percent).toBeCloseTo(0.5, 5);
    expect(store.percent!).toBeLessThan(1);
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

// Phase + per-phase counters (the "Run now looks dead during the scan" fix).
// The store used to collapse every working state into a single boolean, so the
// UI could not tell scanning from executing or say how far the scan had got.
describe("progress store - phase + phase counters", () => {
  it("reports no phase while idle", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", idle()));
    expect(store.phase).toBeNull();
    expect(store.scanned).toBe(0);
  });

  it("reports the scanning phase and the live scanned count", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", scanning(12401)));
    expect(store.phase).toBe("scanning");
    expect(store.scanned).toBe(12401);
  });

  it("sums the scanned count across concurrently scanning accounts", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", scanning(100)));
    store.ingest(perAccount("b", scanning(250)));
    expect(store.scanned).toBe(350);
  });

  it("counts planned uploads plus trashes as the planning total", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", planning(1200, 34)));
    expect(store.phase).toBe("planning");
    expect(store.plannedFiles).toBe(1234);
  });

  it("reports the verifying phase and its sampled count", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", verifying(42)));
    expect(store.phase).toBe("verifying");
    expect(store.verified).toBe(42);
  });

  it("reports the pre-flight power check as its own phase", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", powerCheck()));
    expect(store.phase).toBe("power_check");
  });

  it("prefers executing over the pre-upload phases when accounts differ", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", scanning(500)));
    store.ingest(perAccount("b", planning(10)));
    store.ingest(perAccount("c", executing({ bytes_done: 1, bytes_total: 2 })));
    expect(store.phase).toBe("executing");
    // The scan counter is still readable; it just is not what the bar reports.
    expect(store.scanned).toBe(500);
  });

  it("ignores a malformed plan payload rather than throwing", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", { state: "planning", plan: null }));
    expect(store.phase).toBe("planning");
    expect(store.plannedFiles).toBe(0);
  });

  it("reports no phase for non-working states (paused / backoff / error)", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", paused()));
    store.ingest(perAccount("b", backoff()));
    store.ingest(perAccount("c", errored()));
    expect(store.phase).toBeNull();
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

  it("subscribes to BOTH sync channels: a second subscribe registers no more", async () => {
    const store = useProgressStore();
    await store.subscribe();
    // Status alone leaves the bar indeterminate (the `executing` transition
    // carries only zeros), so the progress channel is not optional.
    expect(listenMock).toHaveBeenCalledTimes(2);
    expect(handlers["sync:status_changed"]).toBeTypeOf("function");
    expect(handlers["sync:source_progress"]).toBeTypeOf("function");

    await store.subscribe();
    expect(listenMock).toHaveBeenCalledTimes(2);
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

  it("unsubscribe tears down every listener", async () => {
    const store = useProgressStore();
    await store.subscribe();
    store.unsubscribe();
    expect(unlistenMock).toHaveBeenCalledTimes(2);
    expect(handlers["sync:status_changed"]).toBeUndefined();
    expect(handlers["sync:source_progress"]).toBeUndefined();
  });

  it("cleans up the first listener when the second registration fails", async () => {
    const store = useProgressStore();
    listenMock.mockImplementationOnce(
      async (event: string, cb: (e: { payload: unknown }) => void) => {
        handlers[event] = (payload: unknown) => cb({ payload });
        return vi.fn(() => {
          delete handlers[event];
          unlistenMock();
        });
      }
    );
    listenMock.mockImplementationOnce(async () => {
      throw new Error("listener registration failed");
    });

    await expect(store.subscribe()).rejects.toThrow("listener registration failed");
    // The half-registered status listener must not leak: it is torn down, and a
    // retry re-registers both from scratch.
    expect(unlistenMock).toHaveBeenCalledTimes(1);
    expect(handlers["sync:status_changed"]).toBeUndefined();

    await store.subscribe();
    expect(handlers["sync:status_changed"]).toBeTypeOf("function");
    expect(handlers["sync:source_progress"]).toBeTypeOf("function");
  });
});

// The fix for "the backing-up bar never leaves the indeterminate sweep": the
// orchestrator transitions to `executing` ONCE with a zeroed ExecProgress and
// then streams the moving counters as separate `sync:source_progress` ticks.
// Folding those ticks in is the only way the percent is ever non-null.
describe("progress store - source_progress ticks", () => {
  const tick = (
    accountId: string,
    p: Partial<ExecProgress>,
    sourceId = "src-1"
  ): { account_id: string; source_id: string; progress: ExecProgress } => ({
    account_id: accountId,
    source_id: sourceId,
    progress: {
      files_done: 0,
      files_total: 0,
      bytes_done: 0,
      bytes_total: 0,
      trashes_done: 0,
      trashes_total: 0,
      errors: 0,
      ...p,
    },
  });

  it("turns the zeroed executing snapshot into a determinate percent", () => {
    const store = useProgressStore();
    // Exactly what the backend sends: the transition, carrying only zeros.
    store.ingest(perAccount("a", executing({})));
    expect(store.percent).toBeNull();

    store.ingestProgress(tick("a", { bytes_done: 250, bytes_total: 1000 }));
    expect(store.percent).toBeCloseTo(0.25, 5);

    store.ingestProgress(tick("a", { bytes_done: 750, bytes_total: 1000 }));
    expect(store.percent).toBeCloseTo(0.75, 5);
  });

  it("prefers the tick over the state's embedded snapshot", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", executing({ bytes_done: 0, bytes_total: 1000 })));
    store.ingestProgress(
      tick("a", { bytes_done: 400, bytes_total: 1000, files_done: 4, files_total: 10 })
    );
    expect(store.percent).toBeCloseTo(0.4, 5);
    expect(store.filesDone).toBe(4);
    expect(store.filesTotal).toBe(10);
  });

  // Edge case (a): a late tick must not resurrect a finished run.
  it("ignores a tick for an account that is no longer executing", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", executing({})));
    store.ingestProgress(tick("a", { bytes_done: 500, bytes_total: 1000 }));
    expect(store.active).toBe(true);

    store.ingest(perAccount("a", idle()));
    expect(store.active).toBe(false);
    expect(store.percent).toBeNull();

    // The executor's final snapshot can trail the transition. It must not make
    // the bar reappear, nor leave a percent behind.
    store.ingestProgress(tick("a", { bytes_done: 1000, bytes_total: 1000 }));
    expect(store.active).toBe(false);
    expect(store.percent).toBeNull();
    expect(store.filesDone).toBe(0);
  });

  it("ignores a tick for an account it has never seen a status for", () => {
    const store = useProgressStore();
    store.ingestProgress(tick("ghost", { bytes_done: 5, bytes_total: 10 }));
    expect(store.active).toBe(false);
    expect(store.percent).toBeNull();
    expect(Object.keys(store.states)).toEqual([]);
  });

  // Edge case (b/c): a new run must not inherit the previous run's last tick.
  it("drops the stale tick when the account re-enters executing for a new run", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", executing({})));
    store.ingestProgress(tick("a", { bytes_done: 1000, bytes_total: 1000, files_done: 9 }));
    expect(store.percent).toBe(1);

    // Next source: another `executing` transition carrying ExecProgress::zero().
    store.ingest(perAccount("a", executing({})));
    expect(store.percent).toBeNull();
    expect(store.filesDone).toBe(0);

    // ...and the new run's own ticks drive it from the start.
    store.ingestProgress(tick("a", { bytes_done: 10, bytes_total: 1000 }, "src-2"));
    expect(store.percent).toBeCloseTo(0.01, 5);
  });

  it("drops the tick on ANY state change, not just a new executing", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", executing({})));
    store.ingestProgress(tick("a", { bytes_done: 800, bytes_total: 1000 }));
    store.ingest(perAccount("a", scanning(3)));
    expect(store.phase).toBe("scanning");
    expect(store.percent).toBeNull();

    store.ingest(perAccount("a", executing({})));
    expect(store.percent).toBeNull();
  });

  it("clears every tick when an aggregate payload replaces the map", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", executing({})));
    store.ingestProgress(tick("a", { bytes_done: 900, bytes_total: 1000 }));
    expect(store.percent).toBeCloseTo(0.9, 5);

    // hydrate()'s shape: an aggregate re-states every account at once.
    store.ingest(global(perAccount("a", executing({}))));
    expect(store.percent).toBeNull();
  });

  // Edge case (c): multi-account aggregation must still SUM.
  it("sums ticks across concurrently executing accounts", () => {
    const store = useProgressStore();
    store.ingest(global(perAccount("a", executing({})), perAccount("b", executing({}))));
    store.ingestProgress(
      tick("a", { bytes_done: 100, bytes_total: 400, files_done: 1, files_total: 2 })
    );
    store.ingestProgress(
      tick("b", { bytes_done: 100, bytes_total: 100, files_done: 3, files_total: 3 })
    );
    // (100 + 100) / (400 + 100) = 0.4
    expect(store.percent).toBeCloseTo(0.4, 5);
    expect(store.filesDone).toBe(4);
    expect(store.filesTotal).toBe(5);
  });

  it("mixes a ticked account with one still on its embedded snapshot", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", executing({})));
    store.ingest(perAccount("b", executing({ bytes_done: 50, bytes_total: 100 })));
    store.ingestProgress(tick("a", { bytes_done: 100, bytes_total: 300 }));
    // (100 + 50) / (300 + 100) = 0.375
    expect(store.percent).toBeCloseTo(0.375, 5);
  });

  it("keeps a mixed upload+delete plan honest across ticks", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", executing({})));
    // Uploads finished, trash ops pending: bytes alone would read 100%.
    store.ingestProgress(
      tick("a", {
        bytes_done: 1000,
        bytes_total: 1000,
        files_done: 2,
        files_total: 2,
        trashes_done: 0,
        trashes_total: 2,
      })
    );
    expect(store.percent).toBeCloseTo(0.5, 5);
  });

  it("updates from a live sync:source_progress event", async () => {
    const store = useProgressStore();
    await store.subscribe();

    handlers["sync:status_changed"](perAccount("a", executing({})));
    expect(store.percent).toBeNull();

    handlers["sync:source_progress"](tick("a", { bytes_done: 3, bytes_total: 4 }));
    expect(store.percent).toBeCloseTo(0.75, 5);
  });

  it("ignores a malformed tick payload rather than throwing", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", executing({ bytes_done: 1, bytes_total: 4 })));
    store.ingestProgress({
      account_id: "a",
      source_id: "src-1",
      progress: null as unknown as ExecProgress,
    });
    // The embedded snapshot still stands; nothing threw.
    expect(store.percent).toBeCloseTo(0.25, 5);
  });
});

// --- 2026-08-14 follow-up: the reconcile-phase Recovering state -------------

describe("recovering state", () => {
  it("is an active working phase with a DETERMINATE byte percent", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", recovering(25, 100)));
    expect(store.active).toBe(true);
    expect(store.phase).toBe("recovering");
    expect(store.percent).toBeCloseTo(0.25, 5);
    expect(store.recoveringBytes).toEqual({ done: 25, total: 100 });
  });

  it("is outranked by executing but outranks the pre-flight phases", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", recovering(1, 10)));
    store.ingest(perAccount("b", scanning(5)));
    expect(store.phase).toBe("recovering");
    store.ingest(
      perAccount("c", executing({ bytes_done: 1, bytes_total: 2, files_total: 1 }))
    );
    expect(store.phase).toBe("executing");
  });

  it("falls back to indeterminate when the recovery total is unknown", () => {
    const store = useProgressStore();
    store.ingest(perAccount("a", recovering(0, 0)));
    expect(store.active).toBe(true);
    expect(store.percent).toBeNull();
  });
});

