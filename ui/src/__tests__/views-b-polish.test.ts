// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";
import type { AccountDto } from "../ipc/types";

// VIEWS-B UX-polish tests: the friendly empty-state cards (no accounts / no
// sources) with their teal CTAs, the empty-dropdown placeholder pattern in the
// add-source wizard's account picker, and the teal active subtab in Settings.
// They drive the real components against a faked backend (the `invoke` seam) +
// faked Tauri event/dialog plugins, asserting the new affordances render and
// wire up the right action.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => undefined),
}));
const openDialogMock = vi.fn();
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...args: unknown[]) => openDialogMock(...args),
}));
vi.mock("@tauri-apps/api/app", () => ({
  getVersion: vi.fn().mockResolvedValue("0.1.0"),
}));

// vue-router: AccountList only needs useRouter().push, so a fake push-only
// implementation covers most of this file. The "Settings subtabs" describe is
// the exception - it mounts the Settings SHELL, which now renders real routed
// children via <RouterView> and <SettingsNav>'s <RouterLink>s (SDD
// 2026-08-02 settings-sidebar-ia, task 3) - so the real router primitives are
// kept alongside the fake `useRouter`/`useRoute` (RouterView/RouterLink
// resolve the router via internal inject keys, not these public composables).
const pushMock = vi.fn();
vi.mock("vue-router", async () => {
  const actual = await vi.importActual<typeof import("vue-router")>("vue-router");
  return {
    ...actual,
    useRouter: () => ({ push: pushMock }),
    useRoute: () => ({ params: {} }),
  };
});

import { createMemoryHistory } from "vue-router";
import AccountList from "../components/AccountList.vue";
import SourceTable from "../components/SourceTable.vue";
import AddSourceWizard from "../components/AddSourceWizard.vue";
import Settings from "../views/Settings.vue";
import { createAppRouter } from "../router";

function makeAccount(over: Partial<AccountDto> = {}): AccountDto {
  return {
    id: "acc-1",
    email: "user@example.com",
    displayName: null,
    state: "ok",
    encryptionEnabled: false,
    createdAt: 0,
    lastSyncedAt: null,
    backendKind: "google_drive",
    ...over,
  };
}

const globalMountOptions = { plugins: [i18n] };

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  openDialogMock.mockReset();
  pushMock.mockReset();
});

describe("AccountList empty state", () => {
  it("renders a friendly empty-state card whose CTA launches the setup wizard", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(AccountList, { global: globalMountOptions });
    await flushPromises();

    const empty = wrapper.find('[data-testid="accounts-empty"]');
    expect(empty.exists()).toBe(true);
    expect(empty.text()).toContain(i18n.global.t("settings.accounts.emptyTitle"));
    expect(empty.text()).toContain(i18n.global.t("settings.accounts.emptyHint"));

    await wrapper.get('[data-testid="accounts-empty-add"]').trigger("click");
    expect(pushMock).toHaveBeenCalledWith("/setup");
  });

  it("hides the empty state and shows account cards once an account exists", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.resolve([makeAccount()]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(AccountList, { global: globalMountOptions });
    await flushPromises();

    expect(wrapper.find('[data-testid="accounts-empty"]').exists()).toBe(false);
    expect(wrapper.text()).toContain("user@example.com");
    // The header "Add account" button is present and also routes to /setup.
    const add = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("settings.accounts.addButton"));
    expect(add).toBeTruthy();
    await add!.trigger("click");
    expect(pushMock).toHaveBeenCalledWith("/setup");
  });
});

describe("SourceTable empty state", () => {
  it("renders a friendly empty-state card whose CTA opens the add-source wizard", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourceTable, { global: globalMountOptions });
    await flushPromises();

    const empty = wrapper.find('[data-testid="sources-empty"]');
    expect(empty.exists()).toBe(true);
    expect(empty.text()).toContain(i18n.global.t("settings.sources.emptyTitle"));
    expect(empty.text()).toContain(i18n.global.t("settings.sources.emptyHint"));

    await wrapper.get('[data-testid="sources-empty-add"]').trigger("click");
    await flushPromises();
    // The wizard modal is now open (its title is distinct from the table title).
    expect(wrapper.text()).toContain(i18n.global.t("settings.addSource.title"));
  });
});

describe("AddSourceWizard account dropdown empty-state placeholder", () => {
  it("disables the account select and shows a 'connect one first' placeholder when there are no accounts", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(AddSourceWizard, { global: globalMountOptions });
    await (wrapper.vm as unknown as { start: () => Promise<void> }).start();
    await flushPromises();

    const select = wrapper.get("select");
    expect((select.element as HTMLSelectElement).disabled).toBe(true);
    const placeholder = select
      .findAll("option")
      .find((o) => o.attributes("disabled") !== undefined);
    expect(placeholder).toBeTruthy();
    expect(placeholder!.text()).toBe(i18n.global.t("settings.addSource.noAccounts"));
  });

  it("enables the account select and lists accounts when at least one exists", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.resolve([makeAccount()]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(AddSourceWizard, { global: globalMountOptions });
    await (wrapper.vm as unknown as { start: () => Promise<void> }).start();
    await flushPromises();

    const select = wrapper.get("select");
    expect((select.element as HTMLSelectElement).disabled).toBe(false);
    expect(select.text()).toContain("user@example.com");
  });
});

// SDD 2026-08-02 settings-sidebar-ia (task 3): the tab strip is gone - the
// active surface is now marked via the SettingsNav sidebar item's
// `aria-current`, driven by the real current route rather than a `tab` prop.
describe("Settings sidebar navigation", () => {
  it("marks the active sidebar item and leaves the others unmarked", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const router = createAppRouter(createMemoryHistory());
    await router.push("/settings/accounts");
    await router.isReady();
    const wrapper = mount(Settings, {
      global: { plugins: [...globalMountOptions.plugins, router] },
    });
    await flushPromises();

    const accountsItem = wrapper.get('[data-testid="settings-nav-item-accounts"]');
    expect(accountsItem.attributes("aria-current")).toBe("page");
    const sourcesItem = wrapper.get('[data-testid="settings-nav-item-sources"]');
    expect(sourcesItem.attributes("aria-current")).toBeUndefined();
  });
});
