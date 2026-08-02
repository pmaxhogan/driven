<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import { getVersion } from "@tauri-apps/api/app";

import { useAccountsStore } from "../stores/accounts";
import { useSettingsStore } from "../stores/settings";
import { useUpdaterStore } from "../stores/updater";
import { ensureSettingsLoaded, inputCls } from "../views/settings/shared";

// Settings sidebar (SDD 2026-08-02 settings-sidebar-ia, task 3; DESIGN Direction
// A, mock a-sidebar-macos.html). Replaces the old four-tab strip: a search box
// filtering by label + keyword, the two "object" pages (Accounts, Sources), a
// "Preferences" group of the seven Rules pages, and a footer that is the ONLY
// entry point to About (Locked decisions). Rendered inside Settings.vue
// alongside <RouterView>, so it is mounted for the whole time the user is
// anywhere under /settings.
type SettingsPageKey =
  | "accounts"
  | "sources"
  | "general"
  | "schedule-power"
  | "performance"
  | "platform"
  | "network"
  | "privacy"
  | "advanced";

interface NavItem {
  key: SettingsPageKey;
  to: string;
  labelKey: string;
}

// Per-page search keywords, seeded from each page's card headings (Task 3
// brief) so the search box finds a control by topic, not just by page title -
// e.g. typing "batt" finds Schedule & Power via its battery-skip card.
const NAV_KEYWORDS: Record<SettingsPageKey, string[]> = {
  accounts: ["account", "connect", "reauth", "reconnect"],
  sources: ["source", "folder", "backup"],
  general: ["scan", "interval", "channel", "update", "language"],
  "schedule-power": ["battery", "metered", "offline", "schedule", "pause", "window"],
  performance: ["bandwidth", "concurrent", "upload", "adaptive", "parallel", "priority"],
  platform: ["launch", "login", "vss", "shadow copy", "apfs", "snapshot", "menu bar"],
  network: ["proxy", "certificate", "root ca"],
  privacy: ["telemetry", "data"],
  advanced: ["scrub", "integrity", "hook", "bundling", "deep verify", "log"],
};

const objectItems: NavItem[] = [
  { key: "accounts", to: "/settings/accounts", labelKey: "settings.nav.accounts" },
  { key: "sources", to: "/settings/sources", labelKey: "settings.nav.sources" },
];

const preferenceItems: NavItem[] = [
  { key: "general", to: "/settings/general", labelKey: "settings.nav.general" },
  {
    key: "schedule-power",
    to: "/settings/schedule-power",
    labelKey: "settings.nav.schedulePower",
  },
  { key: "performance", to: "/settings/performance", labelKey: "settings.nav.performance" },
  // Label is resolved dynamically below (macOS vs Windows); labelKey unused for this item.
  { key: "platform", to: "/settings/platform", labelKey: "settings.nav.platformMacos" },
  { key: "network", to: "/settings/network", labelKey: "settings.nav.network" },
  { key: "privacy", to: "/settings/privacy", labelKey: "settings.nav.privacy" },
  { key: "advanced", to: "/settings/advanced", labelKey: "settings.nav.advanced" },
];

const { t } = useI18n();
const settings = useSettingsStore();
const accounts = useAccountsStore();
const updater = useUpdaterStore();

// This component is mounted for the whole /settings lifetime regardless of
// which child route is active - including the DEFAULT landing page
// (/settings/accounts, which renders AccountList and never loads settings) -
// so it loads its own data rather than relying on a sibling page to have done
// so. Guarded the same way shared.ts's ensureSettingsLoaded is, so mounting
// alongside a page that ALSO calls ensureSettingsLoaded() (every Rules page)
// still issues exactly one get_settings round-trip.
ensureSettingsLoaded();
onMounted(() => {
  if (accounts.accounts.length === 0 && !accounts.loading) {
    void accounts.refresh();
  }
  void updater.loadChannel();
});

const version = ref("");
onMounted(async () => {
  try {
    version.value = await getVersion();
  } catch {
    version.value = "";
  }
});

const search = ref("");

// Locked decisions: the platform page label follows the existing
// `settings.macos` / `settings.windows` nullability, and the item is hidden
// entirely when both are null (Linux).
const platformVisible = computed(
  () => settings.settings?.macos != null || settings.settings?.windows != null
);
const platformLabel = computed(() =>
  settings.settings?.macos != null
    ? t("settings.nav.platformMacos")
    : t("settings.nav.platformWindows")
);

