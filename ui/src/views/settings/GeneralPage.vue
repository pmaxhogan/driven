<script setup lang="ts">
import { onMounted } from "vue";
import { useI18n } from "vue-i18n";

import { useSettingsStore } from "../../stores/settings";
import { useUpdaterStore } from "../../stores/updater";
import { useSettingsForm } from "../../composables/useSettingsForm";
import { cardCls, inputCls, ensureSettingsLoaded } from "./shared";

// General settings page (SDD 2026-08-02 settings-sidebar-ia, task 2). Holds
// the scan interval, plus - as of task 5 - the update-channel selector and a
// check-for-updates action, both moved verbatim out of About.vue, and a
// display-language placeholder note (About's old Privacy card, which also
// held telemetry - now PrivacyPage's sole territory). About.vue KEEPS its own
// check-for-updates action too (DESIGN mock F allows the action to exist in
// both places); the channel + check-status markup below duplicates About's
// rather than being extracted into a shared composable, since both call sites
// just render the same `useUpdaterStore` fields with no state of their own -
// a composable here would be more indirection than the ~15 duplicated lines
// it would save.
const { t } = useI18n();
const settings = useSettingsStore();
const updater = useUpdaterStore();
const { commitPatch, parseRequiredClamped, RANGES } = useSettingsForm();

ensureSettingsLoaded();

const channels = ["stable", "dev"] as const;

// Shared design-system class string (DRIVEN UI design system), duplicated
// from About.vue rather than added to shared.ts - see the module comment.
const primaryBtn =
  "inline-flex items-center justify-center gap-2 rounded-md bg-teal-700 px-4 py-2 text-sm font-medium text-white shadow-xs transition-colors hover:bg-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50";

onMounted(() => {
  void updater.loadChannel();
});

async function commitScanInterval(event: Event): Promise<void> {
  const current = settings.settings?.global.scanIntervalSecs ?? 600;
  const value = parseRequiredClamped(
    (event.target as HTMLInputElement).value,
    RANGES.scanIntervalSecs,
    current
  );
  await commitPatch({ global: { scanIntervalSecs: value } });
}

async function onChannelChange(event: Event): Promise<void> {
  const value = (event.target as HTMLSelectElement).value;
  await updater.setChannel(value);
}

/** Localize a SPEC s24 error code, falling back to a generic message. */
function localizeError(code: string | null): string {
  if (code === null) return "";
  return t(`errors.${code}.long`);
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

    <!-- Updates: channel + check action + status (moved verbatim from
         About.vue, task 5) -->
    <section class="space-y-3" :class="cardCls">
      <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
        {{ t("about.updatesTitle") }}
      </h3>
      <label class="block max-w-xs space-y-1 text-sm">
        <span class="text-zinc-600 dark:text-zinc-400">{{ t("about.channelLabel") }}</span>
        <select
          :value="updater.channel"
          class="w-full"
          :class="inputCls"
          data-testid="channel-select"
          @change="onChannelChange"
        >
          <option v-for="ch in channels" :key="ch" :value="ch">
            {{ t(`about.channel.${ch}`) }}
          </option>
        </select>
      </label>

      <div class="space-y-2">
        <button
          type="button"
          :class="primaryBtn"
          :disabled="updater.checking"
          data-testid="check-updates"
          @click="updater.check()"
        >
          {{ t("about.checkForUpdatesButton") }}
        </button>
        <p v-if="updater.checking" class="text-sm text-zinc-500">
          {{ t("common.loading") }}
        </p>
        <p
          v-else-if="updater.checkErrorCode"
          class="text-sm text-red-600"
          data-testid="check-error"
        >
          {{ localizeError(updater.checkErrorCode) }}
        </p>
        <p
          v-else-if="updater.checked && updater.available"
          class="text-sm"
          data-testid="check-available"
        >
          {{ t("about.updateAvailable", { version: updater.available.version }) }}
        </p>
        <p v-else-if="updater.checked" class="text-sm text-zinc-500" data-testid="check-uptodate">
          {{ t("about.upToDate") }}
        </p>
      </div>
    </section>

    <!-- Display language (placeholder note, moved from About's old Privacy
         card - task 5). -->
    <section class="space-y-1 text-sm" :class="cardCls">
      <p class="text-sm text-zinc-500">
        <span class="text-zinc-600 dark:text-zinc-400">{{ t("about.displayLanguageLabel") }}:</span>
        {{ t("about.moreLanguagesComing") }}
      </p>
    </section>
  </div>
</template>
