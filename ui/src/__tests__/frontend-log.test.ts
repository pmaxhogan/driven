// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// The buffer's delivery function is injectable, so most of this file drives
// `FrontendLogBuffer` directly with a stub. The install path additionally needs
// the IPC seam mocked because it defaults to the real `reportFrontendLogs`.
const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

import {
  FLUSH_INTERVAL_MS,
  FrontendLogBuffer,
  MAX_BATCH,
  MAX_PENDING,
  MAX_TEXT_CHARS,
  flushFrontendLogs,
  formatLogArgs,
  installFrontendLogCapture,
  isTauriAvailable,
  truncateText,
} from "../frontendLog";
import type { FrontendLogEntryDto } from "../ipc/types";

describe("frontend log formatting", () => {
  it("joins console arguments the way the console renders them", () => {
    expect(formatLogArgs(["a", 1, true])).toBe("a 1 true");
  });

  it("serialises objects rather than printing [object Object]", () => {
    expect(formatLogArgs([{ a: 1 }])).toBe('{"a":1}');
  });

  it("falls back to String() for a circular object instead of throwing", () => {
    const circular: Record<string, unknown> = {};
    circular.self = circular;
    expect(formatLogArgs([circular])).toBe("[object Object]");
  });

  it("keeps an Error's name, message, and stack", () => {
    const err = new Error("boom");
    const text = formatLogArgs([err]);
    expect(text).toContain("Error: boom");
  });

  it("renders null and undefined distinctly", () => {
    expect(formatLogArgs([null, undefined])).toBe("null undefined");
  });

  it("leaves a text at exactly the cap untouched", () => {
    const exact = "x".repeat(MAX_TEXT_CHARS);
    expect(truncateText(exact)).toBe(exact);
  });

  it("truncates a longer text with an explicit marker", () => {
    const long = "x".repeat(MAX_TEXT_CHARS + 100);
    const out = truncateText(long);
    expect(out.endsWith("...[truncated]")).toBe(true);
    expect([...out].length).toBe(MAX_TEXT_CHARS + "...[truncated]".length);
  });

  it("truncates by code point so a surrogate pair is never split", () => {
    const long = "\u{1f4be}".repeat(MAX_TEXT_CHARS + 10);
    const out = truncateText(long);
    // A naive `slice` would leave a lone surrogate here.
    expect(out.startsWith("\u{1f4be}")).toBe(true);
    expect(out).not.toContain("�");
    expect([...out].length).toBe(MAX_TEXT_CHARS + "...[truncated]".length);
  });
});

