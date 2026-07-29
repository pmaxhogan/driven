<script setup lang="ts">
import { computed, onMounted } from "vue";
import { useI18n } from "vue-i18n";

import { useScrubStore } from "../stores/scrub";
import type { ScrubRun } from "../ipc/types";

// The integrity-scrub history panel, shown in the Activity dashboard beside the
// event feed.
//
// The scrub is a slow scheduled job (weekly by default) that re-checks backed-up
// objects against what the local database records about them, so this panel is a
// STATUS surface, not a live feed: it loads the recent runs once on mount and
// offers an explicit refresh. Everything it renders is a COUNT - the backend DTO
// deliberately carries no paths, remote ids, or object names - so this panel can
// never display an encrypted source's filenames.
const { t, locale } = useI18n();
const scrub = useScrubStore();

onMounted(() => {
  void scrub.refresh();
});

const dateFmt = computed(
  () =>
    new Intl.DateTimeFormat(locale.value, {
      dateStyle: "medium",
      timeStyle: "short",
    })
);

function when(run: ScrubRun): string {
  return dateFmt.value.format(new Date(run.startedAt));
}

/** The translated one-line verdict for a run. */
function outcomeLabel(run: ScrubRun): string {
  // `incomplete` means the run could not enumerate the remote at all, which is
  // materially different from "checked and found nothing" - conflating them
  // would let a week of failed checks read as a week of clean ones.
  if (run.outcome === "incomplete") return t("scrub.outcome.incomplete");
  if (run.unrecoverable > 0) return t("scrub.outcome.needsAttention");
  if (run.healed > 0) return t("scrub.outcome.repaired");
  return t("scrub.outcome.clean");
}

/** Tailwind classes for the run's status dot, keyed off the same verdict. */
function outcomeClass(run: ScrubRun): string {
  if (run.outcome === "incomplete") return "bg-slate-400 dark:bg-slate-500";
  if (run.unrecoverable > 0) return "bg-red-500";
  if (run.healed > 0) return "bg-amber-500";
  return "bg-emerald-500";
}
</script>

<template>
  <section
    class="rounded-lg border border-slate-200 bg-white p-4 dark:border-slate-700 dark:bg-slate-900"
    data-testid="scrub-panel"
  >
    <header class="mb-3 flex items-center justify-between gap-3">
      <h2 class="text-sm font-semibold text-slate-900 dark:text-slate-100">
        {{ t("scrub.title") }}
      </h2>
      <button
        type="button"
        class="rounded-sm border border-slate-300 px-2 py-0.5 text-xs font-medium text-slate-700 transition-colors hover:bg-slate-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-600 disabled:cursor-not-allowed disabled:opacity-60 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-800"
        :disabled="scrub.loading"
        data-testid="scrub-refresh"
        @click="scrub.refresh()"
      >
        {{ t("scrub.refresh") }}
      </button>
    </header>

    <p class="mb-3 text-xs text-slate-500 dark:text-slate-400">
      {{ t("scrub.explainer") }}
    </p>

    <p
      v-if="scrub.errorCode"
      class="text-sm text-red-700 dark:text-red-300"
      data-testid="scrub-error"
    >
      {{ t(`errors.${scrub.errorCode}.short`) }}
    </p>

    <p
      v-else-if="scrub.needsAttention"
      class="mb-3 rounded-sm border border-red-300 bg-red-50 px-3 py-2 text-sm text-red-800 dark:border-red-500/60 dark:bg-red-950/40 dark:text-red-200"
      role="status"
      data-testid="scrub-attention"
    >
      {{ t("scrub.attention", { count: scrub.unrecoverableTotal }) }}
    </p>

    <p
      v-if="!scrub.errorCode && scrub.loaded && scrub.runs.length === 0"
      class="text-sm text-slate-500 dark:text-slate-400"
      data-testid="scrub-empty"
    >
      {{ t("scrub.empty") }}
    </p>

    <ul v-if="!scrub.errorCode" class="space-y-2">
      <li
        v-for="run in scrub.runs"
        :key="run.id"
        class="flex flex-wrap items-baseline gap-x-3 gap-y-1 text-sm"
        data-testid="scrub-run"
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
          {{ t("scrub.checkedCount", { count: run.checked }) }}
        </span>
        <span
          v-if="run.healed > 0"
          class="text-amber-700 dark:text-amber-300"
          data-testid="scrub-run-healed"
        >
          {{ t("scrub.healedCount", { count: run.healed }) }}
        </span>
        <span
          v-if="run.unrecoverable > 0"
          class="text-red-700 dark:text-red-300"
          data-testid="scrub-run-unrecoverable"
        >
          {{ t("scrub.unrecoverableCount", { count: run.unrecoverable }) }}
        </span>
        <span
          v-if="run.deepChecked > 0"
          class="text-slate-500 dark:text-slate-400"
          data-testid="scrub-run-deep"
        >
          {{ t("scrub.deepCount", { count: run.deepChecked }) }}
        </span>
      </li>
    </ul>
  </section>
</template>
