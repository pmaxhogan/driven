import { defineStore } from "pinia";
import { ref } from "vue";
import type { UnlistenFn } from "@tauri-apps/api/event";

import * as ipc from "../ipc/commands";
import { onSyncBottleneck } from "../ipc/events";
import type { BottleneckSnapshot, BottleneckState } from "../ipc/types";

/**
 * Live bottleneck-classification store (issue #308). The backend classifies
 * the limiting pipeline stage once a second; this store seeds from
 * `bottleneck_status` and then folds each `sync:bottleneck` event, same
 * subscribe()/hydrate() shape as `stores/iostat.ts`.
 *
 * DEBOUNCE + HYSTERESIS: the backend's classifier is a pure per-tick function
 * with no memory (driven-core intentionally leaves flap-smoothing to the
 * caller), so a state a few B/s from a threshold can genuinely flip tick to
 * tick. Rather than repaint the tile every second, `displayed` only adopts a
 * NEW state once that state has been the raw incoming value continuously for
 * `DEBOUNCE_MS` (5s) - any state change resets the window, so brief flapping
 * between two states never reaches the UI. The window is measured off the
 * snapshots' own `tsMs` (backend wall-clock), not `Date.now()`, so this is
 * deterministic under test with no fake timers.
 */

/** How long a new state must hold before the tile adopts it. */
export const DEBOUNCE_MS = 5000;

/** Read a finite non-negative number, or null - untrusted wire data. */
function numOrNull(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : null;
}

function num(v: unknown): number {
  return typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : 0;
}

const VALID_STATES: readonly BottleneckState[] = [
  "not_backing_up",
  "disk",
  "network",
  "api",
  "cpu",
  "mixed",
];

/** Coerce one wire snapshot (missing/garbled fields degrade to safe
 * defaults rather than throwing, matching `stores/iostat.ts`'s `readSample`). */
function readSnapshot(s: unknown): BottleneckSnapshot {
  const o = (s ?? {}) as Record<string, unknown>;
  const state = VALID_STATES.includes(o["state"] as BottleneckState)
    ? (o["state"] as BottleneckState)
    : "not_backing_up";
  return {
    tsMs: num(o["tsMs"]),
    state,
    rateBytesPerSec: numOrNull(o["rateBytesPerSec"]),
    backend: typeof o["backend"] === "string" ? (o["backend"] as string) : null,
    backoffRemainingMs: numOrNull(o["backoffRemainingMs"]),
  };
}

export const useBottleneckStore = defineStore("bottleneck", () => {
  /** The debounced value the tile renders. Null until the first snapshot
   * arrives (hydrate or a live event), so the tile can show its own loading
   * state rather than a misleading default. */
  const displayed = ref<BottleneckSnapshot | null>(null);

  // The raw incoming state being timed for the debounce window - NOT
  // reactive (it is an implementation detail of `apply`, not something a
  // consumer should render mid-debounce).
  let pending: BottleneckSnapshot | null = null;
  let pendingSinceMs: number | null = null;

  /** Fold one incoming snapshot (hydrate seed or live event) through the
   * debounce/hysteresis gate. */
  function apply(snapshot: BottleneckSnapshot): void {
    if (displayed.value === null) {
      // Nothing shown yet: adopt immediately (a hydration read is already a
      // stable point-in-time read, not a tick that might flap) and seed the
      // debounce window so subsequent flapping is judged against it.
      displayed.value = snapshot;
      pending = snapshot;
      pendingSinceMs = snapshot.tsMs;
      return;
    }
    if (pending === null || pending.state !== snapshot.state) {
      // A new candidate state: start (or restart) its debounce window.
      pending = snapshot;
      pendingSinceMs = snapshot.tsMs;
      return;
    }
    // Same candidate state as before: keep its numbers fresh, and promote it
    // to `displayed` once it has held for the whole debounce window.
    pending = snapshot;
    if (pendingSinceMs !== null && snapshot.tsMs - pendingSinceMs >= DEBOUNCE_MS) {
      displayed.value = snapshot;
    }
  }

  // --- lifecycle (the Activity view owns the registration) ------------------
  let unlisten: UnlistenFn | null = null;
  let started = false;

  /** Seed from the latest snapshot + subscribe to live updates (idempotent).
   * Best-effort: a failed seed still leaves the live stream to populate the
   * tile. */
  async function start(): Promise<void> {
    if (started) return;
    started = true;
    try {
      apply(readSnapshot(await ipc.bottleneckStatus()));
    } catch (e) {
      console.error("bottleneck status seed failed", e);
    }
    try {
      const un = await onSyncBottleneck((snapshot) => apply(readSnapshot(snapshot)));
      // stop() may have raced ahead while we awaited; honor it.
      if (!started) {
        un();
        return;
      }
      unlisten = un;
    } catch (e) {
      started = false;
      console.error("bottleneck subscribe failed", e);
    }
  }

  /** Stop the live subscription (view unmount). */
  function stop(): void {
    started = false;
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
  }

  return {
    displayed,
    start,
    stop,
  };
});
