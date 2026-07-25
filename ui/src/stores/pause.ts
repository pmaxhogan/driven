import { defineStore } from "pinia";
import { computed, ref } from "vue";
import type { UnlistenFn } from "@tauri-apps/api/event";

import * as ipc from "../ipc/commands";
import { onPauseChanged } from "../ipc/events";
import type { PauseState } from "../ipc/types";

/**
 * Manual-pause store. Owns the `sync:pause_changed` subscription (registered
 * once at the app root in App.vue, mirroring the updater + progress stores) and
 * exposes the state the yellow paused banner renders: whether a pause is active,
 * whether it is indefinite, and - for a timed pause - the minutes left.
 *
 * The remaining time is derived from a `now` ref the banner ticks once a second
 * rather than from a countdown the store decrements, so the value stays correct
 * across a suspend/resume (it is always `until_ms - Date.now()`, never a drifting
 * accumulator).
 */
export const usePauseStore = defineStore("pause", () => {
  /** The active pause, or null when sync is not manually paused. */
  const pause = ref<PauseState | null>(null);
  /** Wall-clock ms driving the countdown; `tick()` advances it. */
  const now = ref<number>(Date.now());

  /** True while ANY manual pause (timed or indefinite) is in force. */
  const active = computed<boolean>(() => pause.value !== null);

  /** True when the active pause has no auto-resume (held until the user acts). */
  const indefinite = computed<boolean>(() => pause.value?.kind === "indefinite");

  /** Milliseconds until a timed pause auto-resumes; null when not timed-paused.
   * Floored at 0 so an expired-but-not-yet-cleared pause never counts negative. */
  const msRemaining = computed<number | null>(() => {
    const p = pause.value;
    if (p === null || p.kind !== "timed") return null;
    return Math.max(0, p.until_ms - now.value);
  });

  /** Whole minutes left on a timed pause, rounded UP so the last partial minute
   * still reads "1m left" rather than "0m left"; null when not timed-paused. */
  const minutesRemaining = computed<number | null>(() => {
    const ms = msRemaining.value;
    return ms === null ? null : Math.ceil(ms / 60_000);
  });

  /** Fold one `sync:pause_changed` payload (or a hydrate result) into state. */
  function ingest(next: PauseState | null): void {
    pause.value = next;
    now.value = Date.now();
  }

  /** Re-read the wall clock so the countdown recomputes (called on an interval
   * by the banner while a timed pause is showing). */
  function tick(): void {
    now.value = Date.now();
  }

  /** Clear the pause immediately (the banner's Resume button). Optimistically
   * clears local state so the banner disappears on click rather than waiting for
   * the round-trip; the backend's `sync:pause_changed(null)` confirms it. On
   * failure the previous state is restored and the error re-thrown so the caller
   * can surface it. */
  async function resume(): Promise<void> {
    const previous = pause.value;
    pause.value = null;
    try {
      await ipc.resumeSync();
    } catch (e) {
      pause.value = previous;
      throw e;
    }
  }

  // --- event subscription (App.vue owns the app-lifetime registration) ------
  let unlisten: UnlistenFn | null = null;
  let desiredSubscribed = false;

  /** Subscribe to `sync:pause_changed` (idempotent). */
  async function subscribe(): Promise<void> {
    if (desiredSubscribed) return;
    desiredSubscribed = true;
    try {
      const un = await onPauseChanged((next) => ingest(next));
      // unsubscribe() may have raced ahead while we awaited; honor it.
      if (!desiredSubscribed) {
        un();
        return;
      }
      unlisten = un;
    } catch (e) {
      // Reset so a later retry can re-subscribe; re-throw so the caller can log.
      desiredSubscribed = false;
      throw e;
    }
  }

  /** Seed from the backend's CURRENT pause state so a pause set before the
   * webview attached - including one restored from a previous run - shows
   * immediately. Best-effort: a failure leaves the live stream to fill it. */
  async function hydrate(): Promise<void> {
    try {
      ingest(await ipc.getPauseState());
    } catch (e) {
      console.error("pause hydrate failed at app boot", e);
    }
  }

  /** Stop the subscription. */
  function unsubscribe(): void {
    desiredSubscribed = false;
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
  }

  return {
    pause,
    active,
    indefinite,
    msRemaining,
    minutesRemaining,
    ingest,
    tick,
    resume,
    subscribe,
    hydrate,
    unsubscribe,
  };
});
