<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";

import TelemetryPreviewModal from "../../components/TelemetryPreviewModal.vue";
import { useSettingsStore } from "../../stores/settings";
import { cardCls, ensureSettingsLoaded } from "./shared";

// Privacy settings page (SDD 2026-08-02 settings-sidebar-ia, task 2). Moved
// verbatim out of Settings.vue's telemetry card - the single home for
// telemetry now that About.vue's duplicate copy is removed (task 5).
const { t, locale } = useI18n();
const settings = useSettingsStore();

ensureSettingsLoaded();

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

// SPEC s16 telemetry preview (#34): shows the exact next-ping JSON payload in a
// modal, available regardless of the current enabled state - a privacy-
// conscious user inspects it BEFORE opting in.
const showTelemetryPreview = ref(false);

// Issue #309: debug logging mode. Uses the generic `patch` (not a dedicated
// command like telemetry's) - there is no in-flight send to cancel, and the
// backend fully owns computing/clearing `debugLoggingExpiresAtMs` from this
// one boolean, so a plain SettingsPatch round-trip is enough.
async function setDebugLoggingEnabled(event: Event): Promise<void> {
  const checked = (event.target as HTMLInputElement).checked;
  try {
    await settings.patch({ global: { debugLoggingEnabled: checked } });
  } catch {
    // errorCode is set on the store and surfaced as the banner; swallow so
    // the toggle's @change handler never escapes as an unhandled rejection.
  }
}

// DESIGN s8.7: locale-aware, never a hand-rolled English formatter (matches
// Activity.vue's dateTimeFormatter).
const debugLoggingExpiresAt = computed(() => {
  const ms = settings.settings?.global.debugLoggingExpiresAtMs;
  if (!ms) return null;
  return new Intl.DateTimeFormat(locale.value, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(ms));
});
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
      <button
        type="button"
        class="rounded-xs text-xs font-medium text-teal-700 underline transition-colors hover:text-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:text-teal-300"
        data-testid="telemetry-preview-open"
        @click="showTelemetryPreview = true"
      >
        {{ t("settings.rules.telemetryPreviewButton") }}
      </button>
    </section>

    <!-- Issue #309: debug logging mode. The amber warning is ALWAYS shown
         (not just after the toggle is on) so it is read before, not after,
         someone opts in - matching the approved mockup. -->
    <section class="space-y-2" :class="cardCls" data-testid="debug-logging-setting">
      <label class="flex items-center gap-2">
        <input
          type="checkbox"
          class="accent-teal-600"
          data-testid="debug-logging-toggle"
          :checked="settings.settings.global.debugLoggingEnabled"
          @change="setDebugLoggingEnabled"
        />
        <span class="font-medium text-zinc-800 dark:text-zinc-200">
          {{ t("settings.rules.debugLogging.label") }}
        </span>
      </label>
      <p class="text-xs text-zinc-500">
        {{ t("settings.rules.debugLogging.note") }}
      </p>
      <p
        class="rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-xs text-amber-900 dark:border-amber-800 dark:bg-amber-950/40 dark:text-amber-200"
        role="alert"
        data-testid="debug-logging-warning"
      >
        {{ t("settings.rules.debugLogging.warning") }}
      </p>
      <p v-if="settings.settings.global.debugLoggingEnabled" class="text-xs text-zinc-500">
        {{ t("settings.rules.debugLogging.includeInBundle") }}
        <template v-if="debugLoggingExpiresAt">
          {{ t("settings.rules.debugLogging.activeUntil", { time: debugLoggingExpiresAt }) }}
        </template>
      </p>
    </section>
  </div>

  <TelemetryPreviewModal :open="showTelemetryPreview" @close="showTelemetryPreview = false" />
</template>
