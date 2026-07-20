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
  return wrapper;
}

describe("App shell", () => {
  it("renders the top nav with the app wordmark and all four primary surfaces", async () => {
    const wrapper = await mountAppAt("/activity");
    expect(wrapper.find("nav").exists()).toBe(true);
    const links = wrapper.findAll("nav a");
    // Wordmark + Activity | Settings | Restore | About.
    expect(links.length).toBe(5);
  });

  it("marks the Activity link active (and no other) when on /activity", async () => {
    const wrapper = await mountAppAt("/activity");
    const activityLink = wrapper.find('a[href="/activity"]');
    const settingsLink = wrapper.find('a[href="/settings"]');
    expect(activityLink.attributes("aria-current")).toBe("page");
    expect(settingsLink.attributes("aria-current")).toBeUndefined();
  });

  it("keeps Settings active for a nested settings subtab (e.g. /accounts)", async () => {
    const wrapper = await mountAppAt("/accounts");
    const settingsLink = wrapper.find('a[href="/settings"]');
    expect(settingsLink.attributes("aria-current")).toBe("page");
  });

  it("marks Restore active for a nested route (e.g. /restore/some-source)", async () => {
    const wrapper = await mountAppAt("/restore/some-source");
    const restoreLink = wrapper.find('a[href="/restore"]');
    expect(restoreLink.attributes("aria-current")).toBe("page");
  });

  it("subscribes + hydrates the updater and progress stores on boot", async () => {
    await mountAppAt("/activity");
    // Three updater events (available, download_progress, downloaded) + one
    // sync-status event registered.
    expect(listenMock).toHaveBeenCalledTimes(4);
    expect(invokeMock).toHaveBeenCalledWith("get_pending_update_info", undefined);
    expect(invokeMock).toHaveBeenCalledWith("get_sync_status", undefined);
  });

  it("never throws when a subscribe registration fails - hydration still runs", async () => {
    listenMock.mockImplementationOnce(async () => {
      throw new Error("listen failed");
    });
    const wrapper = await mountAppAt("/activity");
    // Boot must complete (no unhandled rejection reaching the test) and the
    // shell still renders.
    expect(wrapper.find("nav").exists()).toBe(true);
    expect(invokeMock).toHaveBeenCalledWith("get_pending_update_info", undefined);
  });
});
