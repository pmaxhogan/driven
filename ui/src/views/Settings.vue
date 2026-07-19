<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useRouter } from "vue-router";
import { useI18n } from "vue-i18n";

import AccountList from "../components/AccountList.vue";
import SourceTable from "../components/SourceTable.vue";
import { useSettingsStore } from "../stores/settings";
import { getVssHelperStatus } from "../ipc/commands";
import type { SettingsPatch, VssHelperStatus } from "../ipc/types";

// Settings view (SPEC s25 /accounts, /sources, /rules; DESIGN s8.2). One view
// hosts the three routed tabs; the active tab comes from the route (router
// passes `tab` as a prop). The Accounts + Sources tabs render their components;
// the Rules tab is the global-rules form (SPEC s22 `global` + Windows `vss_mode`)
// editing the settings store. About is its own route (/about -> About.vue) per
// the s25 route map, so it is not a tab here.
const props = withDefaults(defineProps<{ tab?: "accounts" | "sources" | "rules" }>(), {
  tab: "accounts",
});

const { t } = useI18n();
const router = useRouter();
const settings = useSettingsStore();

const tabs = [
  { key: "accounts", route: "/accounts", label: "settings.tabs.accounts" },
  { key: "sources", route: "/sources", label: "settings.tabs.sources" },
  { key: "rules", route: "/rules", label: "settings.tabs.rules" },
] as const;

const active = computed(() => props.tab);

const ioPriorities = ["normal", "low", "idle"] as const;
const vssModes = ["auto", "always", "never"] as const;

// Least-privilege locked-file backup status (DESIGN s5.3.1). Drives the Rules-tab
// banner shown when locked files (Outlook / DB / VM disks) are being skipped
// because Volume Shadow Copy is unavailable (no elevation, no active helper).
const vssStatus = ref<VssHelperStatus | null>(null);
const showVssBanner = computed(() => vssStatus.value?.lockedFileBackupDegraded ?? false);

// Shared design-system class strings (DRIVEN UI design system). Native controls
// MUST carry explicit light/dark surface + text colors so they stay readable on a
// dark-theme OS; teal is the accent for focus rings.
const inputCls =
  "rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 transition-colors focus:border-teal-500 focus:outline-none focus:ring-2 focus:ring-teal-500/40 disabled:opacity-60 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100";
const cardCls =
  "rounded-lg border border-zinc-200 bg-white p-4 shadow-sm dark:border-zinc-800 dark:bg-zinc-900";

// Local editable mirrors of the numeric "nullable = special" fields, so the
// bound <input> can be empty (= the special value) without fighting the store.
const bandwidthCapText = ref("");
const concurrentUploadsText = ref("");

// Schedule-window (DESIGN s17) local mirrors. Times are edited as "HH:MM"
// strings (native <input type="time">); days[0]=Sunday..[6]=Saturday.
const dayIndices = [0, 1, 2, 3, 4, 5, 6] as const;
const scheduleEnabled = ref(false);
const scheduleStart = ref("00:00");
const scheduleEnd = ref("00:00");
const scheduleDays = ref<boolean[]>([true, true, true, true, true, true, true]);

// Pre/post backup hook local mirrors (DESIGN s17).
const preBackupHook = ref("");
const postBackupHook = ref("");
const hookTimeoutSecs = ref(60);

// Metered pause-or-throttle local mirrors (DESIGN s17).
const meteredModes = ["pause", "throttle"] as const;
const meteredMode = ref("pause");
const meteredCapText = ref("");

function minutesToHHMM(min: number): string {
  const m = ((Math.floor(min) % 1440) + 1440) % 1440;
  const hh = String(Math.floor(m / 60)).padStart(2, "0");
  const mm = String(m % 60).padStart(2, "0");
  return `${hh}:${mm}`;
}

function hhmmToMinutes(value: string): number {
  const [h, m] = value.split(":").map((n) => Number(n));
  if (!Number.isFinite(h) || !Number.isFinite(m)) return 0;
  return (((h * 60 + m) % 1440) + 1440) % 1440;
}

function go(route: string): void {
  void router.push(route);
}

