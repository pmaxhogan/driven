<script setup lang="ts">
import { onMounted } from "vue";
import { useI18n } from "vue-i18n";

import { useUpdaterStore } from "./stores/updater";

// M6 shell: the app is a router host. Each SPEC s25 route renders its own view
// (Settings tabs, SetupWizard, About, and the M7/M8 placeholder Activity /
// Restore). A persistent top nav lets the user move between the primary
// surfaces; the tray menu + deep links navigate the same router.
const { t } = useI18n();

// R2-P1-3: own the updater event subscriptions at the APP ROOT so they are
// ALWAYS live - the backend's STARTUP update check emits `updater:available`
// early, and if the only listener lived in About.vue (mounted on demand) that
// event would be lost and no banner would ever appear. App is the app-lifetime
// root component, so subscribing here (and never tearing it down) guarantees the
// store - which drives the banner everywhere - captures the event regardless of
// which route is open. We also HYDRATE the store from the recorded pending
// update so an emit that fired before the webview attached is still reflected.
const updater = useUpdaterStore();

onMounted(async () => {
  await updater.subscribe();
  await updater.hydratePending();
});

const navLinks = [
  { to: "/activity", label: "nav.activity" },
  { to: "/accounts", label: "nav.accounts" },
  { to: "/sources", label: "nav.sources" },
  { to: "/rules", label: "nav.rules" },
  { to: "/restore", label: "nav.restore" },
  { to: "/about", label: "nav.about" },
] as const;
</script>

<template>
  <div class="min-h-screen flex flex-col">
    <nav class="flex items-center gap-4 border-b px-6 py-3 text-sm">
      <span class="font-semibold">{{ t("app.name") }}</span>
      <RouterLink
        v-for="link in navLinks"
        :key="link.to"
        :to="link.to"
        class="text-zinc-600 hover:text-zinc-900 dark:text-zinc-400 dark:hover:text-zinc-100"
      >
        {{ t(link.label) }}
      </RouterLink>
    </nav>
    <main class="flex-1 p-6">
      <RouterView />
    </main>
  </div>
</template>
