// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount, flushPromises } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";
import { createMemoryHistory } from "vue-router";

// App.vue is the app-lifetime shell (top nav + router host, DESIGN s25/UI-CORE
// IA). It was uncovered by any test (0% - the only mount happens implicitly
// via Cypress/manual QA, not vitest), which is a real gap: the nav's active-
// link logic (`isActive`), the app-boot updater/progress subscribe+hydrate
// wiring (R2-P1-3, issue #46), and the settings-subtab highlighting all live
// here uncalled. The seams are the same ones `updater-store.test.ts` and
// `progress-store.test.ts` already mock: `@tauri-apps/api/core`'s `invoke`
// (list_accounts for the router's first-run guard, get_pending_update_info,
// get_sync_status) and `@tauri-apps/api/event`'s `listen` (the three updater
// events + sync:status_changed). RouterView is stubbed so mounting App never
// pulls in a routed view's own data-fetching - this test is about the SHELL,
// not the pages inside it.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

const unlistenMock = vi.fn();
const listenMock = vi.fn(async () => {
  return vi.fn(() => unlistenMock());
});
vi.mock("@tauri-apps/api/event", () => ({
  listen: () => listenMock(),
}));

import App from "../App.vue";
import GlobalProgressBar from "../components/GlobalProgressBar.vue";
import StatusBanner from "../components/StatusBanner.vue";
import { i18n } from "../i18n";
import { createAppRouter } from "../router";

const ACCOUNT = {
  id: "acct-1",
  email: "user@example.com",
  displayName: null,
  state: "ok",
  encryptionEnabled: false,
  createdAt: 0,
  lastSyncedAt: null,
};

