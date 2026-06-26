import {
  createRouter,
  createWebHistory,
  type Router,
  type RouteRecordRaw,
  type RouterHistory,
} from "vue-router";

import { listAccounts } from "./ipc/commands";

// SPEC s25 route map. The Settings view hosts the Accounts / Sources / Rules
// tabs (DESIGN s8.2); each tab has its own path so the tray menu + deep links
// can target a specific tab directly. Activity (M7) and Restore (M8) are now
// fully implemented views; /restore/:sourceId scopes the browser to one source.
//
// UI-CORE IA fix: /settings is the top-nav entry point to the Settings page; it
// renders the same view defaulting to the Accounts tab. The per-tab /accounts,
// /sources and /rules paths are KEPT so tray deep-links and the in-page subtabs
// continue to navigate directly to a specific tab.
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
    name: "settings",
    component: () => import("./views/Settings.vue"),
    props: { tab: "accounts" },
  },
  {
    path: "/accounts",
    name: "accounts",
    component: () => import("./views/Settings.vue"),
    props: { tab: "accounts" },
  },
  {
    path: "/sources",
    name: "sources",
    component: () => import("./views/Settings.vue"),
    props: { tab: "sources" },
  },
  {
    path: "/rules",
    name: "rules",
    component: () => import("./views/Settings.vue"),
    props: { tab: "rules" },
  },
  {
    path: "/about",
    name: "about",
    component: () => import("./views/About.vue"),
  },
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
