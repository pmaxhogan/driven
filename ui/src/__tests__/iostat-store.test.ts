import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";

// Live disk/network throughput store tests (2026-08-14 follow-up). The seams
// are the `io_throughput_series` seed command and the `sync:io_throughput`
// live event; mocking both drives the whole store - seed, live folding, the
// window cap, the headline-rate math and the unknown states - with no backend.

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

import {
  useIostatStore,
  IO_WINDOW_SAMPLES,
  IO_HEADLINE_SAMPLES,
} from "../stores/iostat";

function sample(tsMs: number, diskBytes: number, netBytes: number) {
  return { tsMs, diskBytes, netBytes };
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  listenMock.mockClear();
  unlistenMock.mockClear();
  for (const k of Object.keys(handlers)) delete handlers[k];
});

describe("iostat store", () => {
  it("seeds from io_throughput_series and subscribes to live samples", async () => {
    invokeMock.mockResolvedValueOnce({
      bucketMs: 1000,
      samples: [sample(1, 100, 10), sample(2, 200, 20)],
    });
    const store = useIostatStore();
    await store.start();

    expect(invokeMock).toHaveBeenCalledWith("io_throughput_series", undefined);
    expect(store.bucketMs).toBe(1000);
    expect(store.diskSeries).toEqual([100, 200]);
    expect(store.netSeries).toEqual([10, 20]);

    // A live event folds in at the tail.
    handlers["sync:io_throughput"](sample(3, 300, 30));
    expect(store.diskSeries).toEqual([100, 200, 300]);
    expect(store.netSeries).toEqual([10, 20, 30]);
  });

  it("caps the window and computes headline rates over the last samples", async () => {
    invokeMock.mockResolvedValueOnce({ bucketMs: 1000, samples: [] });
    const store = useIostatStore();
    await store.start();

    // Before any sample the rates are unknown -> the tiles show empty states.
    expect(store.diskRate).toBeNull();
    expect(store.netRate).toBeNull();

    for (let i = 0; i < IO_WINDOW_SAMPLES + 25; i++) {
      handlers["sync:io_throughput"](sample(i, 1000, 500));
    }
    expect(store.samples.length).toBe(IO_WINDOW_SAMPLES);

    // Constant 1000 B per 1s bucket -> 1000 B/s over the headline window.
    expect(store.diskRate).toBe(1000);
    expect(store.netRate).toBe(500);

    // A quiet tail decays the headline: the last IO_HEADLINE_SAMPLES buckets
    // average in the zeros.
    for (let i = 0; i < IO_HEADLINE_SAMPLES - 1; i++) {
      handlers["sync:io_throughput"](sample(1000 + i, 0, 0));
    }
    expect(store.diskRate).toBe(1000 / IO_HEADLINE_SAMPLES);
    expect(store.netRate).toBe(500 / IO_HEADLINE_SAMPLES);
  });

  it("coerces garbled wire samples to zeros and survives a failed seed", async () => {
    invokeMock.mockRejectedValueOnce(new Error("ipc down"));
    const store = useIostatStore();
    await store.start();
    // The live stream still fills the charts after a failed seed.
    handlers["sync:io_throughput"]({ tsMs: "nope", diskBytes: -5, netBytes: Number.NaN });
    expect(store.samples).toEqual([sample(0, 0, 0)]);
  });

  it("stop() unsubscribes and start() is idempotent", async () => {
    invokeMock.mockResolvedValue({ bucketMs: 1000, samples: [] });
    const store = useIostatStore();
    await store.start();
    await store.start();
    expect(listenMock).toHaveBeenCalledTimes(1);
    store.stop();
    expect(unlistenMock).toHaveBeenCalledTimes(1);
    expect(handlers["sync:io_throughput"]).toBeUndefined();
  });
});
