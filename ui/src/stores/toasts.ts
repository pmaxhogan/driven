import { defineStore } from "pinia";
import { computed, ref } from "vue";

/**
 * Transient in-app toast notifications (issue #9). A small, general-purpose
 * queue: anything in the app can `push()` a short message and it appears in the
 * corner stack rendered by `ToastHost.vue`, auto-dismissing after a timeout.
 *
 * Deliberate properties, all of them driven by "a backup app runs unattended and
 * must never turn into a wall of notifications":
 *
 *  - **Capacity.** At most `MAX_VISIBLE` toasts are on screen; a push past the
 *    cap evicts the OLDEST (its timer is cleared, so nothing leaks).
 *  - **Dedupe.** Pushing the same kind+message as the toast currently at the
 *    head of the stack within `DEDUPE_WINDOW_MS` does NOT stack a second copy -
 *    it restarts the existing toast's timer and returns its id. This is what
 *    keeps a burst of identical events (a user flipping a settings toggle back
 *    and forth, two accounts finishing at once) to a single visible toast.
 *  - **Hover pause.** The dismissal timer is paused while the pointer is over a
 *    toast (or focus is inside it), so a message can never expire out from under
 *    someone in the middle of reading it. Pausing preserves the REMAINING time
 *    rather than restarting, so a hover does not extend the toast's life.
 *
 * Timers are plain `setTimeout` handles held in a non-reactive `Map` keyed by
 * toast id - they are bookkeeping, not render state, so keeping them out of the
 * reactive graph avoids pointless re-renders on every pause/resume.
 */

/** Severity of a toast. Drives its color treatment and default timeout. */
export type ToastKind = "info" | "success" | "warning" | "error";

/** One toast in the stack, oldest first. */
export interface Toast {
  id: number;
  kind: ToastKind;
  message: string;
  /** The full dismissal timeout this toast was created with, in ms. */
  timeoutMs: number;
  /** Wall-clock ms at which the toast was pushed (dedupe window baseline). */
  createdAt: number;
}

/** Argument to `push()`. `kind` defaults to "info", `timeoutMs` to the
 * kind-appropriate default (errors linger longer - they matter more and are
 * usually longer to read). */
export interface PushToastOptions {
  kind?: ToastKind;
  message: string;
  timeoutMs?: number;
}

/** Most toasts on screen at once. Older ones are evicted, not queued: a stale
 * message is worth less than the newest one, and an unbounded stack would cover
 * the window. */
export const MAX_VISIBLE = 4;

/** Default auto-dismiss for info / success / warning toasts. */
export const DEFAULT_TIMEOUT_MS = 5_000;

/** Default auto-dismiss for error toasts (longer - more to read, more at stake). */
export const ERROR_TIMEOUT_MS = 8_000;

/** How recently an identical toast must have been pushed for a repeat to be
 * folded into it instead of stacking a second copy. */
export const DEDUPE_WINDOW_MS = 3_000;

/** The default timeout for a kind, when the caller does not pin one. */
function defaultTimeoutFor(kind: ToastKind): number {
  return kind === "error" ? ERROR_TIMEOUT_MS : DEFAULT_TIMEOUT_MS;
}

/** Non-reactive per-toast timer bookkeeping. `handle` is null while paused;
 * `remainingMs` is the time left to run once resumed. */
interface TimerEntry {
  handle: ReturnType<typeof setTimeout> | null;
  remainingMs: number;
  /** Wall-clock ms the currently-armed timeout started at (0 while paused). */
  startedAt: number;
}

