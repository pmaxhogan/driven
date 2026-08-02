<script setup lang="ts">
import { computed } from "vue";
import { useRouter } from "vue-router";
import { useI18n } from "vue-i18n";

import AccountList from "../components/AccountList.vue";
import SourceTable from "../components/SourceTable.vue";
// About is a Settings SUBTAB now, so this view embeds it. It still lives under
// views/ (and is still reached at /about) because that path is its route entry
// point - the router just resolves /about to THIS view with `tab: "about"`.
import About from "./About.vue";
import GeneralPage from "./settings/GeneralPage.vue";
import SchedulePowerPage from "./settings/SchedulePowerPage.vue";
import PerformancePage from "./settings/PerformancePage.vue";
import PlatformPage from "./settings/PlatformPage.vue";
import NetworkPage from "./settings/NetworkPage.vue";
import PrivacyPage from "./settings/PrivacyPage.vue";
import AdvancedPage from "./settings/AdvancedPage.vue";

// Settings view (SPEC s25 /accounts, /sources, /rules, /about; DESIGN s8.2). One
// view hosts the four routed tabs; the active tab comes from the route (router
// passes `tab` as a prop). The Accounts + Sources tabs render their components;
// the Rules tab stacks the seven per-section page components (SDD 2026-08-02
// settings-sidebar-ia, task 2 - each page is a self-contained SFC that reads
// the settings store directly, so this shell no longer holds any Rules-tab
// form state itself); the About tab embeds the About surface (version, update
// channel, release notes, diagnostics). About used to be a top-nav item of its
// own - it is a configuration surface, so it belongs in this sub-bar, and
// /about now resolves here rather than to a standalone view.
const props = withDefaults(defineProps<{ tab?: "accounts" | "sources" | "rules" | "about" }>(), {
  tab: "accounts",
});

const { t } = useI18n();
const router = useRouter();

const tabs = [
  { key: "accounts", route: "/accounts", label: "settings.tabs.accounts" },
  { key: "sources", route: "/sources", label: "settings.tabs.sources" },
  { key: "rules", route: "/rules", label: "settings.tabs.rules" },
  { key: "about", route: "/about", label: "settings.tabs.about" },
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

    <nav class="flex gap-1 border-b border-zinc-200 text-sm dark:border-zinc-800">
      <button
        v-for="tabItem in tabs"
        :key="tabItem.key"
        type="button"
        class="-mb-px rounded-t px-3 py-2 transition-colors focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500"
        :class="
          active === tabItem.key
            ? 'border-b-2 border-teal-600 font-medium text-teal-700 dark:text-teal-300'
            : 'text-zinc-600 hover:text-teal-700 dark:text-zinc-400 dark:hover:text-teal-300'
        "
        :aria-current="active === tabItem.key ? 'page' : undefined"
        @click="go(tabItem.route)"
      >
        {{ t(tabItem.label) }}
      </button>
    </nav>

    <AccountList v-if="active === 'accounts'" />
    <SourceTable v-else-if="active === 'sources'" />
    <About v-else-if="active === 'about'" />
    <div v-else class="space-y-4">
      <h2 class="text-lg font-medium">
        {{ t("settings.rules.title") }}
      </h2>

      <GeneralPage />
      <SchedulePowerPage />
      <PerformancePage />
      <PlatformPage />
      <NetworkPage />
      <PrivacyPage />
      <AdvancedPage />
    </div>
  </section>
</template>
