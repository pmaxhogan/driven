<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import type { UnlistenFn } from "@tauri-apps/api/event";

import { onOauthComplete } from "../ipc/events";
import { useSetupStore } from "../stores/setup";

// Credentials walkthrough (DESIGN s8.5 step 2; SPEC s6.1 BYO OAuth client). The
// user pastes the Client ID + Client Secret from their own GCP OAuth client,
// then signs in via the loopback PKCE flow (SPEC s6.1 step 7). The browser
// round-trips back to the Rust loopback server, which fires `oauth:complete`;
// we listen for it and also offer a manual "check" affordance mirroring the
// "Done - continue" pattern. On completion the parent advances to the source
// step.
//
// i18n: every visible string is a seeded wizard.step2.* / common.* / errors.*
// key (DESIGN s8.7) - no raw English. Per-GCP-console-step copy is intentionally
// not rendered here because those keys are not seeded; the explanatory body and
// the credential form carry the user through the paste + sign-in.

const { t } = useI18n();
const setup = useSetupStore();

const emit = defineEmits<{ (e: "complete"): void }>();

const clientId = ref("");
const clientSecret = ref("");

let unlisten: UnlistenFn | null = null;

// R1-P2-4 (DESIGN s6.1): a PKCE installed-app client legitimately has an EMPTY
// secret, so only a non-empty client ID is required to submit. The secret is
// passed through as-is (possibly empty).
const canSubmit = computed(() => clientId.value.trim().length > 0 && !setup.busy);

/** Human-facing status line for the in-flight OAuth handshake. */
const statusLabel = computed<string | null>(() => {
  const status = setup.oauthStatus;
  if (!status) return null;
  switch (status.kind) {
    case "openingBrowser":
      return t("wizard.step2.openingBrowser");
    case "awaitingCallback":
      return t("wizard.step2.awaitingCallback");
    case "exchangingCode":
      return t("wizard.step2.exchangingCode");
    case "complete":
      return t("wizard.step2.complete");
    case "failed":
      return null;
    default:
      return null;
  }
});

const errorLong = computed<string | null>(() =>
  setup.errorCode ? t(`errors.${setup.errorCode}.long`) : null
);

const awaiting = computed(
  () =>
    setup.oauthStatus?.kind === "awaitingCallback" ||
    setup.oauthStatus?.kind === "openingBrowser" ||
    setup.oauthStatus?.kind === "exchangingCode"
);

async function signIn(): Promise<void> {
  if (!canSubmit.value) return;
  try {
    await setup.connectAccount(clientId.value.trim(), clientSecret.value.trim());
  } catch {
    // Error code is recorded on the store; surfaced via errorLong. Swallow so a
    // rejected promise never escapes the click handler.
  }
}

async function checkComplete(): Promise<void> {
  const done = await setup.checkSigninComplete();
  if (done) emit("complete");
}

onMounted(async () => {
  // Resolve the browser round-trip the moment the Rust loopback server reports
  // it (SPEC s11.7 oauth:complete). One-shot re-check via the shared poll path -
  // no busy loop.
  unlisten = await onOauthComplete(() => {
    void checkComplete();
  });
});

onBeforeUnmount(() => {
  if (unlisten) {
    unlisten();
    unlisten = null;
  }
});
</script>

<template>
  <div class="space-y-4">
    <p class="text-zinc-600 dark:text-zinc-400">
      {{ t("wizard.step2.body") }}
    </p>

    <div class="space-y-3">
      <label class="block space-y-1">
        <span class="text-sm font-medium">{{ t("wizard.step2.clientIdLabel") }}</span>
        <input
          v-model="clientId"
          type="text"
          autocomplete="off"
          spellcheck="false"
          class="w-full rounded border px-3 py-1.5 text-sm"
          :placeholder="t('wizard.step2.clientIdPlaceholder')"
          :disabled="setup.busy || awaiting"
        />
      </label>

      <label class="block space-y-1">
        <span class="text-sm font-medium">{{ t("wizard.step2.clientSecretLabel") }}</span>
        <input
          v-model="clientSecret"
          type="password"
          autocomplete="off"
          spellcheck="false"
          class="w-full rounded border px-3 py-1.5 text-sm"
          :placeholder="t('wizard.step2.clientSecretPlaceholder')"
          :disabled="setup.busy || awaiting"
        />
      </label>
    </div>

    <div class="flex items-center gap-3">
      <button
        type="button"
        class="rounded border px-3 py-1.5 text-sm disabled:opacity-50"
        :disabled="!canSubmit"
        @click="signIn"
      >
        {{ t("wizard.step2.signInButton") }}
      </button>

      <button
        v-if="awaiting"
        type="button"
        class="rounded border px-3 py-1.5 text-sm disabled:opacity-50"
        :disabled="setup.busy"
        @click="checkComplete"
      >
        {{ t("common.confirm") }}
      </button>

      <span v-if="setup.busy" class="text-sm text-zinc-500">{{ t("common.loading") }}</span>
    </div>

    <p v-if="statusLabel" class="text-sm text-zinc-500">
      {{ statusLabel }}
    </p>

    <p v-if="errorLong" class="text-sm text-red-600" role="alert">
      {{ errorLong }}
    </p>
  </div>
</template>
