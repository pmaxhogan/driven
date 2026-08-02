<script setup lang="ts">
import { ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import { useSettingsStore } from "../../stores/settings";
import { minutesToHHMM, hhmmToMinutes, useSettingsForm } from "../../composables/useSettingsForm";
import { cardCls, inputCls, ensureSettingsLoaded } from "./shared";

// Schedule & Power settings page (SDD 2026-08-02 settings-sidebar-ia, task 2).
// Moved verbatim out of Settings.vue: the "Power and network" card
// (skipOnBattery, pauseWhenOffline, skipOnMetered + metered mode/cap) and the
// "Schedule window" card - together they answer "when does Driven pause /
// run backups?" (per the plan's locked gear-link decision).
const { t } = useI18n();
const settings = useSettingsStore();
const { commitPatch, parseOptionalClamped, RANGES } = useSettingsForm();

ensureSettingsLoaded();

// Metered pause-or-throttle local mirrors (DESIGN s17).
const meteredModes = ["pause", "throttle"] as const;
const meteredMode = ref("pause");
const meteredCapText = ref("");

// Schedule-window (DESIGN s17) local mirrors. Times are edited as "HH:MM"
// strings (native <input type="time">); days[0]=Sunday..[6]=Saturday.
const dayIndices = [0, 1, 2, 3, 4, 5, 6] as const;
const scheduleEnabled = ref(false);
const scheduleStart = ref("00:00");
const scheduleEnd = ref("00:00");
const scheduleDays = ref<boolean[]>([true, true, true, true, true, true, true]);

// Keep the local mirrors in sync with the loaded snapshot.
watch(
  () => settings.settings,
  (s) => {
    if (!s) return;
    // Defensive: a partial global (e.g. an update_settings round-trip that
    // echoes only the patched keys) may omit `schedule`; keep the prior local
    // values rather than crash the watcher.
    const sched = s.global.schedule;
    if (sched) {
      scheduleEnabled.value = sched.enabled;
      scheduleStart.value = minutesToHHMM(sched.startMinute);
      scheduleEnd.value = minutesToHHMM(sched.endMinute);
      // Coerce to exactly seven booleans regardless of what was stored.
      scheduleDays.value = dayIndices.map((i) => sched.days?.[i] ?? true);
    }
    meteredMode.value = s.global.meteredMode ?? "pause";
    meteredCapText.value =
      s.global.meteredBandwidthCapMbps === null ? "" : String(s.global.meteredBandwidthCapMbps);
  },
  { immediate: true }
);

async function setSkipOnBattery(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ global: { skipOnBattery: checked } });
}

async function setSkipOnMetered(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ global: { skipOnMetered: checked } });
}

// Pause-banner spec (2026-08-01): pause backups while offline (default ON).
// Turning it off suits LAN-only / local-folder destinations that don't need
// internet reachability to back up.
async function setPauseWhenOffline(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ global: { pauseWhenOffline: checked } });
}

async function setMeteredMode(event: Event): Promise<void> {
  const value = (event.target as HTMLSelectElement).value;
  await commitPatch({ global: { meteredMode: value } });
}

async function commitMeteredCap(): Promise<void> {
  await commitPatch({
    global: {
      meteredBandwidthCapMbps: parseOptionalClamped(
        meteredCapText.value,
        RANGES.meteredBandwidthCapMbps
      ),
    },
  });
}

// Persist the whole schedule window. The UTC offset is captured fresh from
// this machine on every save (DESIGN s17 - driven-core stays tz-database-free
// and reasons from a fixed offset). `getTimezoneOffset()` returns minutes to
// SUBTRACT to reach UTC, so negate it to get "minutes to add to UTC".
//
// DEVIATION from the pre-task-1 form (SDD task 1): `hhmmToMinutes` now returns
// `null` for an unparseable "HH:MM" string instead of silently coercing to
// midnight. An invalid time text therefore skips the commit entirely and
// leaves the field's last-good value in the store untouched, rather than
// silently writing 00:00.
async function commitSchedule(): Promise<void> {
  const startMinute = hhmmToMinutes(scheduleStart.value);
  const endMinute = hhmmToMinutes(scheduleEnd.value);
  if (startMinute === null || endMinute === null) return;
  await commitPatch({
    global: {
      schedule: {
        enabled: scheduleEnabled.value,
        startMinute,
        endMinute,
        days: [...scheduleDays.value],
        utcOffsetMinutes: -new Date().getTimezoneOffset(),
      },
    },
  });
}

async function setScheduleEnabled(event: Event): Promise<void> {
  scheduleEnabled.value = (event.target as HTMLInputElement).checked;
  await commitSchedule();
}

async function toggleScheduleDay(index: number): Promise<void> {
  scheduleDays.value[index] = !scheduleDays.value[index];
  await commitSchedule();
}
</script>

