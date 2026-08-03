import { defineStore } from "pinia";
import { computed, ref } from "vue";

import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
import type { DrillRun } from "../ipc/types";

/** How many recent drill runs the history panel loads. */
export const DRILL_HISTORY_LIMIT = 10;

/**
 * Restore-drill history store.
 *
 * A drill is a slow, scheduled background job (monthly by default) that
 * restores a small sample of backed-up files through the REAL restore path and
 * verifies them, so like the scrub there is no live tail to subscribe to: the
 * panel loads the most recent runs on mount and re-loads on demand. Every value
 * here is a COUNT or a stable SPEC s24 error code from a closed vocabulary - the
 * backend DTO carries no paths, remote ids, or filenames - so this store can
 * never hold an encrypted source's filenames.
 */
export const useDrillStore = defineStore("drill", () => {
  /** Recent runs across every source, newest first. */
  const runs = ref<DrillRun[]>([]);
  /** True while a load is in flight. */
  const loading = ref(false);
  /** The stable SPEC s24 error code of the last failed load, or null. */
  const errorCode = ref<string | null>(null);
  /** True once a load has completed at least once (so the empty state is not
   * shown before the first result arrives). */
  const loaded = ref(false);

  /** The newest run overall, or null before anything has been recorded. */
  const latest = computed<DrillRun | null>(() => runs.value[0] ?? null);

  /**
   * Files across the loaded runs that could NOT be restored.
   *
   * Summed over the LOADED window rather than only the newest run, for the same
   * reason the scrub sums its unrecoverable count: a file that would not come
   * back three drills ago is just as unrestorable today unless something was
   * done about it. This answers "how much needs attention", not "how much
   * needed attention in the most recent sample".
   */
  const failedTotal = computed<number>(() => runs.value.reduce((sum, r) => sum + r.failed, 0));

  /** True when any loaded run could not restore a file it sampled. */
  const needsAttention = computed<boolean>(() => failedTotal.value > 0);

  /**
   * True when the newest run verified nothing at all.
   *
   * Surfaced separately from {@link needsAttention} because it is a different
   * message: not "your backup is broken" but "we could not actually check".
   * Collapsing the two would either raise a false data-loss alarm or, worse,
   * let a run that proved nothing read as a clean bill of health.
   */
  const inconclusive = computed<boolean>(() => latest.value?.outcome === "inconclusive");

  /** Load (or reload) the recent drill runs. */
  async function refresh(limit: number = DRILL_HISTORY_LIMIT): Promise<void> {
    loading.value = true;
    errorCode.value = null;
    try {
      // Defensive `Array.isArray`, same rationale as the scrub store: this panel
      // renders inside the Activity view, so anything that leaves the call
      // unfulfilled (an older backend that does not know the command, a dev
      // shell with no IPC) must degrade to "no runs" rather than putting
      // `undefined` into a computed and blanking the whole dashboard.
      const next = await ipc.listDrillRuns(undefined, limit);
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
    failedTotal,
    needsAttention,
    inconclusive,
    refresh,
  };
});
