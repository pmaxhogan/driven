<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import type { CreateSftpAccountRequest, SftpAuthMethodDto } from "../ipc/types";

// The SSH (SFTP) destination's credentials step (the non-OAuth, non-local,
// non-S3 branch of DESIGN s8.5 step 2 - see the "SSH (SFTP) backend" design).
// Collects the server (host/port/root path/username) and ONE of two
// credentials - a plain password, or a pasted private key with an optional
// passphrase - and emits ONE request object for the parent to hand to
// `create_sftp_account`.
//
// The password / private key / passphrase are never echoed back: the backend
// writes them straight to the OS keychain and there is no IPC command that
// reads them out again, so this component is the only place they ever exist
// in the webview (mirrors S3CredentialsForm's secret-access-key doc).
//
// Validation here is presentational only - enough to keep the submit button
// honest. The REAL validation is the backend's: it normalizes the settings and
// then PROVES the credential reaches the server, the root path exists, and a
// real write succeeds - all BEFORE any account row is written - so a typo
// surfaces as a specific error (SPEC s24) rather than an account that silently
// never backs anything up.

const { t } = useI18n();

const props = defineProps<{
  busy?: boolean;
  /** A stable SPEC s24 error code from a failed submit, already localized by
   * the parent (which owns the error vocabulary). */
  errorMessage?: string | null;
}>();

const emit = defineEmits<{ (e: "submit", req: CreateSftpAccountRequest): void }>();

const host = ref("");
// Vue auto-casts a native `type="number"` input's v-model to a `number` once a
// value is typed in, while it starts (and stays, if left blank) a `string`.
const port = ref<string | number>("");
const rootPath = ref("");
const username = ref("");
const authMethod = ref<SftpAuthMethodDto>("password");
const password = ref("");
const privateKey = ref("");
const passphrase = ref("");

// Switching auth mode clears the other mode's fields, so a value typed before
// a toggle can never leak into the submitted request (SPEC s24 / the secret
// discipline above): a password left over from before a switch to key auth
// must not silently ride along, and vice versa.
watch(authMethod, () => {
  password.value = "";
  privateKey.value = "";
  passphrase.value = "";
});

const canSubmit = computed(() => {
  if (props.busy) return false;
  if (host.value.trim().length === 0) return false;
  if (rootPath.value.trim().length === 0) return false;
  if (username.value.trim().length === 0) return false;
  if (authMethod.value === "password") {
    return password.value.length > 0;
  }
  return privateKey.value.trim().length > 0;
});

function submit(): void {
  if (!canSubmit.value) return;
  // Vue auto-casts a native `type="number"` input's v-model to a number, so
  // `port` is a `string` when empty/untouched but a `number` once typed into -
  // coerce through `String()` rather than assuming either.
  const trimmedPort = String(port.value).trim();
  emit("submit", {
    host: host.value.trim(),
    port: trimmedPort.length > 0 ? Number(trimmedPort) : null,
    rootPath: rootPath.value.trim(),
    username: username.value.trim(),
    auth: authMethod.value,
    password: authMethod.value === "password" ? password.value : null,
    privateKey: authMethod.value === "privateKey" ? privateKey.value : null,
    // An empty passphrase is no passphrase, not a wrong one - the backend
    // treats "" the same way, but normalizing here keeps the emitted request
    // honest about what was actually entered.
    passphrase:
      authMethod.value === "privateKey" && passphrase.value.length > 0 ? passphrase.value : null,
  });
}

const FIELD =
  "w-full rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-100";
const LABEL = "block text-sm font-medium text-zinc-700 dark:text-zinc-200";
const HINT = "text-xs text-zinc-500 dark:text-zinc-400";
</script>

