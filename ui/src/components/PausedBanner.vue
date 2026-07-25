<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import { usePauseStore } from "../stores/pause";

// The paused banner: a full-width amber bar pinned under the progress bar at the
// top of the window, shown whenever a manual pause is in force. It states which
// kind of pause it is ("Backups paused - 27m left" for the tray's 30-minute
// pause, "Backups paused indefinitely" for pause-until-I-resume) and carries a
// single-click Resume that unpauses immediately.
//
// Visibility and the remaining time come from the pause store (subscribed once at
// the app root in App.vue), so this is a pure render of it plus the one-second
// tick that keeps the countdown honest.
const { t } = useI18n();
const pause = usePauseStore();

const resumeError = ref<string | null>(null);
const resuming = ref(false);

// Tick the store's clock once a second WHILE a timed pause is showing, so the
// "27m left" counts down live. An indefinite pause has nothing to count, so no
// interval is armed for it; the watcher also clears the interval the moment the
// pause ends, so the app is not left with a timer running forever.
let timer: ReturnType<typeof setInterval> | null = null;

function stopTicking(): void {
  if (timer !== null) {
    clearInterval(timer);
    timer = null;
  }
}

watch(
  () => pause.minutesRemaining !== null,
  (timed) => {
    stopTicking();
    if (timed) timer = setInterval(() => pause.tick(), 1_000);
  },
  { immediate: true }
);

onBeforeUnmount(stopTicking);

// "Backups paused indefinitely" vs "Backups paused - 27m left". Bound (not a
// literal) so the i18n no-raw-text rule is satisfied.
const label = computed<string>(() => {
  const minutes = pause.minutesRemaining;
  return minutes === null ? t("pauseBanner.indefinite") : t("pauseBanner.timed", { minutes });
});

async function onResume(): Promise<void> {
  resuming.value = true;
  resumeError.value = null;
  try {
    await pause.resume();
  } catch (e) {
    resumeError.value = e instanceof Error ? e.message : String(e);
  } finally {
    resuming.value = false;
  }
}
</script>

<template>
  <Transition name="driven-pause-fade">
    <div
      v-if="pause.active"
      class="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-amber-400 bg-amber-50 px-6 py-2 text-sm text-amber-800 dark:border-amber-500/60 dark:bg-amber-950/40 dark:text-amber-200"
      role="status"
      data-testid="paused-banner"
    >
      <span class="font-medium">{{ label }}</span>
      <button
        type="button"
        class="rounded-sm border border-amber-500 px-2 py-0.5 font-medium text-amber-900 transition-colors hover:bg-amber-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-amber-600 disabled:cursor-not-allowed disabled:opacity-60 dark:border-amber-400/70 dark:text-amber-100 dark:hover:bg-amber-900/50"
        :disabled="resuming"
        data-testid="paused-banner-resume"
        @click="onResume"
      >
        {{ t("pauseBanner.resume") }}
      </button>
      <span
        v-if="resumeError"
        class="text-red-700 dark:text-red-300"
        data-testid="paused-banner-error"
      >
        {{ resumeError }}
      </span>
    </div>
  </Transition>
</template>

<style scoped>
.driven-pause-fade-enter-active,
.driven-pause-fade-leave-active {
  transition: opacity 200ms ease;
}

.driven-pause-fade-enter-from,
.driven-pause-fade-leave-to {
  opacity: 0;
}
</style>
