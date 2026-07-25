// Webview console capture.
//
// Backend `tracing::` output reaches the rolling `driven.*.log` files and so the
// diagnostic bundle, but the WEBVIEW's own output went nowhere: an installed
// build has no devtools open, so a `console.error` from a store, an uncaught
// exception, or a rejected promise vanished the moment it happened. A bundle
// from a user reporting "the Restore tab just spins" carried no evidence of the
// UI-side failure at all.
//
// This module wraps `console.log/info/warn/error` (always calling through to the
// original first, so devtools behave exactly as before), plus `window.onerror`
// and `unhandledrejection`, buffers entries in a BOUNDED ring, and periodically
// ships batches to the `report_frontend_logs` command, which re-emits them under
// the `driven::frontend` tracing target. Frontend and backend lines then
// interleave in one timeline in one file.
//
// Everything here is defensive about its own failure modes, because a logger
// that misbehaves is worse than no logger:
//
//   - the ring is capped (oldest dropped, drops counted) so a `console.log` in a
//     hot loop can never grow memory without bound;
//   - a failed IPC re-queues a batch AT MOST ONCE and then drops it, so an
//     unavailable backend cannot produce an ever-growing retry backlog or a
//     tight failure loop;
//   - nothing in this file calls `console.*`, so a fault in the shipping path
//     cannot feed itself.

import { reportFrontendLogs } from "./ipc/commands";
import type { FrontendLogEntryDto, FrontendLogLevel } from "./ipc/types";

/** Maximum entries held pending a flush. Beyond this the OLDEST are dropped:
 * during a burst the newest lines are the ones near the failure. */
export const MAX_PENDING = 500;

/** Maximum characters per entry, matching the backend's own cap so the common
 * case needs no server-side truncation. */
export const MAX_TEXT_CHARS = 2000;

/** Maximum entries per IPC call, matching the backend's hard limit (which
 * REJECTS an over-long batch outright). */
export const MAX_BATCH = 200;

/** Periodic flush cadence. Slow enough that idle chatter costs ~12 IPC calls a
 * minute; fast enough that a crash a few seconds later still has the context. */
export const FLUSH_INTERVAL_MS = 5000;

/** Pending count at which a flush is triggered immediately rather than waiting
 * for the timer, so a sudden burst is not held hostage by the 5s cadence. */
export const FLUSH_EAGER_AT = 100;

/** A pending entry plus the retry bookkeeping that keeps requeueing bounded. */
interface PendingEntry extends FrontendLogEntryDto {
  /** True once this entry has survived one failed flush. A second failure drops
   * it: retrying forever would turn a broken IPC into an infinite loop. */
  requeued: boolean;
}

/** How a batch is delivered. Injectable so the buffer is testable without Tauri. */
export type SendFn = (entries: FrontendLogEntryDto[]) => Promise<void>;

/** Collapse one console argument into a string. Objects go through JSON so a
 * logged payload is readable rather than `[object Object]`; a circular or
 * otherwise unserialisable value falls back to `String(value)` rather than
 * throwing inside the console wrapper. */
export function formatLogArg(value: unknown): string {
  if (typeof value === "string") return value;
  if (value instanceof Error) {
    return value.stack
      ? `${value.name}: ${value.message}\n${value.stack}`
      : `${value.name}: ${value.message}`;
  }
  if (value === null) return "null";
  if (value === undefined) return "undefined";
  if (typeof value === "object") {
    try {
      return JSON.stringify(value);
    } catch {
      return String(value);
    }
  }
  return String(value);
}

/** Join console arguments the way the console itself renders them. */
export function formatLogArgs(args: unknown[]): string {
  return args.map(formatLogArg).join(" ");
}

/** Truncate to [MAX_TEXT_CHARS] with an explicit marker, so a reader can tell a
 * clipped line from a complete one. Uses the spread operator to split by code
 * POINT, so a surrogate pair (emoji, some CJK) is never cut in half. */
export function truncateText(text: string, max = MAX_TEXT_CHARS): string {
  const chars = [...text];
  if (chars.length <= max) return text;
  return `${chars.slice(0, max).join("")}...[truncated]`;
}

/**
 * The bounded ring plus the flush policy. Separated from the console wrapping
 * so the buffering, batching, and retry rules can be tested directly with a
 * stub `send` and fake timers.
 */