// Load settings whenever the Rules tab becomes active (deep-linkable). The
// immediate run covers the deep-link / first-render case.
watch(
  active,
  (value) => {
    if (value === "rules" && settings.settings === null) {
      void settings.refresh();
    }
    if (value === "rules") {
      void getVssHelperStatus()
        .then((s) => (vssStatus.value = s))
        .catch(() => {
          // No status (e.g. IPC unavailable in a browser dev shell): hide the
          // banner rather than surface an error on the Rules tab.
          vssStatus.value = null;
        });
    }
  },
  { immediate: true }
);

// Keep the local numeric mirrors in sync with the loaded snapshot.
watch(
  () => settings.settings,
  (s) => {
    if (!s) return;
    bandwidthCapText.value =
      s.global.bandwidthCapMbps === null ? "" : String(s.global.bandwidthCapMbps);
    concurrentUploadsText.value =
      s.global.defaultConcurrentUploads === null ? "" : String(s.global.defaultConcurrentUploads);
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
    preBackupHook.value = s.global.preBackupHook ?? "";
    postBackupHook.value = s.global.postBackupHook ?? "";
    hookTimeoutSecs.value = s.global.hookTimeoutSecs ?? 60;
    meteredMode.value = s.global.meteredMode ?? "pause";
    meteredCapText.value =
      s.global.meteredBandwidthCapMbps === null ? "" : String(s.global.meteredBandwidthCapMbps);
  },
  { immediate: true }
);

// Backend-enforced numeric ranges (mirror of src-tauri/src/commands/settings.rs:
// check_range bounds). We clamp every numeric field to its range BEFORE patching
// so a typed out-of-range value (e.g. 100 concurrent uploads, a 10s scan
// interval) is corrected in place and never round-trips to a backend rejection -
// the rejection used to brick the whole Rules form. The backend still validates;
// this just keeps the UI from ever sending a value it will refuse.
const RANGES = {
  bandwidthCapMbps: [1, 100_000],
  meteredBandwidthCapMbps: [1, 100_000],
  defaultConcurrentUploads: [1, 32],
  scanIntervalSecs: [30, 604_800],
  deepVerifyIntervalSecs: [3_600, 31_536_000],
  hookTimeoutSecs: [1, 86_400],
} as const;

function clampToRange(value: number, [min, max]: readonly [number, number]): number {
  return Math.min(max, Math.max(min, value));
}

// Accept `string | number`: an `<input type="number">` bound with `v-model`
// yields a number, while an `event.target.value` read yields a string. Coerce
// to a trimmed string first so neither call site crashes on `.trim()`.
//
// Parse an OPTIONAL field ("" = the special "null"/unlimited/auto value), clamped
// to its backend range when a value is present.
function parseOptionalClamped(
  input: string | number,
  range: readonly [number, number]
): number | null {
  const trimmed = String(input).trim();
  if (trimmed === "") return null;
  const value = Number(trimmed);
  if (!Number.isFinite(value)) return null;
  return clampToRange(Math.floor(value), range);
}

// Parse a REQUIRED field, clamped to its backend range; a non-numeric input
// keeps the current value (fallback).
function parseRequiredClamped(
  input: string | number,
  range: readonly [number, number],
  fallback: number
): number {
  const value = Number(String(input).trim());
  if (!Number.isFinite(value)) return fallback;
  return clampToRange(Math.floor(value), range);
}

// All Rules commits route through here. The store records the failure as
// `errorCode` (rendered as the inline banner above the form), so we SWALLOW the
// rejection rather than let it escape the @change handler as an unhandled promise
// rejection (which produced a Vue "Unhandled error during execution of native
// event handler" warning). The form stays usable; the banner explains the error.
async function commitPatch(p: SettingsPatch): Promise<void> {
  try {
    await settings.patch(p);
  } catch {
    // errorCode is set on the store and surfaced as the banner.
  }
}

// Issue #58: launch Driven at login (default ON). Patches the persisted
// preference; the backend registers/unregisters the real OS startup entry.
async function setAutoStartOnLogin(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ global: { autoStartOnLogin: checked } });
}

async function setSkipOnBattery(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ global: { skipOnBattery: checked } });
}

async function setSkipOnMetered(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ global: { skipOnMetered: checked } });
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

