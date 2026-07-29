import { defineStore } from "pinia";
import { computed, ref } from "vue";

import type { ActivityEntry } from "../ipc/types";

/** The activity `event_type` the backend writes when macOS TCC (Transparency,
 * Consent and Control) refuses a read - the ONLY signal this banner keys off.
 * It can only ever be emitted on macOS, so the banner is inherently mac-only
 * and needs no platform sniff (exactly like the Windows VSS banner, which also
 * derives purely from backend state). */
export const PERMISSION_DENIED_EVENT = "local.permission_denied";

/**
 * Full Disk Access banner store (DESIGN s5.3.2 - macOS TCC). Drives the
 * dismissible banner that explains why some files are being skipped and offers
 * the one-click deep link into System Settings > Privacy & Security > Full Disk
 * Access.
 *
 * Two properties of a TCC denial shape this store:
 *
 *  - **It is permanent until the user grants access.** Unlike a locked file
 *    (which frees up on its own), a denied path is re-denied on EVERY scan, so
 *    the backend writes one warn row per file per cycle, unbounded. We therefore
 *    dedupe by the row's message (the path) - a file seen across 50 cycles
 *    counts once, and the banner says "3 files", not "150". The dedupe lives
 *    here, in the display layer, so neither the activity log nor the backend has
 *    to change what it records.
 *  - **Dismissal is PER-SESSION by design.** The condition outlives the app
 *    process, so re-surfacing the banner on the next launch is correct, not a
 *    bug - and it means no storage is involved (matching `updater.ts`, the only
 *    other dismissible banner; `localStorage` is used nowhere in this UI).
 *
 * Like `updater.ts`'s banner, this one does NOT un-dismiss itself: once the user
 * closes it, later denials - including ones for files never seen before - keep
 * counting into `deniedCount` but do not re-open the banner. Re-opening on the
 * next denial would mean re-opening within seconds, since a denied file is
 * re-reported every cycle, which would make the dismiss button useless.
 */
export const useFdaBannerStore = defineStore("fdaBanner", () => {
  // The distinct files macOS has refused, keyed by the activity row's message
  // (the path). A row with no message falls back to its id, which is unique per
  // row - that degrades to "no dedupe" for that row rather than collapsing
  // unrelated files into one.
  const deniedFiles = ref<Set<string>>(new Set());

  // Whether the user closed the banner this session.
  const dismissed = ref(false);

  /** How many DISTINCT files macOS has refused this session. */
  const deniedCount = computed(() => deniedFiles.value.size);

  /** Whether the banner should be shown. */
  const visible = computed(() => deniedCount.value > 0 && !dismissed.value);

  /** Ingest one live activity row. Ignores everything that is not a TCC denial;
   * a denial adds its dedupe key. Deliberately does not clear `dismissed` (see
   * the store comment). */
  function noteDenial(entry: ActivityEntry): void {
    if (entry.eventType !== PERMISSION_DENIED_EVENT) return;
    const key = entry.message ?? String(entry.id);
    if (deniedFiles.value.has(key)) return;
    // Replace the Set rather than mutating it: a plain `add` on a `ref<Set>` is
    // not reactive, so the computed count would never re-run.
    deniedFiles.value = new Set(deniedFiles.value).add(key);
  }

  /** Close the banner for the rest of this session. */
  function dismiss(): void {
    dismissed.value = true;
  }

  /** Clear both the denied set and the dismissal, so the banner behaves as if
   * the app just started. Used by tests, and by a future "re-check permissions"
   * action once access has been granted. */
  function reset(): void {
    deniedFiles.value = new Set();
    dismissed.value = false;
  }

  return {
    deniedFiles,
    dismissed,
    deniedCount,
    visible,
    noteDenial,
    dismiss,
    reset,
  };
});
