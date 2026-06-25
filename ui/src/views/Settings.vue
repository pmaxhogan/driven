<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useRouter } from "vue-router";
import { useI18n } from "vue-i18n";

import AccountList from "../components/AccountList.vue";
import SourceTable from "../components/SourceTable.vue";
import { useSettingsStore } from "../stores/settings";

// Settings view (SPEC s25 /accounts, /sources, /rules; DESIGN s8.2). One view
// hosts the three routed tabs; the active tab comes from the route (router
// passes `tab` as a prop). The Accounts + Sources tabs render their components;
// the Rules tab is the global-rules form (SPEC s22 `global` + Windows `vss_mode`)
// editing the settings store. About is its own route (/about -> About.vue) per
// the s25 route map, so it is not a tab here.
const props = withDefaults(
  defineProps<{ tab?: "accounts" | "sources" | "rules" }>(),
  { tab: "accounts" },
);

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

// Local editable mirrors of the numeric "nullable = special" fields, so the
// bound <input> can be empty (= the special value) without fighting the store.
const bandwidthCapText = ref("");
const concurrentUploadsText = ref("");

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
  },
  { immediate: true },
);

// Keep the local numeric mirrors in sync with the loaded snapshot.
watch(
  () => settings.settings,
  (s) => {
    if (!s) return;
    bandwidthCapText.value =
      s.global.bandwidthCapMbps === null
        ? ""
        : String(s.global.bandwidthCapMbps);
    concurrentUploadsText.value =
      s.global.defaultConcurrentUploads === null
        ? ""
        : String(s.global.defaultConcurrentUploads);
  },
  { immediate: true },
);

// Accept `string | number`: an `<input type="number">` bound with `v-model`
// yields a number, while an `event.target.value` read yields a string. Coerce
// to a trimmed string first so neither call site crashes on `.trim()`.
function parseOptionalPositiveInt(input: string | number): number | null {
  const trimmed = String(input).trim();
  if (trimmed === "") return null;
  const value = Number(trimmed);
  if (!Number.isFinite(value) || value <= 0) return null;
  return Math.floor(value);
}

function parsePositiveInt(input: string | number, fallback: number): number {
  const value = Number(String(input).trim());
  if (!Number.isFinite(value) || value <= 0) return fallback;
  return Math.floor(value);
}

async function setSkipOnBattery(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await settings.patch({ global: { skipOnBattery: checked } });
}

async function setSkipOnMetered(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await settings.patch({ global: { skipOnMetered: checked } });
}

async function commitBandwidthCap(): Promise<void> {
  await settings.patch({
    global: { bandwidthCapMbps: parseOptionalPositiveInt(bandwidthCapText.value) },
  });
}

async function commitConcurrentUploads(): Promise<void> {
  await settings.patch({
    global: {
      defaultConcurrentUploads: parseOptionalPositiveInt(
        concurrentUploadsText.value,
      ),
    },
  });
}

async function commitScanInterval(event: Event): Promise<void> {
  const current = settings.settings?.global.scanIntervalSecs ?? 600;
  const value = parsePositiveInt((event.target as HTMLInputElement).value, current);
  await settings.patch({ global: { scanIntervalSecs: value } });
}

async function commitDeepVerifyInterval(event: Event): Promise<void> {
  const current = settings.settings?.global.deepVerifyIntervalSecs ?? 604800;
  const value = parsePositiveInt((event.target as HTMLInputElement).value, current);
  await settings.patch({ global: { deepVerifyIntervalSecs: value } });
}

async function setIoPriority(event: Event): Promise<void> {
  const value = (event.target as HTMLSelectElement).value;
  await settings.patch({ global: { ioPriority: value } });
}

async function setVssMode(event: Event): Promise<void> {
  const value = (event.target as HTMLSelectElement).value;
  await settings.patch({ windows: { vssMode: value } });
}

// SPEC s16 (M9b R2-P1-1): toggle anonymous usage telemetry (default ON) via the
// DEDICATED set_telemetry_enabled command, so the backend flips the in-flight ping
// cancel flag immediately - a disable click while a ping is building still aborts
// that send (the generic update_settings path would too, but this is explicit).
async function setTelemetryEnabled(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await settings.setTelemetryEnabled(checked);
}
</script>

