// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";
import { createMemoryHistory } from "vue-router";

import { i18n } from "../i18n";
import { createAppRouter } from "../router";
import { useAccountsStore } from "../stores/accounts";
import type { AccountDto, SettingsDto } from "../ipc/types";

// Mount tests for the Settings sidebar (SDD 2026-08-02 settings-sidebar-ia,
// task 3). SettingsNav is always mounted under a REAL router (not the
// useRouter()-mocking pattern the other Settings tests use) since its whole
// job is rendering RouterLinks and reacting to the current route - a fake
// push-only router would defeat the point. `createAppRouter` is reused
// directly so the route table under test is the actual app route table, not a
// hand-rolled subset that could silently drift from it.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/app", () => ({
  getVersion: vi.fn().mockResolvedValue("2.7.0"),
}));

import SettingsNav from "../components/SettingsNav.vue";

function makeSettings(over: Partial<SettingsDto> = {}): SettingsDto {
  return {
    global: {
      autoStartOnLogin: false,
      defaultConcurrentUploads: null,
      adaptiveParallelismEnabled: true,
      bandwidthCapMbps: null,
      skipOnBattery: true,
      skipOnMetered: true,
      scanIntervalSecs: 600,
      deepVerifyIntervalSecs: 604800,
      ioPriority: "low",
      logLevel: "info",
      schedule: {
        enabled: false,
        startMinute: 0,
        endMinute: 0,
        days: [true, true, true, true, true, true, true],
        utcOffsetMinutes: 0,
      },
      preBackupHook: null,
      postBackupHook: null,
      hookTimeoutSecs: 60,
      meteredMode: "pause",
      meteredBandwidthCapMbps: null,
      customRootCaPath: null,
      proxyMode: "system",
      proxyUrl: null,
      pauseWhenOffline: true,
      debugLoggingEnabled: false,
      debugLoggingExpiresAtMs: null,
    },
    telemetry: {
      enabled: true,
      installId: "id",
      endpoint: "https://example.test/ping",
    },
    updater: { channel: "stable", checkIntervalSecs: 21600 },
    ui: { trayLeftClickOpens: "activity", locale: "en-US", colorMode: "system" },
    windows: null,
    macos: null,
    bundleSmallFiles: false,
    scrub: { enabled: true, intervalSecs: 604800, sliceSize: 500, deepSample: 0 },
    drill: { enabled: true, intervalSecs: 2592000, sampleSize: 3 },
    ...over,
  };
}

function makeAccount(over: Partial<AccountDto> = {}): AccountDto {
  return {
    id: "acc-1",
    email: "user@example.test",
    displayName: null,
    state: "ok",
    encryptionEnabled: false,
    createdAt: 0,
    lastSyncedAt: null,
    backendKind: "google_drive",
    ...over,
  };
}

function backend(settings: SettingsDto | null, accounts: AccountDto[] = []): void {
  invokeMock.mockImplementation((cmd: string) => {
    if (cmd === "get_settings") {
      return settings === null
        ? Promise.reject(new Error("no settings"))
        : Promise.resolve(settings);
    }
    if (cmd === "list_accounts") return Promise.resolve(accounts);
    if (cmd === "get_update_channel") return Promise.resolve("stable");
    return Promise.resolve(undefined);
  });
}

async function mountAt(path: string) {
  const pinia = createPinia();
  setActivePinia(pinia);
  const router = createAppRouter(createMemoryHistory());
  await router.push(path);
  await router.isReady();
  const wrapper = mount(SettingsNav, {
    global: { plugins: [pinia, i18n, router] },
  });
  await flushPromises();
  return { wrapper, router, pinia };
}

beforeEach(() => {
  invokeMock.mockReset();
  backend(
    makeSettings({
      macos: {
        apfsSnapshot: false,
        menuBar: {
          showUploadSpeed: false,
          showPercent: false,
          showFiles: false,
          showEta: false,
          idle: "none",
        },
      },
    })
  );
});

describe("SettingsNav", () => {
  it("renders the object pages, the Preferences group and the About footer link", async () => {
    const { wrapper } = await mountAt("/settings/accounts");

    const text = wrapper.text();
    for (const label of [
      i18n.global.t("settings.nav.accounts"),
      i18n.global.t("settings.nav.sources"),
      i18n.global.t("settings.nav.general"),
      i18n.global.t("settings.nav.schedulePower"),
      i18n.global.t("settings.nav.performance"),
      i18n.global.t("settings.nav.network"),
      i18n.global.t("settings.nav.privacy"),
      i18n.global.t("settings.nav.advanced"),
    ]) {
      expect(text).toContain(label);
    }
    expect(wrapper.find('[data-testid="settings-nav-about"]').exists()).toBe(true);
  });

  it("labels the platform item macOS when settings.macos is non-null", async () => {
    const { wrapper } = await mountAt("/settings/accounts");
    expect(wrapper.text()).toContain(i18n.global.t("settings.nav.platformMacos"));
    expect(wrapper.find('[data-testid="settings-nav-item-platform"]').exists()).toBe(true);
  });

  it("hides the platform item entirely when both macos and windows are null", async () => {
    backend(makeSettings({ macos: null, windows: null }));
    const { wrapper } = await mountAt("/settings/accounts");
    expect(wrapper.find('[data-testid="settings-nav-item-platform"]').exists()).toBe(false);
  });

  it('filters to Schedule & Power when searching "batt"', async () => {
    const { wrapper } = await mountAt("/settings/accounts");

    await wrapper.get('[data-testid="settings-nav-search"]').setValue("batt");
    await flushPromises();

    const text = wrapper.text();
    expect(text).toContain(i18n.global.t("settings.nav.schedulePower"));
    expect(text).not.toContain(i18n.global.t("settings.nav.performance"));
    expect(text).not.toContain(i18n.global.t("settings.nav.advanced"));
    // Object pages (Accounts/Sources) are a separate list and are unaffected by
    // a Preferences-only keyword match - "batt" matches neither their label nor
    // their (empty) keyword set, so they too drop out of the filtered view.
    expect(wrapper.find('[data-testid="settings-nav-item-accounts"]').exists()).toBe(false);
  });

  it("links the footer to /settings/about", async () => {
    const { wrapper } = await mountAt("/settings/accounts");
    const link = wrapper.get('[data-testid="settings-nav-about"]');
    expect(link.attributes("href")).toBe("/settings/about");
    expect(link.text()).toContain("2.7.0");
  });

  it("shows the reauth badge count when accounts need reauth", async () => {
    const { wrapper, pinia } = await mountAt("/settings/accounts");
    const accounts = useAccountsStore(pinia);
    accounts.accounts = [
      makeAccount({ id: "a1", state: "needs_reauth" }),
      makeAccount({ id: "a2", state: "needs_reauth" }),
      makeAccount({ id: "a3", state: "ok" }),
    ];
    await flushPromises();

    const badge = wrapper.get('[data-testid="settings-nav-reauth-badge"]');
    expect(badge.text()).toBe("2");
  });

  it("renders no reauth badge when no account needs reauth", async () => {
    const { wrapper } = await mountAt("/settings/accounts");
    expect(wrapper.find('[data-testid="settings-nav-reauth-badge"]').exists()).toBe(false);
  });

  it("marks the active item for the current route", async () => {
    const { wrapper } = await mountAt("/settings/general");
    const generalItem = wrapper.get('[data-testid="settings-nav-item-general"]');
    expect(generalItem.attributes("aria-current")).toBe("page");
    const performanceItem = wrapper.get('[data-testid="settings-nav-item-performance"]');
    expect(performanceItem.attributes("aria-current")).toBeUndefined();
  });
});
