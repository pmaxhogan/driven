<script setup lang="ts">
import { onMounted } from "vue";
import { useRoute } from "vue-router";
import { useI18n } from "vue-i18n";

import { useUpdaterStore } from "./stores/updater";

// M6 shell: the app is a router host. Each SPEC s25 route renders its own view
// (Settings tabs, SetupWizard, About, and the M7/M8 Activity / Restore). A
// persistent top nav lets the user move between the primary surfaces; the tray
// menu + deep links navigate the same router.
//
// UI-CORE IA fix: the top nav is the SHELL-level information architecture and
// lists only the four primary surfaces - Activity | Settings | Restore | About.
// Accounts / Sources / Rules are NOT top-nav items: they are subtabs INSIDE the
// Settings page (the only place they live), so the "Settings" item lights up for
// any of /settings, /accounts, /sources, /rules. Teal is the shell accent (brand
// wordmark + active/hover link states).
const { t } = useI18n();
const route = useRoute();

// R2-P1-3: own the updater event subscriptions at the APP ROOT so they are
// ALWAYS live - the backend's STARTUP update check emits `updater:available`
// early, and if the only listener lived in About.vue (mounted on demand) that
// event would be lost and no banner would ever appear. App is the app-lifetime
// root component, so subscribing here (and never tearing it down) guarantees the
// store - which drives the banner everywhere - captures the event regardless of
// which route is open. We also HYDRATE the store from the recorded pending
// update so an emit that fired before the webview attached is still reflected.
const updater = useUpdaterStore();

// R4-P2-1: subscribe() can reject on a partial listener-registration failure (it
// now cleans up + resets state so a later retry can re-subscribe). A failed
// subscribe must NOT skip pending-update hydration: the backend's startup check
// may have ALREADY recorded a pending update, and get_pending_update_info is an
// independent path that still surfaces the banner even with no live listeners. So
// run hydration in a `finally` and swallow the subscribe error here (the store
// already records it via checkErrorCode; we only log so boot never throws).
onMounted(async () => {
  try {
    await updater.subscribe();
  } catch (e) {
    console.error("updater subscribe failed at app boot", e);
  } finally {
    await updater.hydratePending();
  }
});

// The top-nav surfaces. `match` is the set of route paths for which the item is
// the ACTIVE one; Settings owns its three subtab routes so it stays lit while the
// user is on any of them. A path is active when it equals a match path or is
// nested under it (so /restore/:sourceId keeps Restore active).
const navLinks = [
  { to: "/activity", label: "nav.activity", match: ["/activity"] },
  {
    to: "/settings",
    label: "nav.settings",
    match: ["/settings", "/accounts", "/sources", "/rules"],
  },
  { to: "/restore", label: "nav.restore", match: ["/restore"] },
  { to: "/about", label: "nav.about", match: ["/about"] },
] as const;

function isActive(matches: readonly string[]): boolean {
  return matches.some((path) => route.path === path || route.path.startsWith(`${path}/`));
}

// Shared NAV LINK class strings (design system). Identical strings across slices
// keep the shell visually consistent; only active/inactive color + weight differ.
const NAV_LINK_BASE =
  "rounded px-1 py-0.5 transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500";
const NAV_LINK_INACTIVE =
  "text-zinc-600 hover:text-teal-700 dark:text-zinc-400 dark:hover:text-teal-300";
const NAV_LINK_ACTIVE = "text-teal-700 dark:text-teal-300 font-semibold";
</script>

<template>
  <div class="min-h-screen flex flex-col">
    <nav
      class="flex flex-wrap items-center gap-x-6 gap-y-2 border-b border-zinc-200 bg-white px-6 py-3 text-sm dark:border-zinc-800 dark:bg-zinc-900"
      :aria-label="t('nav.primary')"
    >
      <RouterLink
        to="/activity"
        class="mr-2 text-base font-bold tracking-tight text-teal-700 transition-colors hover:text-teal-600 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:text-teal-300 dark:hover:text-teal-200"
      >
        {{ t("app.name") }}
      </RouterLink>
      <RouterLink
        v-for="link in navLinks"
        :key="link.to"
        :to="link.to"
        :class="[NAV_LINK_BASE, isActive(link.match) ? NAV_LINK_ACTIVE : NAV_LINK_INACTIVE]"
        :aria-current="isActive(link.match) ? 'page' : undefined"
      >
        {{ t(link.label) }}
      </RouterLink>
    </nav>
    <main class="flex-1 p-6">
      <RouterView />
    </main>
  </div>
</template>
