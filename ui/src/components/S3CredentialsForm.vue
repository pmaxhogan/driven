<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";

import type { CreateS3AccountRequest } from "../ipc/types";

// The S3 destination's credentials step (the non-OAuth branch of DESIGN s8.5
// step 2). Collects the endpoint, bucket and access key pair, and emits ONE
// request object for the parent to hand to `create_s3_account`.
//
// The secret access key is a password field and is never echoed back: the
// backend writes it straight to the OS keychain and there is no IPC command
// that reads it out again, so this component is the only place it ever exists
// in the webview.
//
// Validation here is presentational only - enough to keep the submit button
// honest. The REAL validation is the backend's: it normalizes the settings and
// then proves the credentials work by listing the destination before any
// account row is written, so a typo surfaces as a specific error rather than an
// account that silently never backs anything up.

const { t } = useI18n();

const props = defineProps<{
  busy?: boolean;
  /** A stable SPEC s24 error code from a failed submit, already localized by
   * the parent (which owns the error vocabulary). */
  errorMessage?: string | null;
}>();

const emit = defineEmits<{ (e: "submit", req: CreateS3AccountRequest): void }>();

const endpoint = ref("");
const bucket = ref("");
const region = ref("");
const prefix = ref("");
const pathStyle = ref(true);
const accessKeyId = ref("");
const secretAccessKey = ref("");

const canSubmit = computed(
  () =>
    !props.busy &&
    endpoint.value.trim().length > 0 &&
    bucket.value.trim().length > 0 &&
    accessKeyId.value.trim().length > 0 &&
    secretAccessKey.value.length > 0
);

function submit(): void {
  if (!canSubmit.value) return;
  emit("submit", {
    endpoint: endpoint.value.trim(),
    bucket: bucket.value.trim(),
    region: region.value.trim() || null,
    pathStyle: pathStyle.value,
    prefix: prefix.value.trim() || null,
    accessKeyId: accessKeyId.value.trim(),
    secretAccessKey: secretAccessKey.value,
  });
}

const FIELD =
  "w-full rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-100";
const LABEL = "block text-sm font-medium text-zinc-700 dark:text-zinc-200";
const HINT = "text-xs text-zinc-500 dark:text-zinc-400";
</script>

<template>
  <form class="space-y-4" data-testid="s3-credentials-form" @submit.prevent="submit">
    <p class="text-zinc-600 dark:text-zinc-400">
      {{ t("s3Setup.body") }}
    </p>

    <div class="space-y-1">
      <label :class="LABEL" for="s3-endpoint">{{ t("s3Setup.endpointLabel") }}</label>
      <input
        id="s3-endpoint"
        v-model="endpoint"
        :class="FIELD"
        type="url"
        autocomplete="off"
        :placeholder="t('s3Setup.endpointPlaceholder')"
      />
      <p :class="HINT">{{ t("s3Setup.endpointHint") }}</p>
    </div>

    <div class="grid gap-4 sm:grid-cols-2">
      <div class="space-y-1">
        <label :class="LABEL" for="s3-bucket">{{ t("s3Setup.bucketLabel") }}</label>
        <input
          id="s3-bucket"
          v-model="bucket"
          :class="FIELD"
          type="text"
          autocomplete="off"
          :placeholder="t('s3Setup.bucketPlaceholder')"
        />
      </div>
      <div class="space-y-1">
        <label :class="LABEL" for="s3-region">{{ t("s3Setup.regionLabel") }}</label>
        <input
          id="s3-region"
          v-model="region"
          :class="FIELD"
          type="text"
          autocomplete="off"
          :placeholder="t('s3Setup.regionPlaceholder')"
        />
        <p :class="HINT">{{ t("s3Setup.regionHint") }}</p>
      </div>
    </div>

    <div class="space-y-1">
      <label :class="LABEL" for="s3-prefix">{{ t("s3Setup.prefixLabel") }}</label>
      <input
        id="s3-prefix"
        v-model="prefix"
        :class="FIELD"
        type="text"
        autocomplete="off"
        :placeholder="t('s3Setup.prefixPlaceholder')"
      />
      <p :class="HINT">{{ t("s3Setup.prefixHint") }}</p>
    </div>

    <label class="flex items-start gap-2">
      <input v-model="pathStyle" type="checkbox" class="mt-1" data-testid="s3-path-style" />
      <span class="min-w-0">
        <span class="block text-sm text-zinc-800 dark:text-zinc-100">
          {{ t("s3Setup.pathStyleLabel") }}
        </span>
        <span :class="HINT">{{ t("s3Setup.pathStyleHint") }}</span>
      </span>
    </label>

    <div class="grid gap-4 sm:grid-cols-2">
      <div class="space-y-1">
        <label :class="LABEL" for="s3-access-key">{{ t("s3Setup.accessKeyLabel") }}</label>
        <input
          id="s3-access-key"
          v-model="accessKeyId"
          :class="FIELD"
          type="text"
          autocomplete="off"
          spellcheck="false"
        />
      </div>
      <div class="space-y-1">
        <label :class="LABEL" for="s3-secret-key">{{ t("s3Setup.secretKeyLabel") }}</label>
        <input
          id="s3-secret-key"
          v-model="secretAccessKey"
          :class="FIELD"
          type="password"
          autocomplete="off"
          spellcheck="false"
          data-testid="s3-secret-key"
        />
        <p :class="HINT">{{ t("s3Setup.secretKeyHint") }}</p>
      </div>
    </div>

    <p v-if="errorMessage" class="text-sm text-red-600 dark:text-red-400" role="alert">
      {{ errorMessage }}
    </p>

    <button
      type="submit"
      class="inline-flex items-center justify-center gap-2 rounded-md bg-teal-700 px-4 py-2 text-sm font-medium text-white shadow-xs transition-colors hover:bg-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50"
      :disabled="!canSubmit"
      data-testid="s3-connect"
    >
      {{ busy ? t("s3Setup.connecting") : t("s3Setup.connectButton") }}
    </button>
  </form>
</template>
