import { defineStore } from "pinia";
import { ref } from "vue";

import * as ipc from "../ipc/commands";
import type { AccountDto } from "../ipc/types";

// Accounts store (SPEC s11.1; DESIGN s8.2 Accounts tab). Holds the connected
// accounts list + loading/error flags. M6 scaffold: the action SIGNATURES are
// frozen and call the typed IPC wrappers; the accounts implementer enriches the
// flows (optimistic updates, the wizard state machine) as needed.
export const useAccountsStore = defineStore("accounts", () => {
  const accounts = ref<AccountDto[]>([]);
  const loading = ref(false);
  const error = ref<string | null>(null);

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

  async function remove(accountId: string, deleteRemote: boolean): Promise<void> {
    await ipc.removeAccount(accountId, deleteRemote);
    await refresh();
  }

  async function reauth(accountId: string): Promise<string> {
    const { authUrl } = await ipc.reauthAccount(accountId);
    return authUrl;
  }

  return { accounts, loading, error, refresh, remove, reauth };
});
