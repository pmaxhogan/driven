import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";

// Pause store tests. The seams are `@tauri-apps/api/event`'s `listen` (the
// `sync:pause_changed` subscription) and `@tauri-apps/api/core`'s `invoke` (the
// `get_pause_state` hydrate and the `resume_sync` call behind the banner's
// Resume button). Mocking both drives: the timed vs indefinite shapes, the
// countdown derivation, the optimistic resume + its rollback on failure, and the
// subscribe/hydrate wiring - all against a fake backend.

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

import { usePauseStore } from "../stores/pause";

const PAUSE_EVENT = "sync:pause_changed";

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  listenMock.mockClear();
  unlistenMock.mockClear();
  for (const key of Object.keys(handlers)) delete handlers[key];
});

describe("pause store", () => {
  it("is inactive with no pause", () => {
    const store = usePauseStore();
    expect(store.active).toBe(false);
    expect(store.indefinite).toBe(false);
    expect(store.minutesRemaining).toBe(null);
  });

  it("reports an indefinite pause with no countdown", () => {
    const store = usePauseStore();
    store.ingest({ kind: "indefinite" });
    expect(store.active).toBe(true);
    expect(store.indefinite).toBe(true);
    expect(store.msRemaining).toBe(null);
    expect(store.minutesRemaining).toBe(null);
  });

  it("derives the minutes left on a timed pause, rounding the last partial minute up", () => {
    const store = usePauseStore();
    // 26 minutes 30 seconds out: the user should read "27m left", not "26m".
    store.ingest({ kind: "timed", until_ms: Date.now() + 26.5 * 60_000 });
    expect(store.active).toBe(true);
    expect(store.indefinite).toBe(false);
    expect(store.minutesRemaining).toBe(27);
  });

  it("floors an already-elapsed timed pause at zero rather than counting negative", () => {
    const store = usePauseStore();
    store.ingest({ kind: "timed", until_ms: Date.now() - 5 * 60_000 });
    expect(store.msRemaining).toBe(0);
    expect(store.minutesRemaining).toBe(0);
  });

  it("recomputes the countdown from the wall clock on tick (no drifting accumulator)", () => {
    const store = usePauseStore();
    const until = Date.now() + 10 * 60_000;
    store.ingest({ kind: "timed", until_ms: until });
    const before = store.msRemaining ?? 0;
    // Jump the clock forward by 5 minutes; a tick must reflect the real elapsed
    // time, not one decrement.
    const realNow = Date.now;
    vi.spyOn(Date, "now").mockImplementation(() => realNow.call(Date) + 5 * 60_000);
    store.tick();
    expect(store.msRemaining).toBeLessThanOrEqual(before - 5 * 60_000 + 50);
    expect(store.minutesRemaining).toBe(5);
    vi.mocked(Date.now).mockRestore();
  });

  it("clears on a null pause event (resume / auto-expiry)", async () => {
    const store = usePauseStore();
    await store.subscribe();
    handlers[PAUSE_EVENT]({ kind: "indefinite" });
    expect(store.active).toBe(true);
    handlers[PAUSE_EVENT](null);
    expect(store.active).toBe(false);
  });

  it("subscribes to sync:pause_changed and ingests the live payload", async () => {
    const store = usePauseStore();
    await store.subscribe();
    expect(listenMock).toHaveBeenCalledWith(PAUSE_EVENT, expect.any(Function));
    handlers[PAUSE_EVENT]({ kind: "timed", until_ms: Date.now() + 60_000 });
    expect(store.active).toBe(true);
    expect(store.minutesRemaining).toBe(1);
  });

  it("subscribe is idempotent (one registration)", async () => {
    const store = usePauseStore();
    await store.subscribe();
    await store.subscribe();
    expect(listenMock).toHaveBeenCalledTimes(1);
  });

  it("hydrates the current pause from get_pause_state", async () => {
    invokeMock.mockResolvedValue({ kind: "indefinite" });
    const store = usePauseStore();
    await store.hydrate();
    expect(invokeMock).toHaveBeenCalledWith("get_pause_state", undefined);
    expect(store.indefinite).toBe(true);
  });

  it("hydrate swallows a backend failure and stays unpaused", async () => {
    invokeMock.mockRejectedValue(new Error("nope"));
    const store = usePauseStore();
    await store.hydrate();
    expect(store.active).toBe(false);
  });

  it("resume clears optimistically and calls resume_sync", async () => {
    const store = usePauseStore();
    store.ingest({ kind: "indefinite" });
    await store.resume();
    expect(invokeMock).toHaveBeenCalledWith("resume_sync", undefined);
    expect(store.active).toBe(false);
  });

  it("resume restores the pause and rethrows when the backend rejects", async () => {
    invokeMock.mockRejectedValue(new Error("resume failed"));
    const store = usePauseStore();
    store.ingest({ kind: "indefinite" });
    await expect(store.resume()).rejects.toThrow("resume failed");
    // The banner must NOT vanish on a failed resume - backups are still paused.
    expect(store.active).toBe(true);
  });

  it("unsubscribe stops the listener", async () => {
    const store = usePauseStore();
    await store.subscribe();
    store.unsubscribe();
    expect(unlistenMock).toHaveBeenCalledTimes(1);
  });
});
