<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";

import { useProgressStore } from "../stores/progress";

// The global backup progress bar (issue #46). A teal bar pinned to the very top
// of the app shell that appears ONLY while a backup/sync run is in progress (any
// account in a working orchestrator state) and hides when idle. It is
// DETERMINATE while the run is uploading (the orchestrator's `executing` state
// carries byte/file totals) and INDETERMINATE during scan/plan/verify, when no
// reliable total exists yet. Visibility + percent are owned by the progress store
// (subscribed once at the app root in App.vue), so this is a pure render of it.
//
// It also carries a PHASE READOUT beneath the bar. Without it a "Run now" over a
// large tree showed nothing but a 4px indeterminate sliver for the whole scan -
// users could not tell it was a progress bar at all, let alone that the app was
// working. The readout names the current phase and, once the backend's live scan
// ticks arrive, its running file count ("Scanning - 12,401 files").
const { t, locale } = useI18n();
const progress = useProgressStore();

const active = computed(() => progress.active);

// 0..100 integer width for the determinate fill, or null when indeterminate.
const widthPct = computed<number | null>(() =>
  progress.percent === null ? null : Math.round(progress.percent * 100)
);

// Locale-aware counts (DESIGN s8.7: never a hand-rolled English formatter).
const numberFormatter = computed(() => new Intl.NumberFormat(locale.value));

// Accessible label + hover tooltip + the visible phase readout, all one string.
// Determinate upload -> "Backing up - 42%"; the pre-upload phases name
// themselves and add their live count once it is non-zero (a count of 0 is the
// instant before the first tick, where a bare "Scanning for changes..." reads
// better than "... - 0 files"). Bound (not a literal) so the i18n no-raw-text
// rule is satisfied; every key is spelled out at its call site so the
// no-unused-keys lint can see it (a `t(someKeyVariable)` indirection reads as an
// unused key and would rot silently on the next locale sweep).
const label = computed<string>(() => {
  const count = numberFormatter.value;
  switch (progress.phase) {
    case "scanning":
      return progress.scanned > 0
        ? t("progress.scanningCount", { count: count.format(progress.scanned) })
        : t("progress.scanning");
    case "planning":
      return progress.plannedFiles > 0
        ? t("progress.planningCount", { count: count.format(progress.plannedFiles) })
        : t("progress.planning");
    case "verifying":
      return progress.verified > 0
        ? t("progress.verifyingCount", { count: count.format(progress.verified) })
        : t("progress.verifying");
    case "power_check":
      return t("progress.starting");
    default:
      if (widthPct.value === null) return t("progress.backingUp");
      // Once the executor's live ticks arrive the file totals are real, so name
      // them: a bare percent gives no sense of scale, and "1,234 of 3,000 files"
      // is what tells the user whether this run is minutes or hours. Falls back
      // to the bare percent for a delete-only plan, which uploads no files.
      return progress.filesTotal > 0
        ? t("progress.backingUpPercentFiles", {
            percent: widthPct.value,
            done: count.format(progress.filesDone),
            total: count.format(progress.filesTotal),
          })
        : t("progress.backingUpPercent", { percent: widthPct.value });
  }
});
</script>

<template>
  <Transition name="driven-progress-fade">
    <div v-if="active" data-testid="global-progress">
      <div
        class="global-progress relative h-1.5 w-full overflow-hidden bg-teal-100 dark:bg-teal-900/40"
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
             has no measurable total yet (scan / plan / verify). It is always a
             PARTIAL width so it can never be mistaken for a finished bar, and
             rounded so it reads as a travelling pill rather than a fill edge. -->
        <div
          v-else
          class="global-progress__indeterminate absolute inset-y-0 left-0 w-2/5 rounded-full bg-teal-600 dark:bg-teal-400"
          data-testid="global-progress-indeterminate"
        ></div>
      </div>
      <!-- Phase readout. `aria-live="polite"` so a screen reader hears the phase
           change without the per-tick count spamming it (the text node updates
           far more often than the phase does, but polite announcements coalesce).
           This is the part that makes the bar legible AS a progress bar. -->
      <p
        class="flex items-center gap-2 bg-teal-50 px-6 py-1 text-xs font-medium text-teal-800 dark:bg-teal-950/60 dark:text-teal-200"
        aria-live="polite"
        data-testid="global-progress-label"
      >
        {{ label }}
      </p>
    </div>
  </Transition>
</template>

<style scoped>
/* Transform-only sweep so the compositor can run it off the main thread - a
   scan that is hammering the disk must never make the one thing telling the
   user "we are working" stutter. The segment is 40% of the track and the
   keyframes are in percentages OF THAT segment, so it enters from fully
   off-track on the left and exits fully off-track on the right. */
.global-progress__indeterminate {
  animation: driven-progress-indeterminate 1.2s ease-in-out infinite;
  will-change: transform;
}

@keyframes driven-progress-indeterminate {
  0% {
    transform: translateX(-150%);
  }
  100% {
    transform: translateX(350%);
  }
}

/* Reduced-motion fallback (below) - a slow, gentle fade in place. */
@keyframes driven-progress-pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.45;
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

/* Respect reduced-motion: drop the travelling sweep for a gentle fade in place.
   The segment MUST stay partial-width here. The previous fallback stretched it
   to `width: 100%`, which rendered a solid full-width teal line - visually
   indistinguishable from a completed determinate fill. That is what a scan
   looked like for anyone with OS animations turned off (Windows' "Show
   animations in Windows" maps to prefers-reduced-motion: reduce), so the bar
   read as "finished" for the entire scan. A partial bar can never be mistaken
   for 100%, and the opacity pulse still says "working" without moving anything
   across the screen. */
@media (prefers-reduced-motion: reduce) {
  .global-progress__indeterminate {
    animation: driven-progress-pulse 2.4s ease-in-out infinite;
    will-change: opacity;
  }
}
</style>
