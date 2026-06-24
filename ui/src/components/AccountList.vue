<script setup lang="ts">
import { onMounted } from "vue";
import { useI18n } from "vue-i18n";

import { useAccountsStore } from "../stores/accounts";

// Accounts settings tab body (SPEC s11.1; DESIGN s8.2). M6 shell: lists the
// connected accounts from the store with add/remove/reconnect affordances. The
// accounts implementer wires the add-account wizard launch + the remove/reauth
// confirmations; the list + store refresh are already live.
const { t } = useI18n();
const accounts = useAccountsStore();

onMounted(() => {
  void accounts.refresh();
});
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
      >
        {{ t("settings.accounts.addButton") }}
      </button>
    </div>

    <p
      v-if="accounts.loading"
      class="text-sm text-zinc-500"
    >
      {{ t("common.loading") }}
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
        class="flex items-center justify-between py-2"
      >
        <div>
          <p class="text-sm font-medium">
            {{ account.email }}
          </p>
          <p class="text-xs text-zinc-500">
            {{ t(`settings.accounts.state.${account.state}`) }}
          </p>
        </div>
        <div class="flex gap-2">
          <button
            v-if="account.state === 'needs_reauth'"
            type="button"
            class="rounded border px-2 py-1 text-xs"
          >
            {{ t("settings.accounts.reauthButton") }}
          </button>
          <button
            type="button"
            class="rounded border px-2 py-1 text-xs"
          >
            {{ t("settings.accounts.removeButton") }}
          </button>
        </div>
      </li>
    </ul>
  </div>
</template>
