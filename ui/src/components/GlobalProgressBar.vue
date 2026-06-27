<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";

import { useProgressStore } from "../stores/progress";

// The global backup progress bar (issue #46). A thin teal bar pinned to the very
// top of the app shell that appears ONLY while a backup/sync run is in progress
// (any account in a working orchestrator state) and hides when idle. It is
// DETERMINATE while the run is uploading (the orchestrator's `executing` state
// carries byte/file totals) and INDETERMINATE during scan/plan/verify, when no
// reliable total exists yet. Visibility + percent are owned by the progress store
// (subscribed once at the app root in App.vue), so this is a pure render of it.
const { t } = useI18n();
const progress = useProgressStore();

const active = computed(() => progress.active);

// 0..100 integer width for the determinate fill, or null when indeterminate.
const widthPct = computed<number | null>(() =>
  progress.percent === null ? null : Math.round(progress.percent * 100)
);

// Accessible label + hover tooltip. Determinate -> "Backing up - 42%"; otherwise
// (scan / plan / verify, no measurable total) just "Backing up...". Bound (not a
// literal) so the i18n no-raw-text rule is satisfied.
const label = computed<string>(() =>
  widthPct.value !== null
    ? t("progress.backingUpPercent", { percent: widthPct.value })
    : t("progress.backingUp")
);
</script>

<template>
  <Transition name="driven-progress-fade">
    <div
      v-if="active"
      class="global-progress relative h-1 w-full overflow-hidden bg-teal-100 dark:bg-teal-900/40"
      role="progressbar"
      :aria-label="label"
      :title="label"
      :aria-valuemin="widthPct === null ? undefined : 0"
      :aria-valuemax="widthPct === null ? undefined : 100"
      :aria-valuenow="widthPct === null ? undefined : widthPct"
    >
      <!-- Determinate: a teal fill sized to the completion percent. -->
      <div
        v-if="widthPct !== null"
        class="h-full bg-teal-600 transition-[width] duration-300 ease-out dark:bg-teal-400"
        :style="{ width: `${widthPct}%` }"
      ></div>
      <!-- Indeterminate: a teal sliver sweeping across while a run is active but
           has no measurable total yet (scan / plan / verify). -->
      <div
        v-else
        class="global-progress__indeterminate absolute inset-y-0 left-0 w-2/5 bg-teal-600 dark:bg-teal-400"
      ></div>
    </div>
  </Transition>
</template>

<style scoped>
.global-progress__indeterminate {
  animation: driven-progress-indeterminate 1.3s ease-in-out infinite;
}

@keyframes driven-progress-indeterminate {
  0% {
    transform: translateX(-150%);
  }
  100% {
    transform: translateX(350%);
  }
}

.driven-progress-fade-enter-active,
.driven-progress-fade-leave-active {
  transition: opacity 200ms ease;
}

.driven-progress-fade-enter-from,
.driven-progress-fade-leave-to {
  opacity: 0;
}

/* Respect reduced-motion: drop the sweep animation and show a steady partial
   bar so the indeterminate state is still visibly "working". */
@media (prefers-reduced-motion: reduce) {
  .global-progress__indeterminate {
    animation: none;
    width: 100%;
    opacity: 0.6;
  }
}
</style>