<template>
  <form class="space-y-4" data-testid="sftp-credentials-form" @submit.prevent="submit">
    <p class="text-zinc-600 dark:text-zinc-400">
      {{ t("sftpSetup.body") }}
    </p>

    <div class="grid gap-4 sm:grid-cols-3">
      <div class="space-y-1 sm:col-span-2">
        <label :class="LABEL" for="sftp-host">{{ t("sftpSetup.hostLabel") }}</label>
        <input
          id="sftp-host"
          v-model="host"
          :class="FIELD"
          type="text"
          autocomplete="off"
          spellcheck="false"
          :placeholder="t('sftpSetup.hostPlaceholder')"
        />
      </div>
      <div class="space-y-1">
        <label :class="LABEL" for="sftp-port">{{ t("sftpSetup.portLabel") }}</label>
        <input
          id="sftp-port"
          v-model="port"
          :class="FIELD"
          type="number"
          min="1"
          max="65535"
          autocomplete="off"
          :placeholder="t('sftpSetup.portPlaceholder')"
        />
      </div>
    </div>

    <div class="space-y-1">
      <label :class="LABEL" for="sftp-root-path">{{ t("sftpSetup.rootPathLabel") }}</label>
      <input
        id="sftp-root-path"
        v-model="rootPath"
        :class="FIELD"
        type="text"
        autocomplete="off"
        spellcheck="false"
        :placeholder="t('sftpSetup.rootPathPlaceholder')"
      />
      <p :class="HINT">{{ t("sftpSetup.rootPathHint") }}</p>
    </div>

    <div class="space-y-1">
      <label :class="LABEL" for="sftp-username">{{ t("sftpSetup.usernameLabel") }}</label>
      <input
        id="sftp-username"
        v-model="username"
        :class="FIELD"
        type="text"
        autocomplete="off"
        spellcheck="false"
      />
    </div>

    <fieldset class="space-y-2">
      <legend :class="LABEL">{{ t("sftpSetup.authMethodLabel") }}</legend>
      <div class="flex gap-4">
        <label class="flex items-center gap-2 text-sm text-zinc-700 dark:text-zinc-200">
          <input
            type="radio"
            name="sftp-auth-method"
            value="password"
            data-testid="sftp-auth-password"
            :checked="authMethod === 'password'"
            @change="authMethod = 'password'"
          />
          {{ t("sftpSetup.authMethodPassword") }}
        </label>
        <label class="flex items-center gap-2 text-sm text-zinc-700 dark:text-zinc-200">
          <input
            type="radio"
            name="sftp-auth-method"
            value="privateKey"
            data-testid="sftp-auth-private-key"
            :checked="authMethod === 'privateKey'"
            @change="authMethod = 'privateKey'"
          />
          {{ t("sftpSetup.authMethodPrivateKey") }}
        </label>
      </div>
    </fieldset>

    <div v-if="authMethod === 'password'" class="space-y-1">
      <label :class="LABEL" for="sftp-password">{{ t("sftpSetup.passwordLabel") }}</label>
      <input
        id="sftp-password"
        v-model="password"
        :class="FIELD"
        type="password"
        autocomplete="off"
        spellcheck="false"
        data-testid="sftp-password"
      />
    </div>

    <template v-else>
      <div class="space-y-1">
        <label :class="LABEL" for="sftp-private-key">{{ t("sftpSetup.privateKeyLabel") }}</label>
        <textarea
          id="sftp-private-key"
          v-model="privateKey"
          :class="FIELD"
          rows="6"
          autocomplete="off"
          spellcheck="false"
          :placeholder="t('sftpSetup.privateKeyPlaceholder')"
        />
        <p :class="HINT">{{ t("sftpSetup.privateKeyHint") }}</p>
      </div>
      <div class="space-y-1">
        <label :class="LABEL" for="sftp-passphrase">{{ t("sftpSetup.passphraseLabel") }}</label>
        <input
          id="sftp-passphrase"
          v-model="passphrase"
          :class="FIELD"
          type="password"
          autocomplete="off"
          spellcheck="false"
          data-testid="sftp-passphrase"
        />
        <p :class="HINT">{{ t("sftpSetup.passphraseHint") }}</p>
      </div>
    </template>

    <p class="text-xs text-amber-700 dark:text-amber-400" data-testid="sftp-trash-warning">
      {{ t("sftpSetup.trashWarning") }}
    </p>

    <p v-if="errorMessage" class="text-sm text-red-600 dark:text-red-400" role="alert">
      {{ errorMessage }}
    </p>

    <button
      type="submit"
      class="inline-flex items-center justify-center gap-2 rounded-md bg-teal-700 px-4 py-2 text-sm font-medium text-white shadow-xs transition-colors hover:bg-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50"
      :disabled="!canSubmit"
      data-testid="sftp-connect"
    >
      {{ busy ? t("sftpSetup.connecting") : t("sftpSetup.connectButton") }}
    </button>
  </form>
</template>
