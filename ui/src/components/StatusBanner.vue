<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";

import { syncNow } from "../ipc/commands";
import { usePauseStore } from "../stores/pause";
import { useProgressStore } from "../stores/progress";
import { useSettingsStore } from "../stores/settings";
import { bannerModel, type BannerModel, type BannerReason } from "../stores/bannerModel";

// The unified status banner (Banner Task 6,
// docs/superpowers/specs/2026-08-01-pause-banner-design.md): a full-width
// amber bar pinned under the progress bar, shown whenever ANY account is not
// backing up - a manual pause (absorbed unchanged from the old PausedBanner:
// countdown tick, Resume with error handling) OR a gate-driven reason
// (battery, metered, offline family, captive portal, an unreachable
// destination, or outside the backup schedule). `bannerModel` (Banner Task 5)
// picks the single highest-priority reason across every account's live
// OrchestratorState plus the manual PauseState; this component is a pure
// render of whatever it returns, plus the one action each reason offers.
const { t } = useI18n();
const router = useRouter();
const pause = usePauseStore();
const progress = useProgressStore();
const settingsStore = useSettingsStore();

// The schedule config isn't subscribed at app boot (App.vue only subscribes
// the updater/progress/pause stores) and Settings.vue only loads it lazily
// when the Rules tab is opened. A schedule-gated pause needs it to render
// "resumes at HH:MM" even if the user never opened Settings, so fetch it once
// here if it is not already loaded. `refresh()` already catches its own
// failure internally (stores it as `errorCode`, never rethrows), so a failed
// fetch just leaves the schedule gear/label degraded (no resume time) rather
// than blocking the banner - no catch needed here.
onMounted(() => {
  if (settingsStore.settings === null) {
    void settingsStore.refresh();
  }
});

// Wall-clock ms driving both the schedule's "resumes at HH:MM" calculation
// and the manual-pause expiry check inside bannerModel. Ticked once a second
// WHILE a banner is showing or a timed manual pause is in force (even before
// the next orchestrator tick reflects it in `progress.states`) - the same
// condition PausedBanner used to tick just its own store, extended to also
// cover the gate-driven reasons' need for a fresh clock. `pause.tick()` is
// still called alongside so the manual countdown (driven by the pause
// store's own `minutesRemaining`) keeps working unchanged.
const now = ref(Date.now());
let timer: ReturnType<typeof setInterval> | null = null;

function stopTicking(): void {
  if (timer !== null) {
    clearInterval(timer);
    timer = null;
  }
}

const model = computed<BannerModel | null>(() =>
  bannerModel(
    progress.states,
    pause.pause,
    settingsStore.settings?.global.schedule ?? null,
    now.value
  )
);

watch(
  () => model.value !== null || pause.pause?.kind === "timed",
  (shouldTick) => {
    stopTicking();
    if (shouldTick) {
      timer = setInterval(() => {
        pause.tick();
        now.value = Date.now();
      }, 1_000);
    }
  },
  { immediate: true }
);

onBeforeUnmount(stopTicking);

const busy = ref(false);
const actionError = ref<string | null>(null);

// Static reason -> label key map for the reasons whose copy needs no
// interpolation. manualTimed/manualIndefinite (minutes/no countdown) and
// schedule (the computed resume time) are handled separately in `label`
// below since each needs its own interpolated value.
const REASON_KEY: Partial<Record<BannerReason, string>> = {
  captivePortal: "statusBanner.captivePortal",
  offline: "statusBanner.offline",
  dnsFailed: "statusBanner.offline",
  noInternet: "statusBanner.offline",
  destinationUnreachable: "statusBanner.destinationUnreachable",
  metered: "statusBanner.metered",
  battery: "statusBanner.battery",
};

/** Mirrors Settings.vue's `minutesToHHMM` (same minute-of-day -> "HH:MM"
 * rendering) - not exported there (a local helper in its `<script setup>`),
 * so this is a small duplicate rather than a cross-file reach. */
function minutesToHHMM(min: number): string {
  const m = ((Math.floor(min) % 1_440) + 1_440) % 1_440;
  const hh = String(Math.floor(m / 60)).padStart(2, "0");
  const mm = String(m % 60).padStart(2, "0");
  return `${hh}:${mm}`;
}

