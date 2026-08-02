<script setup lang="ts">
import { ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import { useSettingsStore } from "../../stores/settings";
import { useSettingsForm } from "../../composables/useSettingsForm";
import { cardCls, inputCls, ensureSettingsLoaded } from "./shared";

// Performance settings page (SDD 2026-08-02 settings-sidebar-ia, task 2).
// Moved verbatim out of Settings.vue's "Performance and bandwidth" card:
// bandwidthCapMbps, defaultConcurrentUploads, adaptiveParallelismEnabled and
// ioPriority. The rest of that card's old contents split elsewhere - scan
// interval -> GeneralPage, deep-verify interval -> AdvancedPage, VSS/APFS
// locked-file backup -> PlatformPage.
const { t } = useI18n();
const settings = useSettingsStore();
const { commitPatch, parseOptionalClamped, RANGES } = useSettingsForm();

ensureSettingsLoaded();

const ioPriorities = ["normal", "low", "idle"] as const;

// Local editable mirrors of the numeric "nullable = special" fields, so the
// bound <input> can be empty (= the special value) without fighting the store.
const bandwidthCapText = ref("");
const concurrentUploadsText = ref("");

watch(
  () => settings.settings,
  (s) => {
    if (!s) return;
    bandwidthCapText.value =
      s.global.bandwidthCapMbps === null ? "" : String(s.global.bandwidthCapMbps);
    concurrentUploadsText.value =
      s.global.defaultConcurrentUploads === null ? "" : String(s.global.defaultConcurrentUploads);
  },
  { immediate: true }
);

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

// DESIGN 11.4.7: adaptive upload parallelism (default ON). When on, the
// in-flight pool grows/shrinks with measured throughput + disk-busy starting
// from the concurrency setting above; when off, the pool is pinned at it.
async function setAdaptiveParallelism(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ global: { adaptiveParallelismEnabled: checked } });
}

async function setIoPriority(event: Event): Promise<void> {
  const value = (event.target as HTMLSelectElement).value;
  await commitPatch({ global: { ioPriority: value } });
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

      <label class="flex items-center gap-2">
        <input
          type="checkbox"
          class="accent-teal-600"
          data-testid="adaptive-parallelism-toggle"
          :checked="settings.settings.global.adaptiveParallelismEnabled"
          @change="setAdaptiveParallelism"
        />
        {{ t("settings.rules.adaptiveParallelismLabel") }}
      </label>
      <p class="text-xs text-zinc-500 dark:text-zinc-400">
        {{ t("settings.rules.adaptiveParallelismNote") }}
      </p>

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
    </section>
  </div>
</template>
