<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import type { UnlistenFn } from "@tauri-apps/api/event";

import { onOauthComplete } from "../ipc/events";
import { useSetupStore } from "../stores/setup";

// Credentials walkthrough (DESIGN s8.5 step 2; SPEC s6.1 BYO OAuth client). The
// user pastes the Client ID + Client Secret from their own GCP OAuth client,
// then signs in via the loopback PKCE flow (SPEC s6.1 step 7). The sign-in opens
// the system browser via the setup store (Tauri opener plugin); the browser
// round-trips back to the Rust loopback server, which fires `oauth:complete`. We
// listen for it and also offer a manual "check" affordance mirroring the
// "Done - continue" pattern. On completion the parent advances to the source
// step.
//
// First-run help: a non-technical user has likely never made a Google OAuth
// client, so this step renders a collapsible, numbered walkthrough with the exact
// current Google Cloud Console button names (the console UI was reorganized under
// "Google Auth Platform" in 2025). All copy flows through seeded wizard.step2.*
// keys.
//
// Robust open: if the automatic system-browser open is blocked, the store still
// captures the consent URL and moves to `awaitingCallback`, so this step shows a
// manual "open / copy this link" fallback - a failed auto-open is never a dead
// end (the loopback server keeps listening).
//
// i18n: every visible string is a seeded wizard.step2.* / common.* / errors.*
// key (DESIGN s8.7) - no raw English.

const { t } = useI18n();
const setup = useSetupStore();

const emit = defineEmits<{ (e: "complete"): void }>();

const clientId = ref("");
const clientSecret = ref("");
const linkCopied = ref(false);

let unlisten: UnlistenFn | null = null;

// Design-system class strings (shared verbatim across slices for consistency).
const PRIMARY_BTN =
  "inline-flex items-center justify-center gap-2 rounded-md bg-teal-700 px-4 py-2 text-sm font-medium text-white shadow-sm transition-colors hover:bg-teal-600 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50";
const SECONDARY_BTN =
  "inline-flex items-center justify-center gap-2 rounded-md border border-zinc-300 bg-white px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800";
const TEXT_INPUT =
  "rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 transition-colors focus:border-teal-500 focus:outline-none focus:ring-2 focus:ring-teal-500/40 disabled:opacity-60 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100";
const CARD =
  "rounded-lg border border-zinc-200 bg-white p-4 shadow-sm dark:border-zinc-800 dark:bg-zinc-900";

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

// The manual open/copy fallback is shown once a consent URL exists and we are
// waiting for the browser round-trip, so a blocked auto-open is recoverable.
const showManualOpen = computed(() => awaiting.value && !!setup.authUrl);

async function signIn(): Promise<void> {
  if (!canSubmit.value) return;
  linkCopied.value = false;
  try {
    await setup.connectAccount(clientId.value.trim(), clientSecret.value.trim());
  } catch {
    // Error code is recorded on the store; surfaced via errorLong. Swallow so a
    // rejected promise never escapes the click handler.
  }
}

/** Manual fallback: re-open the captured consent URL in the system browser. */
async function openSignInPage(): Promise<void> {
  linkCopied.value = false;
  await setup.openAuthUrl();
}

/** Select the whole link when the read-only field is focused, for easy copy. */
function selectAll(event: FocusEvent): void {
  (event.target as HTMLInputElement | null)?.select();
}

/** Manual fallback: copy the consent URL so the user can paste it themselves. */
async function copyLink(): Promise<void> {
  const url = setup.authUrl;
  if (!url) return;
  try {
    if (
      typeof navigator !== "undefined" &&
      navigator.clipboard &&
      typeof navigator.clipboard.writeText === "function"
    ) {
      await navigator.clipboard.writeText(url);
      linkCopied.value = true;
    }
  } catch {
    linkCopied.value = false;
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

    <!-- First-run help: a collapsible, numbered guide to creating a Google OAuth
         client. Native <details> keeps it accessible + skimmable with no extra
         state. Exact current Google Cloud Console labels are quoted in the copy. -->
    <details :class="CARD" class="group">
      <summary
        class="cursor-pointer list-none text-sm font-medium text-teal-700 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:text-teal-300"
      >
        {{ t("wizard.step2.guide.summary") }}
      </summary>
      <div class="mt-3 space-y-3 text-sm text-zinc-600 dark:text-zinc-300">
        <p>{{ t("wizard.step2.guide.intro") }}</p>
        <ol class="list-decimal space-y-2 pl-5">
          <li>{{ t("wizard.step2.guide.step1") }}</li>
          <li>{{ t("wizard.step2.guide.step2") }}</li>
          <li>{{ t("wizard.step2.guide.step3") }}</li>
          <li>{{ t("wizard.step2.guide.step4") }}</li>
          <li>{{ t("wizard.step2.guide.step5") }}</li>
          <li>{{ t("wizard.step2.guide.step6") }}</li>
          <li>{{ t("wizard.step2.guide.step7") }}</li>
        </ol>
        <p
          class="rounded-md bg-amber-50 px-3 py-2 text-amber-800 dark:bg-amber-950/40 dark:text-amber-300"
        >
          {{ t("wizard.step2.guide.testingCaveat") }}
        </p>
      </div>
    </details>

    <div class="space-y-3">
      <label class="block space-y-1">
        <span class="text-sm font-medium">{{ t("wizard.step2.clientIdLabel") }}</span>
        <input
          v-model="clientId"
          type="text"
          autocomplete="off"
          spellcheck="false"
          :class="TEXT_INPUT"
          class="w-full"
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
          :class="TEXT_INPUT"
          class="w-full"
          :placeholder="t('wizard.step2.clientSecretPlaceholder')"
          :disabled="setup.busy || awaiting"
        />
      </label>
    </div>

    <div class="flex flex-wrap items-center gap-3">
      <button type="button" :class="PRIMARY_BTN" :disabled="!canSubmit || awaiting" @click="signIn">
        {{ t("wizard.step2.signInButton") }}
      </button>

      <button
        v-if="awaiting"
        type="button"
        :class="SECONDARY_BTN"
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

    <!-- Manual fallback: if the browser did not open, the user can re-open it or
         copy the link. The loopback server keeps listening either way. -->
    <div v-if="showManualOpen" :class="CARD" class="space-y-3">
      <p class="text-sm text-zinc-600 dark:text-zinc-300">
        {{ t("wizard.step2.manualOpen.prompt") }}
      </p>
      <div class="flex flex-wrap items-center gap-3">
        <button type="button" :class="SECONDARY_BTN" @click="openSignInPage">
          {{ t("wizard.step2.manualOpen.openButton") }}
        </button>
        <button type="button" :class="SECONDARY_BTN" @click="copyLink">
          {{
            linkCopied
              ? t("wizard.step2.manualOpen.copied")
              : t("wizard.step2.manualOpen.copyButton")
          }}
        </button>
      </div>
      <input
        :value="setup.authUrl"
        type="text"
        readonly
        :class="TEXT_INPUT"
        class="w-full"
        :aria-label="t('wizard.step2.manualOpen.linkLabel')"
        @focus="selectAll"
      />
    </div>

    <p v-if="errorLong" class="text-sm text-red-600" role="alert">
      {{ errorLong }}
    </p>
  </div>
</template>