async function commitBandwidthCap(): Promise<void> {
  await commitPatch({
    global: {
      bandwidthCapMbps: parseOptionalClamped(bandwidthCapText.value, RANGES.bandwidthCapMbps),
    },
  });
}

async function commitConcurrentUploads(): Promise<void> {
  await commitPatch({
    global: {
      defaultConcurrentUploads: parseOptionalClamped(
        concurrentUploadsText.value,
        RANGES.defaultConcurrentUploads
      ),
    },
  });
}

async function commitScanInterval(event: Event): Promise<void> {
  const current = settings.settings?.global.scanIntervalSecs ?? 600;
  const value = parseRequiredClamped(
    (event.target as HTMLInputElement).value,
    RANGES.scanIntervalSecs,
    current
  );
  await commitPatch({ global: { scanIntervalSecs: value } });
}

async function commitDeepVerifyInterval(event: Event): Promise<void> {
  const current = settings.settings?.global.deepVerifyIntervalSecs ?? 604800;
  const value = parseRequiredClamped(
    (event.target as HTMLInputElement).value,
    RANGES.deepVerifyIntervalSecs,
    current
  );
  await commitPatch({ global: { deepVerifyIntervalSecs: value } });
}

// Backup hooks (DESIGN s17). A blank command clears the hook (sent as null).
async function commitPreHook(): Promise<void> {
  const cmd = preBackupHook.value.trim();
  await commitPatch({ global: { preBackupHook: cmd === "" ? null : cmd } });
}

async function commitPostHook(): Promise<void> {
  const cmd = postBackupHook.value.trim();
  await commitPatch({ global: { postBackupHook: cmd === "" ? null : cmd } });
}

async function commitHookTimeout(event: Event): Promise<void> {
  const current = settings.settings?.global.hookTimeoutSecs ?? 60;
  const value = parseRequiredClamped(
    (event.target as HTMLInputElement).value,
    RANGES.hookTimeoutSecs,
    current
  );
  await commitPatch({ global: { hookTimeoutSecs: value } });
}

async function setIoPriority(event: Event): Promise<void> {
  const value = (event.target as HTMLSelectElement).value;
  await commitPatch({ global: { ioPriority: value } });
}

