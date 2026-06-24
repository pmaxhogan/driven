<script setup lang="ts">
import { useI18n } from "vue-i18n";

// M6 shell: the app is a router host. Each SPEC s25 route renders its own view
// (Settings tabs, SetupWizard, About, and the M7/M8 placeholder Activity /
// Restore). A persistent top nav lets the user move between the primary
// surfaces; the tray menu + deep links navigate the same router.
const { t } = useI18n();

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