function itemLabel(item: NavItem): string {
  return item.key === "platform" ? platformLabel.value : t(item.labelKey);
}

function matchesSearch(item: NavItem): boolean {
  const query = search.value.trim().toLowerCase();
  if (!query) return true;
  if (itemLabel(item).toLowerCase().includes(query)) return true;
  return NAV_KEYWORDS[item.key].some((keyword) => keyword.includes(query));
}

const visibleObjectItems = computed(() => objectItems.filter(matchesSearch));
const visiblePreferenceItems = computed(() =>
  preferenceItems.filter(
    (item) => (item.key !== "platform" || platformVisible.value) && matchesSearch(item)
  )
);

const reauthCount = computed(() => accounts.needsReauth.length);

const channelLabel = computed(() => t(`about.channel.${updater.channel}`));

// Active item styling uses RouterLink's OWN active-state determination (its
// `v-slot="{ isActive }"`, via `custom` below) rather than a separately
// computed `useRoute().path` check: `useRoute()`/`useRouter()` are the two
// composables several existing Settings tests stub out with a fake push-only
// router (they only ever needed `.push`), and RouterLink's slot resolves the
// current route through vue-router's internal injection instead of calling
// that public composable - so it keeps working under those older mocks
// without requiring every such test file to grow a full router.
const linkBaseCls =
  "block rounded-md px-2 py-1.5 transition-colors focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500";
const linkInactiveCls =
  "text-zinc-600 hover:bg-zinc-100 hover:text-teal-700 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-teal-300";
const linkActiveCls = "bg-teal-50 font-medium text-teal-700 dark:bg-teal-950 dark:text-teal-300";
</script>

<template>
  <nav
    class="w-56 shrink-0 space-y-4 text-sm"
    :aria-label="t('settings.nav.ariaLabel')"
    data-testid="settings-nav"
  >
    <input
      v-model="search"
      type="search"
      :class="inputCls"
      class="w-full"
      :placeholder="t('settings.nav.searchPlaceholder')"
      data-testid="settings-nav-search"
    />

    <ul v-if="visibleObjectItems.length > 0" class="space-y-0.5">
      <li v-for="item in visibleObjectItems" :key="item.key">
        <RouterLink v-slot="{ href, navigate, isActive: active }" :to="item.to" custom>
          <a
            :href="href"
            :class="[linkBaseCls, active ? linkActiveCls : linkInactiveCls]"
            class="flex items-center justify-between gap-2"
            :aria-current="active ? 'page' : undefined"
            :data-testid="`settings-nav-item-${item.key}`"
            @click="navigate"
          >
            <span>{{ itemLabel(item) }}</span>
            <span
              v-if="item.key === 'accounts' && reauthCount > 0"
              class="inline-flex min-w-[1.25rem] items-center justify-center rounded-full bg-amber-500 px-1.5 py-0.5 text-xs font-semibold text-white"
              data-testid="settings-nav-reauth-badge"
            >
              {{ reauthCount }}
            </span>
          </a>
        </RouterLink>
      </li>
    </ul>

    <div v-if="visiblePreferenceItems.length > 0">
      <p class="px-2 text-xs font-semibold uppercase tracking-wide text-zinc-400">
        {{ t("settings.nav.preferencesGroup") }}
      </p>
      <ul class="space-y-0.5">
        <li v-for="item in visiblePreferenceItems" :key="item.key">
          <RouterLink v-slot="{ href, navigate, isActive: active }" :to="item.to" custom>
            <a
              :href="href"
              :class="[linkBaseCls, active ? linkActiveCls : linkInactiveCls]"
              :aria-current="active ? 'page' : undefined"
              :data-testid="`settings-nav-item-${item.key}`"
              @click="navigate"
            >
              {{ itemLabel(item) }}
            </a>
          </RouterLink>
        </li>
      </ul>
    </div>

    <div class="border-t border-zinc-200 pt-3 dark:border-zinc-800">
      <RouterLink
        to="/settings/about"
        class="block rounded-md px-2 py-1 text-xs text-zinc-500 transition-colors hover:text-teal-700 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:text-zinc-400 dark:hover:text-teal-300"
        data-testid="settings-nav-about"
      >
        {{ t("settings.nav.footer", { version, channel: channelLabel }) }}
      </RouterLink>
    </div>
  </nav>
</template>