export class FrontendLogBuffer {
  private pending: PendingEntry[] = [];
  private dropped = 0;
  private flushing = false;
  private timer: ReturnType<typeof setInterval> | null = null;

  constructor(
    private readonly send: SendFn,
    private readonly maxPending = MAX_PENDING,
    private readonly maxBatch = MAX_BATCH,
    /** Pending count that triggers an immediate flush. `Infinity` disables the
     * eager path, which is how a test exercises overflow or multi-batch
     * draining without the threshold quietly draining the ring mid-loop. */
    private readonly eagerAt: number = FLUSH_EAGER_AT
  ) {}

  /** Entries waiting to be shipped. */
  get pendingCount(): number {
    return this.pending.length;
  }

  /** Entries discarded because the ring was full. Reported once, on the next
   * successful flush, so the gap in the log is visible rather than silent. */
  get droppedCount(): number {
    return this.dropped;
  }

  /** Buffer one entry. Returns true when it was queued, false when the ring was
   * full and the oldest entry had to be evicted to make room. */
  push(level: FrontendLogLevel, text: string, ts: number = Date.now()): boolean {
    const entry: PendingEntry = { level, ts, text: truncateText(text), requeued: false };
    this.pending.push(entry);
    let evicted = false;
    while (this.pending.length > this.maxPending) {
      this.pending.shift();
      this.dropped += 1;
      evicted = true;
    }
    if (this.pending.length >= this.eagerAt) {
      // Fire and forget: `flush` swallows its own errors, and awaiting here
      // would make every `console.log` in a burst an async call.
      void this.flush();
    }
    return !evicted;
  }

  /**
   * Ship up to `maxBatch` of the oldest pending entries.
   *
   * Re-entrancy-safe (a concurrent call returns immediately) so the eager-flush
   * path and the timer cannot interleave and send the same entries twice. On a
   * send failure the batch is put back at the FRONT of the ring - preserving
   * order - but only for entries that have not already been retried once.
   */
  async flush(): Promise<void> {
    if (this.flushing || this.pending.length === 0) return;
    this.flushing = true;
    try {
      const droppedBefore = this.dropped;
      // The overflow note below occupies one slot in the payload. Reserve it
      // here, because the backend REJECTS a batch over its 200-entry cap - a
      // 201st entry would lose the whole batch, not just the note.
      const batch = this.pending.splice(0, this.maxBatch - (droppedBefore > 0 ? 1 : 0));
      const payload: FrontendLogEntryDto[] = batch.map(({ level, ts, text }) => ({
        level,
        ts,
        text,
      }));
      if (droppedBefore > 0) {
        // Record the gap IN the log rather than losing it. Appended to the batch
        // so it is ordered after the entries that survived.
        payload.push({
          level: "warn",
          ts: Date.now(),
          text: `frontend log buffer overflowed: ${droppedBefore} entr${
            droppedBefore === 1 ? "y" : "ies"
          } dropped`,
        });
      }
      try {
        await this.send(payload);
        // Only clear the counter for drops we actually reported, so drops that
        // happened DURING the await are still reported next time.
        this.dropped -= droppedBefore;
      } catch {
        // Never `console.error` here - this module wraps the console, and doing
        // so would re-enter the very buffer whose flush just failed.
        const retryable = batch.filter((e) => !e.requeued);
        for (const entry of retryable) entry.requeued = true;
        this.pending.unshift(...retryable);
        while (this.pending.length > this.maxPending) {
          this.pending.pop();
          this.dropped += 1;
        }
      }
    } finally {
      this.flushing = false;
    }
  }

  /** Start the periodic flush timer. Idempotent. */
  start(intervalMs = FLUSH_INTERVAL_MS): void {
    if (this.timer !== null) return;
    this.timer = setInterval(() => {
      void this.flush();
    }, intervalMs);
  }

  /** Stop the periodic flush timer. Does NOT flush - call `flush()` for that. */
  stop(): void {
    if (this.timer === null) return;
    clearInterval(this.timer);
    this.timer = null;
  }
}

/** True when a Tauri backend is present to receive the batches.
 *
 * Tauri v2 injects `__TAURI_INTERNALS__` onto `window` before any app code
 * runs. Under vitest (and in a plain browser) it is absent, so capture installs
 * as a no-op: unit tests must not have their console swallowed, and must not
 * fire IPC at a backend that is not there. */