export const useToastsStore = defineStore("toasts", () => {
  /** The visible stack, OLDEST first. `ToastHost` renders it in this order and
   * anchors the stack to the bottom of the window, so the newest toast is
   * nearest the corner. */
  const toasts = ref<Toast[]>([]);

  /** Monotonic id source. Ids are never reused, so a timer that fires for an
   * already-dismissed toast can only ever be a no-op. */
  let nextId = 1;

  const timers = new Map<number, TimerEntry>();

  /** Stop and forget a toast's timer (safe to call for an unknown id). */
  function clearTimer(id: number): void {
    const entry = timers.get(id);
    if (entry && entry.handle !== null) clearTimeout(entry.handle);
    timers.delete(id);
  }

  /** Arm (or re-arm) a toast's dismissal timer for `remainingMs`. */
  function armTimer(id: number, remainingMs: number): void {
    const handle = setTimeout(() => {
      timers.delete(id);
      remove(id);
    }, remainingMs);
    timers.set(id, { handle, remainingMs, startedAt: Date.now() });
  }

  /** Drop a toast from the stack (no timer bookkeeping - callers do that). */
  function remove(id: number): void {
    const index = toasts.value.findIndex((toast) => toast.id === id);
    if (index === -1) return;
    toasts.value = toasts.value.filter((toast) => toast.id !== id);
  }

  /** Whether `options` repeats the newest toast closely enough to fold into it.
   * Only the NEWEST toast is compared: two different messages interleaved are
   * genuinely different events and both deserve to be seen. */
  function isDuplicateOfNewest(kind: ToastKind, message: string, now: number): Toast | null {
    const newest = toasts.value[toasts.value.length - 1];
    if (!newest) return null;
    if (newest.kind !== kind || newest.message !== message) return null;
    return now - newest.createdAt <= DEDUPE_WINDOW_MS ? newest : null;
  }

  /**
   * Show a toast. Returns the id of the toast now showing the message - a NEW
   * id normally, or the EXISTING id when this push was folded into an identical
   * toast still inside the dedupe window (whose timer is restarted, so the
   * repeat does extend the message's visibility).
   */
  function push(options: PushToastOptions): number {
    const kind = options.kind ?? "info";
    const timeoutMs = options.timeoutMs ?? defaultTimeoutFor(kind);
    const now = Date.now();

    const duplicate = isDuplicateOfNewest(kind, options.message, now);
    if (duplicate) {
      duplicate.createdAt = now;
      clearTimer(duplicate.id);
      armTimer(duplicate.id, duplicate.timeoutMs);
      return duplicate.id;
    }

    const id = nextId++;
    const toast: Toast = { id, kind, message: options.message, timeoutMs, createdAt: now };
    const next = [...toasts.value, toast];
    // Evict from the FRONT (oldest) until we are back under the cap.
    while (next.length > MAX_VISIBLE) {
      const evicted = next.shift();
      if (evicted) clearTimer(evicted.id);
    }
    toasts.value = next;
    armTimer(id, timeoutMs);
    return id;
  }

  /** Dismiss a toast now (the X button, or programmatically). */
  function dismiss(id: number): void {
    clearTimer(id);
    remove(id);
  }

  /** Pause a toast's auto-dismiss (pointer over it / focus inside it), banking
   * the time it has left. A second pause is a no-op, so an overlapping
   * mouseenter + focusin cannot double-bank. */
  function pause(id: number): void {
    const entry = timers.get(id);
    if (!entry || entry.handle === null) return;
    clearTimeout(entry.handle);
    const elapsed = Date.now() - entry.startedAt;
    timers.set(id, {
      handle: null,
      remainingMs: Math.max(0, entry.remainingMs - elapsed),
      startedAt: 0,
    });
  }

  /** Resume a paused toast's auto-dismiss for whatever time it had left. */
  function resume(id: number): void {
    const entry = timers.get(id);
    if (!entry || entry.handle !== null) return;
    armTimer(id, entry.remainingMs);
  }

  /** Drop every toast and its timer (teardown / tests). */
  function clear(): void {
    for (const id of [...timers.keys()]) clearTimer(id);
    toasts.value = [];
  }

  /** True while anything is on screen - lets the host skip rendering entirely. */
  const any = computed<boolean>(() => toasts.value.length > 0);

  return { toasts, any, push, dismiss, pause, resume, clear };
});