function backend(): void {
  invokeMock.mockImplementation((cmd: string) => {
    if (cmd === "list_accounts") return Promise.resolve([ACCOUNT]);
    if (cmd === "get_pending_update_info") return Promise.resolve(null);
    if (cmd === "get_sync_status") return Promise.resolve({ accounts: [] });
    if (cmd === "get_pause_state") return Promise.resolve(null);
    return Promise.resolve(undefined);
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  listenMock.mockClear();
  unlistenMock.mockClear();
  backend();
});

async function mountAppAt(path: string) {
  const pinia = createPinia();
  setActivePinia(pinia);
  const router = createAppRouter(createMemoryHistory());
  await router.push(path);
  await router.isReady();
  const wrapper = mount(App, {
    global: {
      plugins: [pinia, i18n, router],
      stubs: { RouterView: true },
    },
  });
  await flushPromises();
  return { wrapper, router };
}

describe("App shell", () => {
  it("renders the top nav with the app wordmark and all three primary surfaces", async () => {
    const { wrapper } = await mountAppAt("/activity");
    expect(wrapper.find("nav").exists()).toBe(true);
    const links = wrapper.findAll("nav a");
    // Wordmark + Activity | Restore | Settings. About is no longer a top-nav
    // surface - it is a subtab inside Settings.
    expect(links.length).toBe(4);
    expect(links.map((l) => l.attributes("href"))).toEqual([
      "/activity",
      "/activity",
      "/restore",
      "/settings",
    ]);
  });

  it("orders Restore before Settings in the nav", async () => {
    const { wrapper } = await mountAppAt("/activity");
    const hrefs = wrapper.findAll("nav a").map((l) => l.attributes("href"));
    expect(hrefs.indexOf("/restore")).toBeLessThan(hrefs.indexOf("/settings"));
  });

  it("offers no top-nav About link (it moved into Settings)", async () => {
    const { wrapper } = await mountAppAt("/activity");
    expect(wrapper.find('nav a[href="/about"]').exists()).toBe(false);
  });

  // SDD 2026-08-02 settings-sidebar-ia (task 3): Accounts/Sources/Rules/About
  // are routed pages under /settings now, not tabs - /accounts, /sources,
  // /rules and /about all redirect into a /settings/* child route. The
  // Settings nav item must stay lit wherever that redirect lands.
  it("keeps Settings active on a settings child route (e.g. /settings/accounts)", async () => {
    const { wrapper } = await mountAppAt("/settings/accounts");
    expect(wrapper.find('a[href="/settings"]').attributes("aria-current")).toBe("page");
  });

  it("marks the Activity link active (and no other) when on /activity", async () => {
    const { wrapper } = await mountAppAt("/activity");
    const activityLink = wrapper.find('a[href="/activity"]');
    const settingsLink = wrapper.find('a[href="/settings"]');
    expect(activityLink.attributes("aria-current")).toBe("page");
    expect(settingsLink.attributes("aria-current")).toBeUndefined();
  });

  it("keeps Settings active for an old flat path that redirects into settings (e.g. /accounts)", async () => {
    const { wrapper } = await mountAppAt("/accounts");
    const settingsLink = wrapper.find('a[href="/settings"]');
    expect(settingsLink.attributes("aria-current")).toBe("page");
  });

  // /rules used to be its own tab route; it now redirects to the General page
  // (Locked decisions), and the Settings nav item stays lit for the resolved
  // destination exactly as it does for any other settings child route.
  it("redirects /rules to /settings/general and keeps Settings active", async () => {
    const { wrapper, router } = await mountAppAt("/rules");
    expect(router.currentRoute.value.path).toBe("/settings/general");
    expect(wrapper.find('a[href="/settings"]').attributes("aria-current")).toBe("page");
  });

  it("marks Restore active for a nested route (e.g. /restore/some-source)", async () => {
    const { wrapper } = await mountAppAt("/restore/some-source");
    const restoreLink = wrapper.find('a[href="/restore"]');
    expect(restoreLink.attributes("aria-current")).toBe("page");
  });

  // The shell chrome must not be scrollable out of the way: on a long view (a
  // 10k-row Activity list) the running-backup progress bar used to disappear off
  // the top. Progress bar + paused banner + nav are one sticky block, so all
  // three pin together and the document stays the scroll container (which
  // `useVirtualList` depends on - it windows off window scroll).
  it("pins the progress bar, paused banner and nav in one sticky header", async () => {
    const { wrapper } = await mountAppAt("/activity");
    const header = wrapper.get('[data-testid="app-header"]');

    expect(header.classes()).toContain("sticky");
    expect(header.classes()).toContain("top-0");
    // Above page content and Restore's `sticky bottom-0 z-10` bar, below modals.
    expect(header.classes()).toContain("z-30");
    // All three pieces of chrome live INSIDE it (so none can scroll away alone).
    expect(header.findComponent(GlobalProgressBar).exists()).toBe(true);
    expect(header.findComponent(StatusBanner).exists()).toBe(true);
    expect(header.find("nav").exists()).toBe(true);
  });

  // Sticky, not fixed, and no inner scroll container: the header keeps its space
  // in normal flow, so the shell needs no compensating top padding and there is
  // exactly one scrollbar (the document's).
  it("leaves the document as the only scroll container", async () => {
    const { wrapper } = await mountAppAt("/activity");
    const root = wrapper.get('[data-testid="app-header"]').element.parentElement!;

    expect(root.className).not.toContain("overflow");
    const main = wrapper.get("main");
    expect(main.classes()).not.toContain("overflow-y-auto");
    expect(main.classes().some((c) => c.startsWith("pt-"))).toBe(false);
  });

  it("subscribes + hydrates the updater, progress and pause stores on boot", async () => {
    await mountAppAt("/activity");
    // Three updater events (available, download_progress, downloaded) + the
    // progress store's TWO sync events (status_changed for the phase,
    // source_progress for the moving counters) + the pause event registered +
    // the two ToastHost subscriptions (status_changed for "Backup started",
    // activity:new for the backup_done row behind "Backup complete") + the
    // FdaBanner's own activity:new subscription (the macOS TCC denial row).
    expect(listenMock).toHaveBeenCalledTimes(9);
    expect(invokeMock).toHaveBeenCalledWith("get_pending_update_info", undefined);
    expect(invokeMock).toHaveBeenCalledWith("get_sync_status", undefined);
    expect(invokeMock).toHaveBeenCalledWith("get_pause_state", undefined);
  });

  it("never throws when a subscribe registration fails - hydration still runs", async () => {
    listenMock.mockImplementationOnce(async () => {
      throw new Error("listen failed");
    });
    const { wrapper } = await mountAppAt("/activity");
    // Boot must complete (no unhandled rejection reaching the test) and the
    // shell still renders.
    expect(wrapper.find("nav").exists()).toBe(true);
    expect(invokeMock).toHaveBeenCalledWith("get_pending_update_info", undefined);
  });
});