// Persist the whole schedule window. The UTC offset is captured fresh from
// this machine on every save (DESIGN s17 - driven-core stays tz-database-free
// and reasons from a fixed offset). `getTimezoneOffset()` returns minutes to
// SUBTRACT to reach UTC, so negate it to get "minutes to add to UTC".
async function commitSchedule(): Promise<void> {
  await commitPatch({
    global: {
      schedule: {
        enabled: scheduleEnabled.value,
        startMinute: hhmmToMinutes(scheduleStart.value),
        endMinute: hhmmToMinutes(scheduleEnd.value),
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

async function setVssMode(event: Event): Promise<void> {
  const value = (event.target as HTMLSelectElement).value;
  await commitPatch({ windows: { vssMode: value } });
}

// SPEC s16 (M9b R2-P1-1): toggle anonymous usage telemetry (default ON) via the
// DEDICATED set_telemetry_enabled command, so the backend flips the in-flight ping
// cancel flag immediately - a disable click while a ping is building still aborts
// that send (the generic update_settings path would too, but this is explicit).
async function setTelemetryEnabled(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  try {
    await settings.setTelemetryEnabled(checked);
  } catch {
    // errorCode is set on the store and surfaced as the banner; swallow so the
    // toggle's @change handler never escapes as an unhandled rejection.
  }
}
</script>

<template>
  <section class="space-y-4">
    <h1 class="text-2xl font-semibold">
      {{ t("settings.title") }}
    </h1>

    <nav class="flex gap-1 border-b border-zinc-200 text-sm dark:border-zinc-800">
      <button
        v-for="tabItem in tabs"
        :key="tabItem.key"
        type="button"
        class="-mb-px rounded-t px-3 py-2 transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500"
        :class="
          active === tabItem.key
            ? 'border-b-2 border-teal-600 font-medium text-teal-700 dark:text-teal-300'
            : 'text-zinc-600 hover:text-teal-700 dark:text-zinc-400 dark:hover:text-teal-300'
        "
        :aria-current="active === tabItem.key ? 'page' : undefined"
        @click="go(tabItem.route)"
      >
        {{ t(tabItem.label) }}
      </button>
    </nav>

    <AccountList v-if="active === 'accounts'" />
    <SourceTable v-else-if="active === 'sources'" />
    <div v-else class="space-y-4">
      <h2 class="text-lg font-medium">
        {{ t("settings.rules.title") }}
      </h2>

      <p v-if="settings.loading && !settings.settings" class="text-sm text-zinc-500">
        {{ t("common.loading") }}
      </p>
      <p
        v-else-if="!settings.settings && settings.errorCode"
        class="text-sm text-red-600"
        role="alert"
      >
        {{ t(`errors.${settings.errorCode}.long`) }}
      </p>
      <div
        v-else-if="settings.settings"
        class="max-w-2xl space-y-4 text-sm"
        data-testid="rules-form"
      >
        <!-- A rejected patch shows here as an inline banner WITHOUT hiding the
             form, so the user can correct the value. Previously the v-else-if
             chain replaced the entire form with the raw error ("[object Object]")
             and the page was unrecoverable until an app restart. Client-side
             clamping (below) makes a backend rejection rare; this is the safety
             net for the non-numeric cases. -->
        <p
          v-if="settings.errorCode"
          class="rounded-md bg-red-50 px-3 py-2 text-sm text-red-700 dark:bg-red-950/40 dark:text-red-300"
          role="alert"
          data-testid="rules-error"
        >
          {{ t(`errors.${settings.errorCode}.long`) }}
        </p>
        <!-- Startup -->
        <section class="space-y-3" :class="cardCls" data-testid="startup-setting">
          <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
            {{ t("settings.rules.sections.startup") }}
          </h3>
          <label class="flex items-center gap-2">
            <input
              type="checkbox"
              class="accent-teal-600"
              data-testid="autostart-toggle"
              :checked="settings.settings.global.autoStartOnLogin"
              @change="setAutoStartOnLogin"
            />
            {{ t("settings.rules.autoStartOnLoginLabel") }}
          </label>
          <p class="text-xs text-zinc-500 dark:text-zinc-400">
            {{ t("settings.rules.autoStartOnLoginNote") }}
          </p>
        </section>

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
                <input
                  v-model="scheduleStart"
                  type="time"
                  :class="inputCls"
                  @change="commitSchedule"
                />
              </label>
              <label class="block space-y-1">
                <span class="text-zinc-600 dark:text-zinc-400">{{
                  t("settings.rules.schedule.endLabel")
                }}</span>
                <input
                  v-model="scheduleEnd"
                  type="time"
                  :class="inputCls"
                  @change="commitSchedule"
                />
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
                  class="rounded-md border px-2 py-1 text-xs transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500"
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

        <!-- Performance and bandwidth -->
        <section class="space-y-3" :class="cardCls">
          <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
            {{ t("settings.rules.sections.performance") }}
          </h3>
          <label class="block space-y-1">
            <span class="text-zinc-600 dark:text-zinc-400">{{
              t("settings.rules.bandwidthCapLabel")
            }}</span>
            <input
              v-model="bandwidthCapText"
              type="number"
              min="1"
              max="100000"
              class="w-full"
              :class="inputCls"
              :placeholder="t('settings.rules.bandwidthCapUnlimited')"
              @change="commitBandwidthCap"
            />
          </label>

          <label class="block space-y-1">
            <span class="text-zinc-600 dark:text-zinc-400">{{
              t("settings.rules.concurrentUploadsLabel")
            }}</span>
            <input
              v-model="concurrentUploadsText"
              type="number"
              min="1"
              max="32"
              class="w-full"
              :class="inputCls"
              :placeholder="t('settings.rules.concurrentUploadsAuto')"
              @change="commitConcurrentUploads"
            />
          </label>

          <label class="block space-y-1">
            <span class="text-zinc-600 dark:text-zinc-400">{{
              t("settings.rules.scanIntervalLabel")
            }}</span>
            <input
              type="number"
              min="30"
              max="604800"
              class="w-full"
              :class="inputCls"
              :value="settings.settings.global.scanIntervalSecs"
              @change="commitScanInterval"
            />
          </label>

          <label class="block space-y-1">
            <span class="text-zinc-600 dark:text-zinc-400">{{
              t("settings.rules.deepVerifyIntervalLabel")
            }}</span>
            <input
              type="number"
              min="3600"
              max="31536000"
              class="w-full"
              :class="inputCls"
              :value="settings.settings.global.deepVerifyIntervalSecs"
              @change="commitDeepVerifyInterval"
            />
          </label>

          <label class="block space-y-1">
            <span class="text-zinc-600 dark:text-zinc-400">{{
              t("settings.rules.ioPriorityLabel")
            }}</span>
            <select
              class="w-full"
              :class="inputCls"
              :value="settings.settings.global.ioPriority"
              @change="setIoPriority"
            >
              <option v-for="priority in ioPriorities" :key="priority" :value="priority">
                {{ t(`settings.rules.ioPriority.${priority}`) }}
              </option>
            </select>
          </label>

          <div
            v-if="showVssBanner"
            data-testid="vss-degraded-banner"
            class="rounded-lg border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-800/60 dark:bg-amber-950/40"
          >
            <h4 class="font-semibold text-amber-800 dark:text-amber-300">
              {{ t("settings.rules.vssBanner.title") }}
            </h4>
            <p class="mt-1 text-amber-700 dark:text-amber-200/80">
              {{ t("settings.rules.vssBanner.body") }}
            </p>
          </div>

          <label v-if="settings.settings.windows" class="block space-y-1">
            <span class="text-zinc-600 dark:text-zinc-400">{{
              t("settings.rules.vssModeLabel")
            }}</span>
            <select
              class="w-full"
              :class="inputCls"
              :value="settings.settings.windows.vssMode"
              @change="setVssMode"
            >
              <option v-for="mode in vssModes" :key="mode" :value="mode">
                {{ t(`settings.rules.vssMode.${mode}`) }}
              </option>
            </select>
          </label>
        </section>

        <!-- Backup hooks -->
        <section class="space-y-2" :class="cardCls" data-testid="hooks-setting">
          <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
            {{ t("settings.rules.hooks.title") }}
          </h3>
          <label class="block space-y-1">
            <span class="text-zinc-600 dark:text-zinc-400">{{
              t("settings.rules.hooks.preLabel")
            }}</span>
            <input
              v-model="preBackupHook"
              type="text"
              data-testid="pre-hook"
              class="w-full font-mono"
              :class="inputCls"
              :placeholder="t('settings.rules.hooks.placeholder')"
              @change="commitPreHook"
            />
          </label>
          <label class="block space-y-1">
            <span class="text-zinc-600 dark:text-zinc-400">{{
              t("settings.rules.hooks.postLabel")
            }}</span>
            <input
              v-model="postBackupHook"
              type="text"
              data-testid="post-hook"
              class="w-full font-mono"
              :class="inputCls"
              :placeholder="t('settings.rules.hooks.placeholder')"
              @change="commitPostHook"
            />
          </label>
          <label class="block space-y-1">
            <span class="text-zinc-600 dark:text-zinc-400">{{
              t("settings.rules.hooks.timeoutLabel")
            }}</span>
            <input
              type="number"
              min="1"
              max="86400"
              class="w-full"
              :class="inputCls"
              :value="hookTimeoutSecs"
              @change="commitHookTimeout"
            />
          </label>
          <p class="text-xs text-zinc-500">
            {{ t("settings.rules.hooks.note") }}
          </p>
        </section>

        <!-- Privacy -->
        <section class="space-y-1" :class="cardCls" data-testid="telemetry-setting">
          <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
            {{ t("settings.rules.sections.privacy") }}
          </h3>
          <label class="flex items-center gap-2">
            <input
              type="checkbox"
              class="accent-teal-600"
              data-testid="telemetry-toggle"
              :checked="settings.settings.telemetry.enabled"
              @change="setTelemetryEnabled"
            />
            {{ t("settings.rules.telemetryLabel") }}
          </label>
          <p class="text-xs text-zinc-500">
            {{ t("settings.rules.telemetryNote") }}
          </p>
        </section>
      </div>
    </div>
  </section>
</template>