describe("FrontendLogBuffer", () => {
  it("batches pending entries into one send", async () => {
    const sent: FrontendLogEntryDto[][] = [];
    const buffer = new FrontendLogBuffer(async (entries) => {
      sent.push(entries);
    });
    buffer.push("info", "one", 1);
    buffer.push("warn", "two", 2);
    await buffer.flush();

    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual([
      { level: "info", ts: 1, text: "one" },
      { level: "warn", ts: 2, text: "two" },
    ]);
    expect(buffer.pendingCount).toBe(0);
  });

  it("splits a batch larger than the backend's per-call cap", async () => {
    const sent: FrontendLogEntryDto[][] = [];
    // Eager flushing disabled so the ring is observed at a known size rather
    // than being drained mid-loop by the 100-entry threshold.
    const buffer = new FrontendLogBuffer(
      async (entries) => {
        sent.push(entries);
      },
      MAX_PENDING,
      MAX_BATCH,
      Infinity
    );
    for (let i = 0; i < MAX_BATCH + 5; i += 1) buffer.push("info", `line ${i}`, i);

    await buffer.flush();
    // The backend REJECTS an over-long batch, so the first send must be exactly
    // at the cap, with the remainder left pending for the next round.
    expect(sent[0]).toHaveLength(MAX_BATCH);
    expect(buffer.pendingCount).toBe(5);

    await buffer.flush();
    expect(sent[1]).toHaveLength(5);
    expect(buffer.pendingCount).toBe(0);
  });

  it("truncates each entry as it is buffered", async () => {
    const sent: FrontendLogEntryDto[][] = [];
    const buffer = new FrontendLogBuffer(async (entries) => {
      sent.push(entries);
    });
    buffer.push("info", "y".repeat(MAX_TEXT_CHARS + 50), 1);
    await buffer.flush();
    expect(sent[0][0].text.endsWith("...[truncated]")).toBe(true);
  });

  it("drops the OLDEST entries when the ring overflows and reports the gap", async () => {
    const sent: FrontendLogEntryDto[][] = [];
    // Eager flushing disabled so the ring genuinely overflows instead of being
    // drained on the way past 100.
    const buffer = new FrontendLogBuffer(
      async (entries) => {
        sent.push(entries);
      },
      MAX_PENDING,
      MAX_BATCH,
      Infinity
    );
    for (let i = 0; i < MAX_PENDING + 10; i += 1) buffer.push("info", `line ${i}`, i);

    // Bounded: memory cannot grow with a console.log in a hot loop.
    expect(buffer.pendingCount).toBe(MAX_PENDING);
    expect(buffer.droppedCount).toBe(10);

    // Drain fully, then assert the overflow was RECORDED rather than silently
    // swallowed - a gap you cannot see is worse than one you can.
    while (buffer.pendingCount > 0) await buffer.flush();
    // No send may exceed the backend's per-call cap, INCLUDING the appended
    // overflow note - the backend rejects an over-long batch outright.
    for (const call of sent) expect(call.length).toBeLessThanOrEqual(MAX_BATCH);
    const texts = sent.flat().map((e) => e.text);
    expect(texts.some((t) => t.includes("overflowed"))).toBe(true);
    // The newest line survived; the very oldest did not.
    expect(texts).toContain(`line ${MAX_PENDING + 9}`);
    expect(texts).not.toContain("line 0");
  });

  it("re-queues a failed batch exactly once, then drops it", async () => {
    let attempts = 0;
    const buffer = new FrontendLogBuffer(async () => {
      attempts += 1;
      throw new Error("ipc down");
    });
    buffer.push("info", "keep me", 1);

    await buffer.flush();
    expect(attempts).toBe(1);
    // Requeued at the front, so ordering is preserved for the retry.
    expect(buffer.pendingCount).toBe(1);

    await buffer.flush();
    expect(attempts).toBe(2);
    // Second failure drops it: a broken IPC must not accumulate a backlog or
    // spin retrying forever.
    expect(buffer.pendingCount).toBe(0);

    await buffer.flush();
    expect(attempts).toBe(2);
  });

  it("keeps the dropped counter when the reporting flush itself fails", async () => {
    const buffer = new FrontendLogBuffer(
      async () => {
        throw new Error("ipc down");
      },
      MAX_PENDING,
      MAX_BATCH,
      Infinity
    );
    for (let i = 0; i < MAX_PENDING + 3; i += 1) buffer.push("info", `line ${i}`, i);
    await buffer.flush();
    // The overflow report went out with a batch that never landed, so the
    // counter must NOT have been cleared - the gap is still unreported.
    expect(buffer.droppedCount).toBeGreaterThanOrEqual(3);
  });

  it("does nothing on flush when there is nothing pending", async () => {
    const send = vi.fn(async () => {});
    const buffer = new FrontendLogBuffer(send);
    await buffer.flush();
    expect(send).not.toHaveBeenCalled();
  });

  it("flushes on the timer", async () => {
    vi.useFakeTimers();
    try {
      const send = vi.fn(async () => {});
      const buffer = new FrontendLogBuffer(send);
      buffer.start();
      buffer.push("info", "tick", 1);
      expect(send).not.toHaveBeenCalled();

      await vi.advanceTimersByTimeAsync(FLUSH_INTERVAL_MS);
      expect(send).toHaveBeenCalledTimes(1);

      // Stop must actually stop: no further sends once the timer is cleared.
      buffer.stop();
      buffer.push("info", "after stop", 2);
      await vi.advanceTimersByTimeAsync(FLUSH_INTERVAL_MS * 3);
      expect(send).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("flushes eagerly once the buffer is large, without waiting for the timer", async () => {
    vi.useFakeTimers();
    try {
      const send = vi.fn(async () => {});
      const buffer = new FrontendLogBuffer(send);
      buffer.start();
      for (let i = 0; i < 100; i += 1) buffer.push("info", `burst ${i}`, i);
      // No timer advance at all - the eager threshold did the work.
      await vi.advanceTimersByTimeAsync(0);
      expect(send).toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });
});

describe("installFrontendLogCapture", () => {
  let uninstall: (() => void) | null = null;

  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
  });

  afterEach(() => {
    uninstall?.();
    uninstall = null;
    vi.restoreAllMocks();
  });

  it("is a no-op without a Tauri backend, so vitest keeps its console", () => {
    // jsdom has no __TAURI_INTERNALS__, which is exactly the guard.
    expect(isTauriAvailable()).toBe(false);
    const before = console.log;
    const off = installFrontendLogCapture();
    expect(console.log).toBe(before);
    off();
  });

  it("passes console calls through to the original method", () => {
    const original = vi.spyOn(console, "warn").mockImplementation(() => {});
    uninstall = installFrontendLogCapture({ available: true, send: async () => {} });
    console.warn("still visible", 42);
    expect(original).toHaveBeenCalledWith("still visible", 42);
  });

  it("captures console output at the mapped level", async () => {
    const sent: FrontendLogEntryDto[][] = [];
    vi.spyOn(console, "log").mockImplementation(() => {});
    vi.spyOn(console, "error").mockImplementation(() => {});
    uninstall = installFrontendLogCapture({
      available: true,
      send: async (entries) => {
        sent.push(entries);
      },
    });

    console.log("plain");
    console.error("bad");
    await flushFrontendLogs();

    const entries = sent.flat();
    // console.log maps to info (not debug) so it survives the backend's default
    // info-level filter.
    expect(entries).toContainEqual(expect.objectContaining({ level: "info", text: "plain" }));
    expect(entries).toContainEqual(expect.objectContaining({ level: "error", text: "bad" }));
  });

  it("captures uncaught errors and unhandled rejections", async () => {
    const sent: FrontendLogEntryDto[][] = [];
    uninstall = installFrontendLogCapture({
      available: true,
      send: async (entries) => {
        sent.push(entries);
      },
    });

    window.dispatchEvent(
      new ErrorEvent("error", { message: "kaboom", filename: "app.js", lineno: 7, colno: 3 })
    );
    window.dispatchEvent(
      new (class extends Event {
        reason = "nope";
      })("unhandledrejection") as PromiseRejectionEvent
    );
    await flushFrontendLogs();

    const texts = sent.flat().map((e) => e.text);
    expect(texts.some((t) => t.includes("uncaught error: kaboom (app.js:7:3)"))).toBe(true);
    expect(texts.some((t) => t.includes("unhandled promise rejection: nope"))).toBe(true);
  });

  it("restores the original console methods on uninstall", () => {
    const before = console.log;
    const off = installFrontendLogCapture({ available: true, send: async () => {} });
    expect(console.log).not.toBe(before);
    off();
    expect(console.log).toBe(before);
  });

  it("routes captured entries to the report_frontend_logs command by default", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => {});
    uninstall = installFrontendLogCapture({ available: true });
    console.warn("to the backend");
    await flushFrontendLogs();

    expect(invokeMock).toHaveBeenCalledWith("report_frontend_logs", {
      entries: [expect.objectContaining({ level: "warn", text: "to the backend" })],
    });
  });

  it("flushFrontendLogs is a safe no-op when capture was never installed", async () => {
    await expect(flushFrontendLogs()).resolves.toBeUndefined();
  });

  it("flushFrontendLogs does not spin when every send fails", async () => {
    let attempts = 0;
    // Spy BEFORE installing: spying afterwards would replace the capture
    // wrapper outright and nothing would ever be buffered.
    vi.spyOn(console, "log").mockImplementation(() => {});
    uninstall = installFrontendLogCapture({
      available: true,
      send: async () => {
        attempts += 1;
        throw new Error("ipc down");
      },
    });
    console.log("one");

    await flushFrontendLogs();
    // One attempt: the entry was requeued (pending did not shrink), so the loop
    // stops rather than retrying until the round cap.
    expect(attempts).toBe(1);
  });
});
