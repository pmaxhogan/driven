import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";

// Live bottleneck-classification store tests (issue #308). The seams are the
// `bottleneck_status` seed command and the `sync:bottleneck` live event;
// mocking both drives the whole store - seed, live folding, the debounce +
// hysteresis gate, and the garbled-wire-data coercion - with no backend.

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

import { useBottleneckStore, DEBOUNCE_MS } from "../stores/bottleneck";
import type { BottleneckSnapshot } from "../ipc/types";

function snap(
  tsMs: number,
  state: BottleneckSnapshot["state"],
  extra: Partial<BottleneckSnapshot> = {}
): BottleneckSnapshot {
  return {
    tsMs,
    state,
    rateBytesPerSec: null,
    backend: null,
    backoffRemainingMs: null,
    ...extra,
  };
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  listenMock.mockClear();
  unlistenMock.mockClear();
  for (const k of Object.keys(handlers)) delete handlers[k];
});

describe("bottleneck store", () => {
  it("seeds from bottleneck_status and subscribes to live events", async () => {
    invokeMock.mockResolvedValueOnce(snap(1000, "not_backing_up"));
    const store = useBottleneckStore();
    await store.start();

    expect(invokeMock).toHaveBeenCalledWith("bottleneck_status", undefined);
    expect(store.displayed?.state).toBe("not_backing_up");
    expect(listenMock).toHaveBeenCalledWith("sync:bottleneck", expect.any(Function));
  });

  it("adopts the FIRST snapshot immediately (a hydration read needs no debounce)", async () => {
    invokeMock.mockResolvedValueOnce(snap(1000, "disk", { rateBytesPerSec: 210_000_000 }));
    const store = useBottleneckStore();
    await store.start();
    expect(store.displayed?.state).toBe("disk");
  });

  it("holds the old state until a new one has been stable for the debounce window", async () => {
    invokeMock.mockResolvedValueOnce(snap(0, "not_backing_up"));
    const store = useBottleneckStore();
    await store.start();
    expect(store.displayed?.state).toBe("not_backing_up");

    // A new state arrives, but hasn't held long enough yet.
    handlers["sync:bottleneck"](snap(1000, "network", { rateBytesPerSec: 42_000_000 }));
    expect(store.displayed?.state).toBe("not_backing_up");

    handlers["sync:bottleneck"](
      snap(1000 + DEBOUNCE_MS - 1, "network", { rateBytesPerSec: 42_000_000 })
    );
    expect(store.displayed?.state).toBe("not_backing_up");

    // Now it has held for >= DEBOUNCE_MS since it first appeared.
    handlers["sync:bottleneck"](
      snap(1000 + DEBOUNCE_MS, "network", { rateBytesPerSec: 42_000_000 })
    );
    expect(store.displayed?.state).toBe("network");
  });

  it("flapping between two states never reaches the display", async () => {
    invokeMock.mockResolvedValueOnce(snap(0, "mixed"));
    const store = useBottleneckStore();
    await store.start();

    // Alternates every tick for well over the debounce window - each change
    // resets the window, so `displayed` never moves off the seeded state.
    for (let i = 1; i <= 20; i++) {
      handlers["sync:bottleneck"](snap(i * 1000, i % 2 === 0 ? "disk" : "cpu"));
    }
    expect(store.displayed?.state).toBe("mixed");
  });

  it("keeps the debounced state's numbers fresh while it holds", async () => {
    invokeMock.mockResolvedValueOnce(snap(0, "cpu", { rateBytesPerSec: 100_000 }));
    const store = useBottleneckStore();
    await store.start();

    // Same state, later tick, different rate, still not past the window.
    handlers["sync:bottleneck"](snap(1000, "cpu", { rateBytesPerSec: 500_000 }));
    // Not yet promoted (started counting from ts=0, the seed).
    expect(store.displayed?.rateBytesPerSec).toBe(100_000);

    handlers["sync:bottleneck"](snap(DEBOUNCE_MS, "cpu", { rateBytesPerSec: 900_000 }));
    expect(store.displayed?.state).toBe("cpu");
    expect(store.displayed?.rateBytesPerSec).toBe(900_000);
  });

  it("coerces a garbled wire snapshot to safe defaults", async () => {
    invokeMock.mockRejectedValueOnce(new Error("ipc down"));
    const store = useBottleneckStore();
    await store.start();
    expect(store.displayed).toBeNull();

    handlers["sync:bottleneck"]({
      tsMs: "nope",
      state: "not_a_real_state",
      rateBytesPerSec: -5,
      backend: 42,
      backoffRemainingMs: Number.NaN,
    });
    expect(store.displayed).toEqual(
      snap(0, "not_backing_up", { rateBytesPerSec: null, backend: null, backoffRemainingMs: null })
    );
  });

  it("stop() unsubscribes and start() is idempotent", async () => {
    invokeMock.mockResolvedValue(snap(0, "not_backing_up"));
    const store = useBottleneckStore();
    await store.start();
    await store.start();
    expect(listenMock).toHaveBeenCalledTimes(1);
    store.stop();
    expect(unlistenMock).toHaveBeenCalledTimes(1);
    expect(handlers["sync:bottleneck"]).toBeUndefined();
  });
});