export function isTauriAvailable(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/** The console methods this module wraps, and the level each maps to.
 * `console.log` maps to `info` rather than `debug` so it survives the backend's
 * default `info` filter - it is by far the most common call and dropping it
 * would defeat the purpose. */
const WRAPPED: ReadonlyArray<readonly ["log" | "info" | "warn" | "error", FrontendLogLevel]> = [
  ["log", "info"],
  ["info", "info"],
  ["warn", "warn"],
  ["error", "error"],
];

/** The process-wide buffer, created by `installFrontendLogCapture`. Null until
 * install (and permanently null outside Tauri), which is what makes
 * `flushFrontendLogs` a safe no-op in tests and in a browser. */
let active: FrontendLogBuffer | null = null;

/**
 * Install the console wrappers, the global error handlers, and the flush timer.
 *
 * No-op (returning a no-op uninstall) when no Tauri backend is present.
 * Idempotent: a second call while already installed does nothing. Returns an
 * uninstall function that restores the original console methods and stops the
 * timer - used by the tests, and available should a caller ever need it.
 */
export function installFrontendLogCapture(options?: {
  /** Override the backend availability check. Tests use this to exercise the
   * real wiring without a Tauri runtime. */
  available?: boolean;
  /** Override the delivery function. Tests use this to observe batches. */
  send?: SendFn;
  /** Override the flush cadence. */
  intervalMs?: number;
}): () => void {
  const available = options?.available ?? isTauriAvailable();
  if (!available || active !== null) return () => {};

  const buffer = new FrontendLogBuffer(options?.send ?? ((entries) => reportFrontendLogs(entries)));
  active = buffer;

  // The ORIGINAL function references, not bound copies: uninstall must restore
  // the exact same object it replaced, or a caller that captured `console.log`
  // beforehand (and any test asserting identity) sees a stranger afterwards.
  const originals = new Map<string, (...args: unknown[]) => void>();
  for (const [method, level] of WRAPPED) {
    const original = console[method] as (...args: unknown[]) => void;
    originals.set(method, original);
    console[method] = (...args: unknown[]): void => {
      // Call through FIRST so devtools show the line even if capture throws.
      original.apply(console, args);
      buffer.push(level, formatLogArgs(args));
    };
  }

  const onError = (event: ErrorEvent): void => {
    const where = event.filename ? ` (${event.filename}:${event.lineno}:${event.colno})` : "";
    const detail = event.error instanceof Error ? formatLogArg(event.error) : event.message;
    buffer.push("error", `uncaught error: ${detail}${where}`);
  };
  const onRejection = (event: PromiseRejectionEvent): void => {
    buffer.push("error", `unhandled promise rejection: ${formatLogArg(event.reason)}`);
  };
  if (typeof window !== "undefined") {
    window.addEventListener("error", onError);
    window.addEventListener("unhandledrejection", onRejection);
  }

  buffer.start(options?.intervalMs);

  return () => {
    buffer.stop();
    for (const [method] of WRAPPED) {
      const original = originals.get(method);
      if (original) {
        console[method] = original as typeof console.log;
      }
    }
    if (typeof window !== "undefined") {
      window.removeEventListener("error", onError);
      window.removeEventListener("unhandledrejection", onRejection);
    }
    if (active === buffer) active = null;
  };
}

/**
 * Ship everything currently buffered, right now.
 *
 * Awaited before exporting a diagnostic bundle so the console lines leading up
 * to the user's "this is broken, here is a bundle" moment are IN that bundle
 * rather than still sitting in the ring. Loops until the ring drains (or stops
 * draining) because one flush moves at most one batch. Never throws - a failed
 * flush must not block the export the user actually asked for.
 */
export async function flushFrontendLogs(): Promise<void> {
  const buffer = active;
  if (buffer === null) return;
  // Bounded: MAX_PENDING / MAX_BATCH rounded up, plus slack for a requeue.
  const maxRounds = Math.ceil(MAX_PENDING / MAX_BATCH) + 2;
  for (let i = 0; i < maxRounds; i += 1) {
    const before = buffer.pendingCount;
    if (before === 0) return;
    await buffer.flush();
    // A flush that moved nothing (send failed and everything was requeued)
    // means retrying is pointless; stop rather than spin.
    if (buffer.pendingCount >= before) return;
  }
}
