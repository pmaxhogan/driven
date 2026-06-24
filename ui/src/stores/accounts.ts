import { defineStore } from "pinia";
import { computed, ref } from "vue";

import * as ipc from "../ipc/commands";
import type { AccountDto } from "../ipc/types";

// Accounts store (SPEC s11.1; DESIGN s8.2 Accounts tab). Holds the connected
// accounts list + loading/error flags and the full CRUD over the typed IPC
// wrappers. The add-account flow itself lives in the setup-wizard store; this
// store owns the list view (refresh / remove / reauth) and exposes a helper the
// AccountList banner uses to flip an account to needs_reauth when the backend
// emits `account:needs_reauth` (so the banner reacts without a round-trip).
export const useAccountsStore = defineStore("accounts", () => {
  const accounts = ref<AccountDto[]>([]);
  const loading = ref(false);
  const error = ref<string | null>(null);

  /** Accounts the backend has flagged as needing re-authentication. */
  const needsReauth = computed(() =>
    accounts.value.filter((a) => a.state === "needs_reauth"),
  );

  async function refresh(): Promise<void> {
    loading.value = true;
    error.value = null;
    try {
      accounts.value = await ipc.listAccounts();
    } catch (e) {
      error.value = String(e);
    } finally {
      loading.value = false;
    }
  }

  async function remove(
    accountId: string,
    deleteRemote: boolean,
  ): Promise<void> {
    await ipc.removeAccount(accountId, deleteRemote);
    await refresh();
  }

  async function reauth(accountId: string): Promise<string> {
    const { authUrl } = await ipc.reauthAccount(accountId);
    return authUrl;
  }

  /**
   * Locally mark an account as needing re-auth in response to the
   * `account:needs_reauth` event, without a server round-trip. Idempotent: a
   * no-op if the account is unknown or already flagged.
   */
  function markNeedsReauth(accountId: string): void {
    const account = accounts.value.find((a) => a.id === accountId);
    if (account && account.state !== "needs_reauth") {
      account.state = "needs_reauth";
    }
  }

  return {
    accounts,
    loading,
    error,
    needsReauth,
    refresh,
    remove,
    reauth,
    markNeedsReauth,
  };
});
