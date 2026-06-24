<script setup lang="ts">
import { computed } from "vue";
import { useRouter } from "vue-router";
import { useI18n } from "vue-i18n";

import AccountList from "../components/AccountList.vue";
import SourceTable from "../components/SourceTable.vue";

// Settings view (SPEC s25 /accounts, /sources, /rules; DESIGN s8.2). One view
// hosts the three tabs; the active tab comes from the route (router passes
// `tab` as a prop). M6 shell: the Accounts + Sources tabs render their
// components; the Rules tab is a labelled placeholder the settings implementer
// fills in against the settings store.
const props = withDefaults(
  defineProps<{ tab?: "accounts" | "sources" | "rules" }>(),
  { tab: "accounts" },
);

const { t } = useI18n();
const router = useRouter();

const tabs = [
  { key: "accounts", route: "/accounts", label: "settings.tabs.accounts" },
  { key: "sources", route: "/sources", label: "settings.tabs.sources" },
  { key: "rules", route: "/rules", label: "settings.tabs.rules" },
] as const;

const active = computed(() => props.tab);

function go(route: string): void {
  void router.push(route);
}
</script>

<template>
  <section class="space-y-4">
    <h1 class="text-2xl font-semibold">
      {{ t("settings.title") }}
    </h1>

    <nav class="flex gap-2 border-b text-sm">
      <button
        v-for="tabItem in tabs"
        :key="tabItem.key"
        type="button"
        class="px-3 py-2"
        :class="
          active === tabItem.key
            ? 'border-b-2 border-zinc-900 dark:border-zinc-100 font-medium'
            : 'text-zinc-500'
        "
        @click="go(tabItem.route)"
      >
        {{ t(tabItem.label) }}
      </button>
    </nav>

    <AccountList v-if="active === 'accounts'" />
    <SourceTable v-else-if="active === 'sources'" />
    <div
      v-else
      class="space-y-2"
    >
      <h2 class="text-lg font-medium">
        {{ t("settings.rules.title") }}
      </h2>
    </div>
  </section>
</template>
