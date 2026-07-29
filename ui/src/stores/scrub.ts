import { defineStore } from "pinia";
import { computed, ref } from "vue";

import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
import type { ScrubRun } from "../ipc/types";

/** How many recent scrub runs the history panel loads. */
export const SCRUB_HISTORY_LIMIT = 10;

/**
 * Integrity-scrub history store.
 *
 * The scrub is a slow, scheduled background job (weekly by default), so unlike
 * the activity feed there is no live tail to subscribe to and nothing to
 * accumulate page by page: the panel loads the most recent runs on mount and
 * re-loads on demand. Every value here is a COUNT - the backend DTO carries no
 * paths, remote ids, or object names - so this store can never hold an
 * encrypted source's filenames.
 */
export const useScrubStore = defineStore("scrub", () => {
  /** Recent runs across every source, newest first. */
  const runs = ref<ScrubRun[]>([]);
  /** True while a load is in flight. */
  const loading = ref(false);
  /** The stable SPEC s24 error code of the last failed load, or null. */
  const errorCode = ref<string | null>(null);
  /** True once a load has completed at least once (so the empty state is not
   * shown before the first result arrives). */
  const loaded = ref(false);

  /** The newest run overall, or null before anything has been recorded. */
  const latest = computed<ScrubRun | null>(() => runs.value[0] ?? null);

  /**
   * Objects across the loaded runs that drifted and could NOT be repaired.
   *
   * Deliberately summed over the LOADED window rather than only the newest run:
   * the scrub walks a rolling slice, so damage found three runs ago is just as
   * unresolved as damage found today - it is not "old news", it simply has not
   * been fixed. A user looking at this number wants "how much needs attention",
   * not "how much needed attention in the most recent slice".
   */
  const unrecoverableTotal = computed<number>(() =>
    runs.value.reduce((sum, r) => sum + r.unrecoverable, 0)
  );

  /** True when any loaded run reports drift that could not be repaired. */
  const needsAttention = computed<boolean>(() => unrecoverableTotal.value > 0);

  /** Load (or reload) the recent scrub runs. */
  async function refresh(limit: number = SCRUB_HISTORY_LIMIT): Promise<void> {
    loading.value = true;
    errorCode.value = null;
    try {
      // Defensive `Array.isArray`: this panel renders inside the Activity view,
      // so anything that leaves the call unfulfilled (an older backend that does
      // not know the command, a dev shell with no IPC) must degrade to "no runs"
      // rather than putting `undefined` into a computed and blanking the whole
      // dashboard.
      const next = await ipc.listScrubRuns(undefined, limit);
      runs.value = Array.isArray(next) ? next : [];
      loaded.value = true;
    } catch (e) {
      // The stable code, never String(e): the view renders `errors.<code>.*`.
      errorCode.value = toErrorCode(e);
    } finally {
      loading.value = false;
    }
  }

  return {
    runs,
    loading,
    errorCode,
    loaded,
    latest,
    unrecoverableTotal,
    needsAttention,
    refresh,
  };
});
