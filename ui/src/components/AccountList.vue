<script setup lang="ts">
import { onMounted, onUnmounted, ref } from "vue";
import { useRouter } from "vue-router";
import { useI18n } from "vue-i18n";
import type { UnlistenFn } from "@tauri-apps/api/event";

import { onAccountNeedsReauth, onOauthComplete } from "../ipc/events";
import { useAccountsStore } from "../stores/accounts";

// Accounts settings tab body (SPEC s11.1; DESIGN s8.2). Lists the connected
// Google accounts with add (-> launches the setup wizard at /setup), remove
// (with an "also delete from Drive" opt-in confirmation), and reconnect
// (re-auth) affordances. A banner appears whenever any account needs re-auth;
// it reacts live to the backend `account:needs_reauth` event as well as to the
// state loaded from list_accounts.
const { t, locale } = useI18n();
const accounts = useAccountsStore();
const router = useRouter();

// Per-row remove confirmation state: the id of the account whose remove is
// being confirmed, plus the "delete remote too" choice for that row.
const confirmingRemoveId = ref<string | null>(null);
const deleteRemote = ref(false);
const reauthError = ref<string | null>(null);
// A3: the in-flight re-consent session id (set when the user clicks Reconnect).
// Completed via the `oauth:complete` event so re-consent lands on the EXISTING
// account without creating a duplicate.
const reauthSessionId = ref<string | null>(null);

let unlisten: UnlistenFn | null = null;
let unlistenOauth: UnlistenFn | null = null;

onMounted(async () => {
  await accounts.refresh();
  unlisten = await onAccountNeedsReauth((payload) => {
    accounts.markNeedsReauth(payload.account_id);
  });
  // A3: when the loopback server reports the re-consent finished, complete the
  // session onto the existing account.
  unlistenOauth = await onOauthComplete(() => {
    void finishReauth();
  });
});

onUnmounted(() => {
  if (unlisten) {
    unlisten();
    unlisten = null;
  }
  if (unlistenOauth) {
    unlistenOauth();
    unlistenOauth = null;
  }
});

async function finishReauth(): Promise<void> {
  const sessionId = reauthSessionId.value;
  if (!sessionId) return;
  try {
    const done = await accounts.completeReauth(sessionId);
    if (done) {
      reauthSessionId.value = null;
    }
  } catch (e) {
    reauthError.value = String(e);
  }
}

function addAccount(): void {
  void router.push("/setup");
}

function beginRemove(accountId: string): void {
  confirmingRemoveId.value = accountId;
  deleteRemote.value = false;
}

function cancelRemove(): void {
  confirmingRemoveId.value = null;
  deleteRemote.value = false;
}

async function confirmRemove(accountId: string): Promise<void> {
  await accounts.remove(accountId, deleteRemote.value);
  confirmingRemoveId.value = null;
  deleteRemote.value = false;
}

async function reconnect(accountId: string): Promise<void> {
  reauthError.value = null;
  try {
    // A3: the backend returns BOTH the consent URL and the session id; the
    // FRONTEND opens the URL (A4: single owner), and the session id is held so
    // `oauth:complete` can finish re-consent onto the existing account.
    const { sessionId, authUrl } = await accounts.reauth(accountId);
    reauthSessionId.value = sessionId;
    if (typeof window !== "undefined" && typeof window.open === "function") {
      window.open(authUrl, "_blank");
    }
  } catch (e) {
    reauthError.value = String(e);
  }
}

function formatLastSynced(ms: number | null): string {
  if (ms === null) return t("settings.accounts.neverSynced");
  const formatted = new Intl.DateTimeFormat(locale.value, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(ms));
  return t("settings.accounts.lastSynced", { when: formatted });
}
</script>

<template>
  <div class="space-y-3">
    <div class="flex items-center justify-between">
      <h2 class="text-lg font-medium">
        {{ t("settings.accounts.title") }}
      </h2>
      <button
        type="button"
        class="rounded border px-3 py-1.5 text-sm"
        @click="addAccount"
      >
        {{ t("settings.accounts.addButton") }}
      </button>
    </div>

    <div
      v-if="accounts.needsReauth.length > 0"
      class="rounded border border-amber-400 bg-amber-50 p-3 text-sm dark:bg-amber-950/40"
      data-testid="reauth-banner"
    >
      {{ t("errors.auth.invalid_grant.long") }}
    </div>

    <p
      v-if="reauthError"
      class="text-sm text-red-600"
    >
      {{ reauthError }}
    </p>

    <p
      v-if="accounts.loading"
      class="text-sm text-zinc-500"
    >
      {{ t("common.loading") }}
    </p>
    <p
      v-else-if="accounts.error"
      class="text-sm text-red-600"
    >
      {{ accounts.error }}
    </p>
    <p
      v-else-if="accounts.accounts.length === 0"
      class="text-sm text-zinc-500"
    >
      {{ t("settings.accounts.empty") }}
    </p>
    <ul
      v-else
      class="divide-y"
    >
      <li
        v-for="account in accounts.accounts"
        :key="account.id"
        class="space-y-2 py-2"
      >
        <div class="flex items-center justify-between">
          <div>
            <p class="text-sm font-medium">
              {{ account.email }}
            </p>
            <p class="text-xs text-zinc-500">
              {{ t(`settings.accounts.state.${account.state}`) }}
            </p>
            <p class="text-xs text-zinc-400">
              {{ formatLastSynced(account.lastSyncedAt) }}
            </p>
          </div>
          <div class="flex gap-2">
            <button
              v-if="account.state === 'needs_reauth'"
              type="button"
              class="rounded border px-2 py-1 text-xs"
              @click="reconnect(account.id)"
            >
              {{ t("settings.accounts.reauthButton") }}
            </button>
            <button
              type="button"
              class="rounded border px-2 py-1 text-xs"
              @click="beginRemove(account.id)"
            >
              {{ t("settings.accounts.removeButton") }}
            </button>
          </div>
        </div>

        <div
          v-if="confirmingRemoveId === account.id"
          class="space-y-2 rounded border border-red-300 bg-red-50 p-3 text-sm dark:bg-red-950/30"
          data-testid="remove-confirm"
        >
          <label class="flex items-center gap-2">
            <input
              v-model="deleteRemote"
              type="checkbox"
            >
            {{ t("settings.accounts.deleteRemoteLabel") }}
          </label>
          <div class="flex gap-2">
            <button
              type="button"
              class="rounded border border-red-400 px-2 py-1 text-xs text-red-700"
              @click="confirmRemove(account.id)"
            >
              {{ t("settings.accounts.removeButton") }}
            </button>
            <button
              type="button"
              class="rounded border px-2 py-1 text-xs"
              @click="cancelRemove"
            >
              {{ t("common.cancel") }}
            </button>
          </div>
        </div>
      </li>
    </ul>
  </div>
</template>