<template>
  <p v-if="settings.loading && !settings.settings" class="text-sm text-zinc-500">
    {{ t("common.loading") }}
  </p>
  <p v-else-if="!settings.settings && settings.errorCode" class="text-sm text-red-600" role="alert">
    {{ t(`errors.${settings.errorCode}.long`) }}
  </p>
  <div v-else-if="settings.settings" class="max-w-2xl space-y-4 text-sm" data-testid="rules-form">
    <p
      v-if="settings.errorCode"
      class="rounded-md bg-red-50 px-3 py-2 text-sm text-red-700 dark:bg-red-950/40 dark:text-red-300"
      role="alert"
      data-testid="rules-error"
    >
      {{ t(`errors.${settings.errorCode}.long`) }}
    </p>

    <!-- Power and network -->
    <section class="space-y-3" :class="cardCls">
      <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
        {{ t("settings.rules.sections.powerNetwork") }}
      </h3>
      <label class="flex items-center gap-2">
        <input
          type="checkbox"
          class="accent-teal-600"
          :checked="settings.settings.global.skipOnBattery"
          @change="setSkipOnBattery"
        />
        {{ t("settings.rules.skipOnBatteryLabel") }}
      </label>

      <label class="flex items-center gap-2">
        <input
          type="checkbox"
          class="accent-teal-600"
          data-testid="pause-when-offline-toggle"
          :checked="settings.settings.global.pauseWhenOffline"
          @change="setPauseWhenOffline"
        />
        {{ t("settings.rules.pauseWhenOfflineLabel") }}
      </label>
      <p class="text-xs text-zinc-500 dark:text-zinc-400">
        {{ t("settings.rules.pauseWhenOfflineNote") }}
      </p>

      <label class="flex items-center gap-2">
        <input
          type="checkbox"
          class="accent-teal-600"
          :checked="settings.settings.global.skipOnMetered"
          @change="setSkipOnMetered"
        />
        {{ t("settings.rules.skipOnMeteredLabel") }}
      </label>

      <div
        v-if="settings.settings.global.skipOnMetered"
        class="space-y-2 border-l-2 border-teal-600/40 pl-4"
        data-testid="metered-setting"
      >
        <label class="block space-y-1">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.rules.metered.modeLabel")
          }}</span>
          <select
            data-testid="metered-mode"
            class="w-full"
            :class="inputCls"
            :value="meteredMode"
            @change="setMeteredMode"
          >
            <option v-for="mode in meteredModes" :key="mode" :value="mode">
              {{ t(`settings.rules.metered.mode.${mode}`) }}
            </option>
          </select>
        </label>
        <label v-if="meteredMode === 'throttle'" class="block space-y-1">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.rules.metered.capLabel")
          }}</span>
          <input
            v-model="meteredCapText"
            type="number"
            min="1"
            max="100000"
            class="w-full"
            :class="inputCls"
            :placeholder="t('settings.rules.bandwidthCapUnlimited')"
            @change="commitMeteredCap"
          />
        </label>
      </div>
    </section>

    <!-- Schedule window -->
    <section class="space-y-2" :class="cardCls" data-testid="schedule-setting">
      <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
        {{ t("settings.rules.sections.schedule") }}
      </h3>
      <label class="flex items-center gap-2">
        <input
          type="checkbox"
          class="accent-teal-600"
          data-testid="schedule-enabled"
          :checked="scheduleEnabled"
          @change="setScheduleEnabled"
        />
        {{ t("settings.rules.schedule.label") }}
      </label>
      <div v-if="scheduleEnabled" class="space-y-3 border-l-2 border-teal-600/40 pl-4">
        <div class="flex gap-3">
          <label class="block space-y-1">
            <span class="text-zinc-600 dark:text-zinc-400">{{
              t("settings.rules.schedule.startLabel")
            }}</span>
            <input v-model="scheduleStart" type="time" :class="inputCls" @change="commitSchedule" />
          </label>
          <label class="block space-y-1">
            <span class="text-zinc-600 dark:text-zinc-400">{{
              t("settings.rules.schedule.endLabel")
            }}</span>
            <input v-model="scheduleEnd" type="time" :class="inputCls" @change="commitSchedule" />
          </label>
        </div>
        <div class="space-y-1">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.rules.schedule.daysLabel")
          }}</span>
          <div class="flex flex-wrap gap-1">
            <button
              v-for="i in dayIndices"
              :key="i"
              type="button"
              class="rounded-md border px-2 py-1 text-xs transition-colors focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500"
              :class="
                scheduleDays[i]
                  ? 'border-teal-600 bg-teal-700 text-white'
                  : 'border-zinc-300 text-zinc-600 hover:border-teal-500 hover:text-teal-700 dark:border-zinc-700 dark:text-zinc-300 dark:hover:text-teal-300'
              "
              :aria-pressed="scheduleDays[i]"
              @click="toggleScheduleDay(i)"
            >
              {{ t(`settings.rules.schedule.day.${i}`) }}
            </button>
          </div>
        </div>
        <p class="text-xs text-zinc-500">
          {{ t("settings.rules.schedule.note") }}
        </p>
      </div>
    </section>
  </div>
</template>
