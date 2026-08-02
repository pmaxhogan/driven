<script setup lang="ts">
import { ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import { useSettingsStore } from "../../stores/settings";
import { useSettingsForm } from "../../composables/useSettingsForm";
import { cardCls, inputCls, ensureSettingsLoaded } from "./shared";

// Advanced settings page (SDD 2026-08-02 settings-sidebar-ia, task 2). Moved
// verbatim out of Settings.vue: the deep-verify interval field (from the old
// "Performance and bandwidth" card), small-file bundling (issue #35),
// scheduled integrity scrub, and backup hooks.
const { t } = useI18n();
const settings = useSettingsStore();
const { commitPatch, parseRequiredClamped, RANGES } = useSettingsForm();

ensureSettingsLoaded();

// Pre/post backup hook local mirrors (DESIGN s17).
const preBackupHook = ref("");
const postBackupHook = ref("");
const hookTimeoutSecs = ref(60);

// Scheduled integrity-scrub local mirrors. The cadence is stored in SECONDS but
// entered in HOURS (a weekly default reads as `168`, not `604800`).
const scrubIntervalHoursText = ref("");
const scrubSliceText = ref("");
const scrubDeepSampleText = ref("");

watch(
  () => settings.settings,
  (s) => {
    if (!s) return;
    preBackupHook.value = s.global.preBackupHook ?? "";
    postBackupHook.value = s.global.postBackupHook ?? "";
    hookTimeoutSecs.value = s.global.hookTimeoutSecs ?? 60;
    // Scrub: seconds -> hours for display. A settings snapshot written by an
    // older build may not carry the group at all, so fall back to the shipped
    // defaults rather than rendering NaN.
    scrubIntervalHoursText.value = String(Math.round((s.scrub?.intervalSecs ?? 604_800) / 3600));
    scrubSliceText.value = String(s.scrub?.sliceSize ?? 500);
    scrubDeepSampleText.value = String(s.scrub?.deepSample ?? 0);
  },
  { immediate: true }
);

async function commitDeepVerifyInterval(event: Event): Promise<void> {
  const current = settings.settings?.global.deepVerifyIntervalSecs ?? 604800;
  const value = parseRequiredClamped(
    (event.target as HTMLInputElement).value,
    RANGES.deepVerifyIntervalSecs,
    current
  );
  await commitPatch({ global: { deepVerifyIntervalSecs: value } });
}

// Issue #35: opt-in small-file bundling (default OFF). A standalone advanced
// toggle - the backend writes the `bundle_small_files` KV key the core planner
// reads; the thresholds stay backend-only.
async function setBundleSmallFiles(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ bundleSmallFiles: checked });
}

// Scheduled integrity scrub. The cadence is stored in SECONDS but shown in
// HOURS: a weekly default is `604800`, which is unreadable as a number in a box,
// while `168` is a duration a person can reason about.
async function setScrubEnabled(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  await commitPatch({ scrub: { enabled: checked } });
}

async function commitScrubInterval(): Promise<void> {
  const hours = parseRequiredClamped(
    scrubIntervalHoursText.value,
    RANGES.scrubIntervalHours,
    Math.round((settings.settings?.scrub?.intervalSecs ?? 604_800) / 3600)
  );
  scrubIntervalHoursText.value = String(hours);
  await commitPatch({ scrub: { intervalSecs: hours * 3600 } });
}

async function commitScrubSlice(): Promise<void> {
  const value = parseRequiredClamped(
    scrubSliceText.value,
    RANGES.scrubSliceSize,
    settings.settings?.scrub?.sliceSize ?? 500
  );
  scrubSliceText.value = String(value);
  await commitPatch({ scrub: { sliceSize: value } });
}

async function commitScrubDeepSample(): Promise<void> {
  const value = parseRequiredClamped(
    scrubDeepSampleText.value,
    RANGES.scrubDeepSample,
    settings.settings?.scrub?.deepSample ?? 0
  );
  scrubDeepSampleText.value = String(value);
  await commitPatch({ scrub: { deepSample: value } });
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

    <!-- Deep-verify interval (moved out of the old Performance card) -->
    <section class="space-y-3" :class="cardCls">
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
    </section>

    <!-- Advanced: small-file bundling (issue #35) -->
    <section class="space-y-2" :class="cardCls" data-testid="bundling-setting">
      <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
        {{ t("settings.rules.sections.advanced") }}
      </h3>
      <label class="flex items-center gap-2">
        <input
          type="checkbox"
          class="accent-teal-600"
          data-testid="bundle-small-files-toggle"
          :checked="settings.settings.bundleSmallFiles"
          @change="setBundleSmallFiles"
        />
        {{ t("settings.rules.bundleSmallFilesLabel") }}
      </label>
      <p class="text-xs text-zinc-500 dark:text-zinc-400">
        {{ t("settings.rules.bundleSmallFilesNote") }}
      </p>
    </section>

    <!-- Scheduled integrity scrub -->
    <section class="space-y-2" :class="cardCls" data-testid="scrub-setting">
      <h3 class="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
        {{ t("scrub.settings.heading") }}
      </h3>
      <p class="text-xs text-zinc-500 dark:text-zinc-400">
        {{ t("scrub.settings.description") }}
      </p>
      <label class="flex items-center gap-2">
        <input
          type="checkbox"
          class="accent-teal-600"
          data-testid="scrub-enabled-toggle"
          :checked="settings.settings?.scrub?.enabled ?? true"
          @change="setScrubEnabled"
        />
        {{ t("scrub.settings.enabledLabel") }}
      </label>
      <label class="block space-y-1">
        <span class="text-zinc-600 dark:text-zinc-400">{{
          t("scrub.settings.intervalLabel")
        }}</span>
        <input
          v-model="scrubIntervalHoursText"
          type="number"
          min="1"
          max="8760"
          data-testid="scrub-interval"
          class="w-full"
          :class="inputCls"
          @change="commitScrubInterval"
        />
      </label>
      <label class="block space-y-1">
        <span class="text-zinc-600 dark:text-zinc-400">{{ t("scrub.settings.sliceLabel") }}</span>
        <input
          v-model="scrubSliceText"
          type="number"
          min="10"
          max="10000"
          data-testid="scrub-slice"
          class="w-full"
          :class="inputCls"
          @change="commitScrubSlice"
        />
      </label>
      <label class="block space-y-1">
        <span class="text-zinc-600 dark:text-zinc-400">{{
          t("scrub.settings.deepSampleLabel")
        }}</span>
        <input
          v-model="scrubDeepSampleText"
          type="number"
          min="0"
          max="100"
          data-testid="scrub-deep-sample"
          class="w-full"
          :class="inputCls"
          @change="commitScrubDeepSample"
        />
      </label>
      <p class="text-xs text-zinc-500 dark:text-zinc-400">
        {{ t("scrub.settings.deepSampleHelp") }}
      </p>
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
  </div>
</template>
