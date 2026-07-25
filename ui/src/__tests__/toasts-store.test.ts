import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { createPinia, setActivePinia } from "pinia";

// Toast store unit tests. The store is pure state + `setTimeout` bookkeeping, so
// these drive it with fake timers and no DOM: auto-dismiss at the kind's default
// timeout, manual dismiss, the visible cap + oldest-first eviction, the
// consecutive-duplicate fold, and the hover pause/resume banking the REMAINING
// time rather than restarting it.

import {
  DEDUPE_WINDOW_MS,
  DEFAULT_TIMEOUT_MS,
  ERROR_TIMEOUT_MS,
  MAX_VISIBLE,
  useToastsStore,
} from "../stores/toasts";

function store() {
  setActivePinia(createPinia());
  return useToastsStore();
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("toasts store", () => {
  it("pushes an info toast by default and returns its id", () => {
    const toasts = store();
    const id = toasts.push({ message: "hello" });

    expect(toasts.toasts).toHaveLength(1);
    expect(toasts.toasts[0]).toMatchObject({ id, kind: "info", message: "hello" });
    expect(toasts.any).toBe(true);
  });

  it("auto-dismisses after the default timeout", async () => {
    const toasts = store();
    toasts.push({ kind: "success", message: "saved" });

    await vi.advanceTimersByTimeAsync(DEFAULT_TIMEOUT_MS - 1);
    expect(toasts.toasts).toHaveLength(1);

    await vi.advanceTimersByTimeAsync(1);
    expect(toasts.toasts).toHaveLength(0);
    expect(toasts.any).toBe(false);
  });

  it("keeps an error toast up for the longer error timeout", async () => {
    const toasts = store();
    toasts.push({ kind: "error", message: "it broke" });

    await vi.advanceTimersByTimeAsync(DEFAULT_TIMEOUT_MS);
    expect(toasts.toasts).toHaveLength(1);

    await vi.advanceTimersByTimeAsync(ERROR_TIMEOUT_MS - DEFAULT_TIMEOUT_MS);
    expect(toasts.toasts).toHaveLength(0);
  });

  it("honors an explicit timeoutMs over the kind default", async () => {
    const toasts = store();
    toasts.push({ message: "brief", timeoutMs: 1_000 });

    await vi.advanceTimersByTimeAsync(1_000);
    expect(toasts.toasts).toHaveLength(0);
  });

  it("dismisses on demand and does not fire the timer afterwards", async () => {
    const toasts = store();
    const id = toasts.push({ message: "hello" });
    toasts.dismiss(id);

    expect(toasts.toasts).toHaveLength(0);
    // The cleared timer must not resurrect anything or throw when its original
    // deadline passes.
    await vi.advanceTimersByTimeAsync(DEFAULT_TIMEOUT_MS * 2);
    expect(toasts.toasts).toHaveLength(0);
  });

  it("caps the stack and evicts the OLDEST toast", () => {
    const toasts = store();
    for (let i = 0; i < MAX_VISIBLE + 2; i++) toasts.push({ message: `m${i}` });

    expect(toasts.toasts).toHaveLength(MAX_VISIBLE);
    // The two oldest are gone; the newest is last (the stack is oldest-first).
    expect(toasts.toasts.map((toast) => toast.message)).toEqual(["m2", "m3", "m4", "m5"]);
  });

  it("folds an identical consecutive message into the showing toast", async () => {
    const toasts = store();
    const first = toasts.push({ kind: "success", message: "Settings saved" });

    await vi.advanceTimersByTimeAsync(DEDUPE_WINDOW_MS - 500);
    const second = toasts.push({ kind: "success", message: "Settings saved" });

    expect(second).toBe(first);
    expect(toasts.toasts).toHaveLength(1);
    // The repeat RESTARTS the timer, so the toast outlives the original deadline.
    await vi.advanceTimersByTimeAsync(DEFAULT_TIMEOUT_MS - 1);
    expect(toasts.toasts).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(toasts.toasts).toHaveLength(0);
  });

  it("stacks a repeat once the dedupe window has passed", async () => {
    const toasts = store();
    const first = toasts.push({ message: "Backup started", timeoutMs: 60_000 });

    await vi.advanceTimersByTimeAsync(DEDUPE_WINDOW_MS + 1);
    const second = toasts.push({ message: "Backup started", timeoutMs: 60_000 });

    expect(second).not.toBe(first);
    expect(toasts.toasts).toHaveLength(2);
  });

  it("does not fold a repeat that is not the NEWEST toast", () => {
    const toasts = store();
    toasts.push({ message: "a" });
    toasts.push({ message: "b" });
    toasts.push({ message: "a" });

    expect(toasts.toasts.map((toast) => toast.message)).toEqual(["a", "b", "a"]);
  });

  it("pauses the dismissal timer while hovered and banks the remaining time", async () => {
    const toasts = store();
    const id = toasts.push({ message: "read me" });

    await vi.advanceTimersByTimeAsync(2_000);
    toasts.pause(id);

    // Time passing while paused must not dismiss it, however long.
    await vi.advanceTimersByTimeAsync(DEFAULT_TIMEOUT_MS * 3);
    expect(toasts.toasts).toHaveLength(1);

    toasts.resume(id);
    // Only the REMAINING 3s are left - a resume does not restart the full 5s.
    await vi.advanceTimersByTimeAsync(DEFAULT_TIMEOUT_MS - 2_000 - 1);
    expect(toasts.toasts).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(toasts.toasts).toHaveLength(0);
  });

  it("ignores a double pause so overlapping hover + focus cannot double-bank", async () => {
    const toasts = store();
    const id = toasts.push({ message: "read me" });

    await vi.advanceTimersByTimeAsync(2_000);
    toasts.pause(id);
    await vi.advanceTimersByTimeAsync(1_000);
    toasts.pause(id);
    toasts.resume(id);

    await vi.advanceTimersByTimeAsync(DEFAULT_TIMEOUT_MS - 2_000);
    expect(toasts.toasts).toHaveLength(0);
  });

  it("clears every toast and its timer", async () => {
    const toasts = store();
    toasts.push({ message: "a" });
    toasts.push({ message: "b" });
    toasts.clear();

    expect(toasts.toasts).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(DEFAULT_TIMEOUT_MS * 2);
    expect(toasts.toasts).toHaveLength(0);
  });
});
