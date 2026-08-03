<script setup lang="ts">
import { computed, onMounted } from "vue";
import { useI18n } from "vue-i18n";

import { useDrillStore } from "../stores/drill";
import type { DrillRun } from "../ipc/types";

// The restore-drill history panel, shown in the Activity dashboard beside the
// event feed and below the integrity-scrub panel.
//
// A drill is a slow scheduled job (monthly by default) that restores a small
// sample of backed-up files through the REAL restore path and verifies them, so
// this panel is a STATUS surface, not a live feed: it loads the recent runs once
// on mount and offers an explicit refresh. Everything it renders is a COUNT or a
// stable SPEC s24 error code from a closed vocabulary - the backend DTO
// deliberately carries no paths, remote ids, or filenames - so this panel can
// never display an encrypted source's filenames.
const { t, locale } = useI18n();
const drill = useDrillStore();

onMounted(() => {
  void drill.refresh();
});

const dateFmt = computed(
  () =>
    new Intl.DateTimeFormat(locale.value, {
      dateStyle: "medium",
      timeStyle: "short",
    })
);

function when(run: DrillRun): string {
  return dateFmt.value.format(new Date(run.startedAt));
}

/** The translated one-line verdict for a run. */
function outcomeLabel(run: DrillRun): string {
  // Keyed off `failed` first so a partial failure never reads as a pass, and
  // `inconclusive` is rendered as its own thing rather than folded into either
  // side: a drill that verified nothing has not cleared the backup, but it has
  // not condemned it either.
  if (run.failed > 0) return t("drill.outcome.failed");
  if (run.outcome === "inconclusive") return t("drill.outcome.inconclusive");
  return t("drill.outcome.passed");
}

/** Tailwind classes for the run's status dot, keyed off the same verdict. */
function outcomeClass(run: DrillRun): string {
  if (run.failed > 0) return "bg-red-500";
  if (run.outcome === "inconclusive") return "bg-slate-400 dark:bg-slate-500";
  return "bg-emerald-500";
}

/** The failure breakdown as one readable line, e.g. "crypto.decrypt_failed x2". */
function failureSummary(run: DrillRun): string {
  return run.failureCodes.map((f) => `${f.code} x${f.count}`).join(", ");
}
</script>

<template>
  <section
    class="rounded-lg border border-slate-200 bg-white p-4 dark:border-slate-700 dark:bg-slate-900"
    data-testid="drill-panel"
  >
    <header class="mb-3 flex items-center justify-between gap-3">
      <h2 class="text-sm font-semibold text-slate-900 dark:text-slate-100">
        {{ t("drill.title") }}
      </h2>
      <button
        type="button"
        class="rounded-sm border border-slate-300 px-2 py-0.5 text-xs font-medium text-slate-700 transition-colors hover:bg-slate-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-600 disabled:cursor-not-allowed disabled:opacity-60 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-800"
        :disabled="drill.loading"
        data-testid="drill-refresh"
        @click="drill.refresh()"
      >
        {{ t("drill.refresh") }}
      </button>
    </header>

    <p class="mb-3 text-xs text-slate-500 dark:text-slate-400">
      {{ t("drill.explainer") }}
    </p>

    <p
      v-if="drill.errorCode"
      class="text-sm text-red-700 dark:text-red-300"
      data-testid="drill-error"
    >
      {{ t(`errors.${drill.errorCode}.short`) }}
    </p>

    <p
      v-else-if="drill.needsAttention"
      class="mb-3 rounded-sm border border-red-300 bg-red-50 px-3 py-2 text-sm text-red-800 dark:border-red-500/60 dark:bg-red-950/40 dark:text-red-200"
      role="status"
      data-testid="drill-attention"
    >
      {{ t("drill.attention", { count: drill.failedTotal }) }}
    </p>

    <!-- Distinct from the failure banner on purpose: "we could not check" is
         not "your backup is broken", and it must not read as a pass either. -->
    <p
      v-else-if="drill.inconclusive"
      class="mb-3 rounded-sm border border-slate-300 bg-slate-50 px-3 py-2 text-sm text-slate-700 dark:border-slate-600 dark:bg-slate-800/60 dark:text-slate-300"
      role="status"
      data-testid="drill-inconclusive"
    >
      {{ t("drill.inconclusive") }}
    </p>

    <p
      v-if="!drill.errorCode && drill.loaded && drill.runs.length === 0"
      class="text-sm text-slate-500 dark:text-slate-400"
      data-testid="drill-empty"
    >
      {{ t("drill.empty") }}
    </p>

    <ul v-if="!drill.errorCode" class="space-y-2">
      <li
        v-for="run in drill.runs"
        :key="run.id"
        class="flex flex-wrap items-baseline gap-x-3 gap-y-1 text-sm"
        data-testid="drill-run"
      >
        <span
          class="inline-block h-2 w-2 shrink-0 rounded-full"
          :class="outcomeClass(run)"
          aria-hidden="true"
        />
        <span class="font-medium text-slate-900 dark:text-slate-100">
          {{ outcomeLabel(run) }}
        </span>
        <span class="text-slate-500 dark:text-slate-400">{{ when(run) }}</span>
        <span class="text-slate-500 dark:text-slate-400">
          {{ t("drill.verifiedCount", { count: run.verified }) }}
        </span>
        <span
          v-if="run.skipped > 0"
          class="text-slate-500 dark:text-slate-400"
          data-testid="drill-run-skipped"
        >
          {{ t("drill.skippedCount", { count: run.skipped }) }}
        </span>
        <span
          v-if="run.failed > 0"
          class="text-red-700 dark:text-red-300"
          data-testid="drill-run-failed"
        >
          {{ t("drill.failedCount", { count: run.failed }) }}
        </span>
        <span
          v-if="run.failureCodes.length > 0"
          class="w-full font-mono text-xs text-red-700 dark:text-red-300"
          data-testid="drill-run-codes"
        >
          {{ failureSummary(run) }}
        </span>
      </li>
    </ul>
  </section>
</template>