const label = computed<string>(() => {
  const m = model.value;
  if (m === null) return "";
  if (m.reason === "manualTimed") {
    return t("pauseBanner.timed", { minutes: pause.minutesRemaining ?? 0 });
  }
  if (m.reason === "manualIndefinite") return t("pauseBanner.indefinite");
  if (m.reason === "schedule") {
    return m.resumeAtMinute === null
      ? t("statusBanner.scheduleUnknown")
      : t("statusBanner.schedule", { time: minutesToHHMM(m.resumeAtMinute) });
  }
  const key = REASON_KEY[m.reason];
  return key ? t(key) : "";
});

// Battery/metered ("Back up anyway") and schedule ("Back up now") share the
// `bypass` action but read differently, per the design doc's per-reason table.
const bypassLabel = computed<string>(() =>
  model.value?.reason === "schedule" ? t("statusBanner.backUpNow") : t("statusBanner.backUpAnyway")
);

async function runAction(action: () => Promise<void>): Promise<void> {
  busy.value = true;
  actionError.value = null;
  try {
    await action();
  } catch (e) {
    actionError.value = e instanceof Error ? e.message : String(e);
  } finally {
    busy.value = false;
  }
}

function onResume(): Promise<void> {
  return runAction(() => pause.resume());
}

function onRetry(): Promise<void> {
  return runAction(() => syncNow(null));
}

function onBypass(): Promise<void> {
  return runAction(() => syncNow(null, true));
}

function onGear(): void {
  const gear = model.value?.gear;
  if (!gear) return;
  void router.push(`/rules#${gear}`);
}
</script>

<template>
  <Transition name="driven-pause-fade">
    <div
      v-if="model"
      class="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-amber-400 bg-amber-50 px-6 py-2 text-sm text-amber-800 dark:border-amber-500/60 dark:bg-amber-950/40 dark:text-amber-200"
      role="status"
      data-testid="paused-banner"
    >
      <span class="font-medium">{{ label }}</span>

      <button
        v-if="model.action === 'resume'"
        type="button"
        class="rounded-sm border border-amber-500 px-2 py-0.5 font-medium text-amber-900 transition-colors hover:bg-amber-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-amber-600 disabled:cursor-not-allowed disabled:opacity-60 dark:border-amber-400/70 dark:text-amber-100 dark:hover:bg-amber-900/50"
        :disabled="busy"
        data-testid="paused-banner-resume"
        @click="onResume"
      >
        {{ t("pauseBanner.resume") }}
      </button>

      <button
        v-else-if="model.action === 'retry'"
        type="button"
        class="rounded-sm border border-amber-500 px-2 py-0.5 font-medium text-amber-900 transition-colors hover:bg-amber-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-amber-600 disabled:cursor-not-allowed disabled:opacity-60 dark:border-amber-400/70 dark:text-amber-100 dark:hover:bg-amber-900/50"
        :disabled="busy"
        data-testid="status-banner-retry"
        @click="onRetry"
      >
        {{ t("statusBanner.retry") }}
      </button>

      <button
        v-else
        type="button"
        class="rounded-sm border border-amber-500 px-2 py-0.5 font-medium text-amber-900 transition-colors hover:bg-amber-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-amber-600 disabled:cursor-not-allowed disabled:opacity-60 dark:border-amber-400/70 dark:text-amber-100 dark:hover:bg-amber-900/50"
        :disabled="busy"
        data-testid="status-banner-bypass"
        @click="onBypass"
      >
        {{ bypassLabel }}
      </button>

      <button
        v-if="model.gear"
        type="button"
        class="rounded-sm p-1 text-amber-700 transition-colors hover:bg-amber-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-amber-600 dark:text-amber-200 dark:hover:bg-amber-900/50"
        :aria-label="t('statusBanner.gear')"
        data-testid="status-banner-gear"
        @click="onGear"
      >
        <svg
          class="h-4 w-4"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
          stroke-linejoin="round"
          aria-hidden="true"
        >
          <circle cx="12" cy="12" r="3" />
          <path
            d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"
          />
        </svg>
      </button>

      <span
        v-if="actionError"
        class="text-red-700 dark:text-red-300"
        data-testid="paused-banner-error"
      >
        {{ actionError }}
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