<template>
  <section class="space-y-4">
    <h1 class="text-2xl font-semibold">
      {{ t("settings.title") }}
    </h1>

    <nav class="flex gap-2 border-b text-sm">
      <button
        v-for="tabItem in tabs"
        :key="tabItem.key"
        type="button"
        class="px-3 py-2"
        :class="
          active === tabItem.key
            ? 'border-b-2 border-zinc-900 dark:border-zinc-100 font-medium'
            : 'text-zinc-500'
        "
        @click="go(tabItem.route)"
      >
        {{ t(tabItem.label) }}
      </button>
    </nav>

    <AccountList v-if="active === 'accounts'" />
    <SourceTable v-else-if="active === 'sources'" />
    <div
      v-else
      class="space-y-4"
    >
      <h2 class="text-lg font-medium">
        {{ t("settings.rules.title") }}
      </h2>

      <p
        v-if="settings.loading"
        class="text-sm text-zinc-500"
      >
        {{ t("common.loading") }}
      </p>
      <p
        v-else-if="settings.error"
        class="text-sm text-red-600"
      >
        {{ settings.error }}
      </p>
      <div
        v-else-if="settings.settings"
        class="max-w-md space-y-4 text-sm"
        data-testid="rules-form"
      >
        <label class="flex items-center gap-2">
          <input
            type="checkbox"
            :checked="settings.settings.global.skipOnBattery"
            @change="setSkipOnBattery"
          >
          {{ t("settings.rules.skipOnBatteryLabel") }}
        </label>

        <label class="flex items-center gap-2">
          <input
            type="checkbox"
            :checked="settings.settings.global.skipOnMetered"
            @change="setSkipOnMetered"
          >
          {{ t("settings.rules.skipOnMeteredLabel") }}
        </label>

        <label class="block space-y-1">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.rules.bandwidthCapLabel")
          }}</span>
          <input
            v-model="bandwidthCapText"
            type="number"
            min="1"
            :placeholder="t('settings.rules.bandwidthCapUnlimited')"
            class="w-full rounded border px-2 py-1"
            @change="commitBandwidthCap"
          >
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
            :placeholder="t('settings.rules.concurrentUploadsAuto')"
            class="w-full rounded border px-2 py-1"
            @change="commitConcurrentUploads"
          >
        </label>

        <label class="block space-y-1">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.rules.scanIntervalLabel")
          }}</span>
          <input
            type="number"
            min="1"
            :value="settings.settings.global.scanIntervalSecs"
            class="w-full rounded border px-2 py-1"
            @change="commitScanInterval"
          >
        </label>

        <label class="block space-y-1">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.rules.deepVerifyIntervalLabel")
          }}</span>
          <input
            type="number"
            min="1"
            :value="settings.settings.global.deepVerifyIntervalSecs"
            class="w-full rounded border px-2 py-1"
            @change="commitDeepVerifyInterval"
          >
        </label>

        <label class="block space-y-1">
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.rules.ioPriorityLabel")
          }}</span>
          <select
            :value="settings.settings.global.ioPriority"
            class="w-full rounded border px-2 py-1"
            @change="setIoPriority"
          >
            <option
              v-for="priority in ioPriorities"
              :key="priority"
              :value="priority"
            >
              {{ t(`settings.rules.ioPriority.${priority}`) }}
            </option>
          </select>
        </label>

        <label
          v-if="settings.settings.windows"
          class="block space-y-1"
        >
          <span class="text-zinc-600 dark:text-zinc-400">{{
            t("settings.rules.vssModeLabel")
          }}</span>
          <select
            :value="settings.settings.windows.vssMode"
            class="w-full rounded border px-2 py-1"
            @change="setVssMode"
          >
            <option
              v-for="mode in vssModes"
              :key="mode"
              :value="mode"
            >
              {{ t(`settings.rules.vssMode.${mode}`) }}
            </option>
          </select>
        </label>

        <div
          class="space-y-1 border-t pt-4"
          data-testid="telemetry-setting"
        >
          <label class="flex items-center gap-2">
            <input
              type="checkbox"
              data-testid="telemetry-toggle"
              :checked="settings.settings.telemetry.enabled"
              @change="setTelemetryEnabled"
            >
            {{ t("settings.rules.telemetryLabel") }}
          </label>
          <p class="text-xs text-zinc-500">
            {{ t("settings.rules.telemetryNote") }}
          </p>
        </div>
      </div>
    </div>
  </section>
</template>
