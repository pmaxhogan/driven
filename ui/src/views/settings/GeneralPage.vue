<script setup lang="ts">
import { useI18n } from "vue-i18n";

import { useSettingsStore } from "../../stores/settings";
import { useSettingsForm } from "../../composables/useSettingsForm";
import { cardCls, inputCls, ensureSettingsLoaded } from "./shared";

// General settings page (SDD 2026-08-02 settings-sidebar-ia, task 2). Holds
// ONLY the scan interval for now - moved verbatim out of Settings.vue's old
// "Performance and bandwidth" card, which split across three pages (scan
// interval here, deep-verify interval -> AdvancedPage, VSS/APFS -> Platform).
// The update-channel selector and check-for-updates action join this page in
// task 5.
const { t } = useI18n();
const settings = useSettingsStore();
const { commitPatch, parseRequiredClamped, RANGES } = useSettingsForm();

ensureSettingsLoaded();

async function commitScanInterval(event: Event): Promise<void> {
  const current = settings.settings?.global.scanIntervalSecs ?? 600;
  const value = parseRequiredClamped(
    (event.target as HTMLInputElement).value,
    RANGES.scanIntervalSecs,
    current
  );
  await commitPatch({ global: { scanIntervalSecs: value } });
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

    <section class="space-y-3" :class="cardCls">
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
    </section>
  </div>
</template>
