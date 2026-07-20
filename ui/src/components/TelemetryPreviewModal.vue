<script setup lang="ts">
import { ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import * as ipc from "../ipc/commands";

// TelemetryPreviewModal (SPEC s16 preview; user privacy request via #34). Shows
// the EXACT JSON payload the next telemetry ping would send, fetched via
// `preview_telemetry_ping` - a read-only backend call that never touches the
// network, never advances the delta checkpoint, and never resets the latency
// reservoir (see telemetry.rs `resolve_payload`'s doc comment). Available even
// when telemetry is currently OFF - the whole point is letting a
// privacy-conscious user inspect the payload BEFORE opting in.
//
// Follows the ChangelogModal pattern: `open` is a plain boolean prop (the
// parent owns visibility), an overlay click or the close button emits `close`.
const { t } = useI18n();

const props = defineProps<{ open: boolean }>();
const emit = defineEmits<{ close: [] }>();

const loading = ref(false);
const errorMessage = ref<string | null>(null);
const payload = ref<unknown>(null);
const copied = ref(false);

const SECONDARY_BTN =
  "inline-flex items-center justify-center gap-2 rounded-md border border-zinc-300 bg-white px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800";

async function load(): Promise<void> {
  loading.value = true;
  errorMessage.value = null;
  payload.value = null;
  copied.value = false;
  try {
    payload.value = await ipc.previewTelemetryPing();
  } catch (e) {
    errorMessage.value = String(e);
  } finally {
    loading.value = false;
  }
}

// Fetch a fresh payload each time the modal opens - the aggregate window is
// time-sensitive, so a stale open-then-reopen should not show yesterday's
// counts.
watch(
  () => props.open,
  (isOpen) => {
    if (isOpen) void load();
  },
  { immediate: true }
);

const prettyPayload = () => (payload.value === null ? "" : JSON.stringify(payload.value, null, 2));

async function copy(): Promise<void> {
  const text = prettyPayload();
  if (!text) return;
  try {
    if (
      typeof navigator !== "undefined" &&
      navigator.clipboard &&
      typeof navigator.clipboard.writeText === "function"
    ) {
      await navigator.clipboard.writeText(text);
      copied.value = true;
    }
  } catch {
    copied.value = false;
  }
}

function close(): void {
  emit("close");
}
</script>

<template>
  <div
    v-if="open"
    class="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
    role="dialog"
    aria-modal="true"
    data-testid="telemetry-preview-modal"
    @click.self="close"
  >
    <div
      class="max-h-[80vh] w-full max-w-lg overflow-y-auto rounded-lg border border-zinc-200 bg-white p-6 shadow-xl dark:border-zinc-800 dark:bg-zinc-900"
    >
      <div class="mb-4 flex items-start justify-between gap-4">
        <h2 class="text-lg font-semibold">
          {{ t("telemetryPreview.title") }}
        </h2>
        <button
          type="button"
          class="rounded-md px-2 py-1 text-sm text-zinc-500 transition-colors hover:text-teal-700 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:hover:text-teal-300"
          :aria-label="t('common.close')"
          @click="close"
        >
          {{ t("common.close") }}
        </button>
      </div>

      <p class="mb-3 text-xs text-zinc-500 dark:text-zinc-400" data-testid="telemetry-preview-caption">
        {{ t("telemetryPreview.caption") }}
      </p>

      <p v-if="loading" class="text-sm text-zinc-500" data-testid="telemetry-preview-loading">
        {{ t("common.loading") }}
      </p>
      <p
        v-else-if="errorMessage"
        class="text-sm text-red-600"
        data-testid="telemetry-preview-error"
      >
        {{ errorMessage }}
      </p>
      <template v-else>
        <pre
          class="max-h-96 overflow-auto rounded-md border border-zinc-200 bg-zinc-50 p-3 text-xs text-zinc-900 dark:border-zinc-800 dark:bg-zinc-950 dark:text-zinc-100"
          data-testid="telemetry-preview-json"
          >{{ prettyPayload() }}</pre
        >
        <div class="mt-3">
          <button type="button" :class="SECONDARY_BTN" @click="copy">
            {{ copied ? t("telemetryPreview.copiedButton") : t("telemetryPreview.copyButton") }}
          </button>
        </div>
      </template>
    </div>
  </div>
</template>
