import {
  createRouter,
  createWebHistory,
  type Router,
  type RouteRecordRaw,
  type RouterHistory,
} from "vue-router";

import { listAccounts } from "./ipc/commands";

// SPEC s25 route map. The Settings view is a SHELL (sidebar + <RouterView>,
// SDD 2026-08-02 settings-sidebar-ia): /settings hosts nine child routes -
// accounts, sources, general, schedule-power, performance, platform, network,
// privacy, advanced - plus /settings/about, each rendering its own page
// component with no props. Activity (M7) and Restore (M8) are fully
// implemented views; /restore/:sourceId scopes the browser to one source.
//
// The pre-sidebar flat paths (/accounts, /sources, /rules, /about) are kept as
// REDIRECTS to their nested equivalent so every existing deep link (tray menu,
// StatusBanner gear, bookmarks) keeps resolving. /settings itself redirects to
// /settings/accounts (the default landing page).
//
// The /settings parent record is deliberately UNNAMED: it now has children AND
// a redirect, and naming a parent that has children is a vue-router foot-gun -
// navigating BY that name renders the childless parent with an empty
// <RouterView>. Nothing in this codebase navigates by route name (verified by
// grep across ui/src and src-tauri) - every push/link is path-based - so only
// the child routes carry names, and only for readability in devtools.
const routes: RouteRecordRaw[] = [
  {
    path: "/setup",
    name: "setup",
    component: () => import("./views/SetupWizard.vue"),
  },
  {
    // SPEC s25: "/" redirects to /activity. The first-run guard (below) may then
    // divert a fresh install on to the setup wizard.
    path: "/",
    redirect: "/activity",
  },
  {
    path: "/activity",
    name: "activity",
    component: () => import("./views/Activity.vue"),
  },
  {
    path: "/settings",
    component: () => import("./views/Settings.vue"),
    children: [
      { path: "", redirect: "/settings/accounts" },
      {
        path: "accounts",
        name: "settings-accounts",
        component: () => import("./components/AccountList.vue"),
      },
      {
        path: "sources",
        name: "settings-sources",
        component: () => import("./components/SourceTable.vue"),
      },
      {
        path: "general",
        name: "settings-general",
        component: () => import("./views/settings/GeneralPage.vue"),
      },
      {
        path: "schedule-power",
        name: "settings-schedule-power",
        component: () => import("./views/settings/SchedulePowerPage.vue"),
      },
      {
        path: "performance",
        name: "settings-performance",
        component: () => import("./views/settings/PerformancePage.vue"),
      },
      {
        path: "platform",
        name: "settings-platform",
        component: () => import("./views/settings/PlatformPage.vue"),
      },
      {
        path: "network",
        name: "settings-network",
        component: () => import("./views/settings/NetworkPage.vue"),
      },
      {
        path: "privacy",
        name: "settings-privacy",
        component: () => import("./views/settings/PrivacyPage.vue"),
      },
      {
        path: "advanced",
        name: "settings-advanced",
        component: () => import("./views/settings/AdvancedPage.vue"),
      },
      {
        // About is reached ONLY from the SettingsNav footer (Locked decisions).
        // Task 3 wires it straight to the existing About.vue view - Task 4 will
        // wrap it (and Accounts/Sources) in thin page components.
        path: "about",
        name: "settings-about",
        component: () => import("./views/About.vue"),
      },
    ],
  },
  { path: "/accounts", redirect: "/settings/accounts" },
  { path: "/sources", redirect: "/settings/sources" },
  { path: "/rules", redirect: "/settings/general" },
  { path: "/about", redirect: "/settings/about" },
  {
    path: "/restore",
    name: "restore",
    component: () => import("./views/Restore.vue"),
  },
  {
    path: "/restore/:sourceId",
    name: "restore-scoped",
    component: () => import("./views/Restore.vue"),
    props: true,
  },
];

/**
 * First-run decision (UI-CORE). Given the navigation target path, returns
 * "/setup" when the app has ZERO configured accounts and the user is landing on
 * the default surface; otherwise null (proceed normally). Reuses the same
 * `list_accounts` IPC command AccountList.vue loads accounts through.
 *
 * Only the DEFAULT landing ("/" or its "/activity" redirect target) is diverted:
 * a deep-link to a specific surface (tray menu -> /accounts, /restore, ...) is
 * always honoured, so the user is never trapped. Robust to IPC failure: any
 * error resolves to null so boot never crashes and never blocks - it just falls
 * through to the normal Activity landing.
 */
export async function firstRunTarget(toPath: string): Promise<string | null> {
  if (toPath !== "/" && toPath !== "/activity") return null;
  try {
    const accounts = await listAccounts();
    if (accounts.length === 0) return "/setup";
  } catch {
    // IPC unavailable / backend error: do not block boot or trap the user.
    return null;
  }
  return null;
}

/**
 * Install the one-shot first-run guard on a router. The guard self-removes after
 * the FIRST navigation so it can only ever divert the initial launch - once the
 * user has an account, or navigates anywhere themselves, normal routing resumes
 * and they can never be trapped on /setup.
 */
function installFirstRunGuard(router: Router): void {
  const remove = router.beforeEach(async (to) => {
    remove();
    const target = await firstRunTarget(to.path);
    return target ?? true;
  });
}

/**
 * Build the app router. Exposed as a factory (in addition to the shared
 * `router` singleton) so unit tests can spin up a fresh instance - each with its
 * own one-shot first-run guard - over an in-memory history.
 */
export function createAppRouter(history: RouterHistory = createWebHistory()): Router {
  const router = createRouter({ history, routes });
  installFirstRunGuard(router);
  return router;
}

export const router = createAppRouter();
