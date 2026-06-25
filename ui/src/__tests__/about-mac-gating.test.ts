// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

// About.vue macOS install gating (ROADMAP M9 R1-P2-1). The V1 macOS in-app
// updater is not reliable, so on macOS the update-available banner must hide the
// in-app "Install update" button and surface a "Download the latest release"
// link instead; Windows/Linux keep the in-app install. We mock the IPC `invoke`
// + event `listen` seams (no Tauri runtime) and the `getVersion` app call, drive
// the updater store into the "available" state, and assert the rendered controls
// for each platform by stubbing navigator.userAgent.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: () => Promise.resolve(() => {}),
}));

vi.mock("@tauri-apps/api/app", () => ({
  getVersion: () => Promise.resolve("0.1.0"),
}));

import { i18n } from "../i18n";
import About from "../views/About.vue";
import { useUpdaterStore } from "../stores/updater";
import { useSettingsStore } from "../stores/settings";

const FAKE_SETTINGS = {
  telemetry: { enabled: false },
};

function setUserAgent(ua: string): void {
  Object.defineProperty(window.navigator, "userAgent", {
    value: ua,
    configurable: true,
  });
}

function mountAbout() {
  // The component shares the test's ACTIVE pinia (set in beforeEach) so the
  // store handle the test grabs is the same instance the component uses.
  return mount(About, {
    global: { plugins: [i18n] },
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  setActivePinia(createPinia());
  // Default backend responses for the About onMounted calls.
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case "get_settings":
        return Promise.resolve(FAKE_SETTINGS);
      case "get_update_channel":
        return Promise.resolve("stable");
      case "list_releases":
        return Promise.resolve([]);
      default:
        return Promise.resolve(null);
    }
  });
});

const MAC_UA =
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15";
const WIN_UA =
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36";

describe("About.vue macOS updater gating", () => {
  it("on macOS hides in-app install and shows a DMG download link", async () => {
    setUserAgent(MAC_UA);
    const settings = useSettingsStore();
    settings.settings = FAKE_SETTINGS as never;
    const wrapper = mountAbout();
    await flushPromises();

    // Force an available update so the banner renders.
    const updater = useUpdaterStore();
    updater.available = {
      version: "0.2.0",
      notes: "Faster sync.",
      publishedAt: "2026-06-24T00:00:00Z",
      channel: "stable",
    };
    await flushPromises();

    expect(wrapper.find('[data-testid="update-banner"]').exists()).toBe(true);
    // No in-app install button on macOS.
    expect(wrapper.find('[data-testid="install-update"]').exists()).toBe(false);
    // A DMG download link instead.
    const link = wrapper.find('[data-testid="download-latest-dmg"]');
    expect(link.exists()).toBe(true);
    expect(link.attributes("href")).toContain("/releases/latest");
    expect(
      wrapper.find('[data-testid="install-mac-unsupported"]').exists(),
    ).toBe(true);
  });

  it("on Windows shows the in-app install button (no DMG link)", async () => {
    setUserAgent(WIN_UA);
    const settings = useSettingsStore();
    settings.settings = FAKE_SETTINGS as never;
    const wrapper = mountAbout();
    await flushPromises();

    const updater = useUpdaterStore();
    updater.available = {
      version: "0.2.0",
      notes: "Faster sync.",
      publishedAt: "2026-06-24T00:00:00Z",
      channel: "stable",
    };
    await flushPromises();

    expect(wrapper.find('[data-testid="update-banner"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="install-update"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="download-latest-dmg"]').exists()).toBe(
      false,
    );
  });
});
