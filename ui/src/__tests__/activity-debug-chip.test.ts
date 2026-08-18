// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";

// Issue #309: the "Debug data included" chip next to Activity's export-
// diagnostic-bundle button, shown only while debug logging mode is on
// (mirrors the mockup's amber chip). Activity loads its own settings
// snapshot (via ensureSettingsLoaded) since it doesn't otherwise depend on
// the Settings view having been visited first.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => undefined),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: vi.fn(),
  save: vi.fn(),
}));
vi.mock("vue-router", () => ({
  useRouter: () => ({ push: vi.fn() }),
  useRoute: () => ({ params: {} }),
}));

import Activity from "../views/Activity.vue";
import type { SettingsDto } from "../ipc/types";

const CHIP = '[data-testid="activity-debug-data-chip"]';

function makeSettings(debugLoggingEnabled: boolean): SettingsDto {
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
      debugLoggingEnabled,
      debugLoggingExpiresAtMs: debugLoggingEnabled ? Date.now() + 60_000 : null,
    },
    telemetry: { enabled: true, installId: "id", endpoint: "https://example.test/ping" },
    updater: { channel: "stable", checkIntervalSecs: 21600 },
    ui: { trayLeftClickOpens: "activity", locale: "en-US", colorMode: "system" },
    windows: null,
    macos: null,
    bundleSmallFiles: false,
    scrub: { enabled: true, intervalSecs: 604800, sliceSize: 500, deepSample: 0 },
    drill: { enabled: true, intervalSecs: 2592000, sampleSize: 3 },
  };
}

function stubBackend(debugLoggingEnabled: boolean): void {
  invokeMock.mockImplementation(async (cmd: string) => {
    switch (cmd) {
      case "get_settings":
        return makeSettings(debugLoggingEnabled);
      case "query_activity":
        return {
          entries: [],
          total: 0,
          limit: 100,
          hasMore: false,
          nextBeforeTs: null,
          nextBeforeId: null,
        };
      case "distinct_activity_event_types":
        return ["upload_done"];
      case "activity_summary":
        return {
          bytesToday: 0,
          bytesWeek: 0,
          fileStatusCounts: [],
          throughputWindowBytes: 0,
          throughputWindowFiles: 0,
          throughputWindowMs: 60_000,
        };
      case "activity_throughput_series":
        return { bytes: [], files: [] };
      case "list_sources":
        return [];
      default:
        return undefined;
    }
  });
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
});

describe("Activity debug-data chip (issue #309)", () => {
  it("is absent when debug logging mode is off", async () => {
    stubBackend(false);
    const wrapper = mount(Activity, { global: { plugins: [i18n] } });
    await flushPromises();

    expect(wrapper.find(CHIP).exists()).toBe(false);

    wrapper.unmount();
  });

  it("is shown next to the export button when debug logging mode is on", async () => {
    stubBackend(true);
    const wrapper = mount(Activity, { global: { plugins: [i18n] } });
    await flushPromises();

    const chip = wrapper.get(CHIP);
    expect(chip.text()).toBe(i18n.global.t("activity.debugDataIncludedChip"));
    // Sits next to the export button (same flex row).
    expect(
      chip.element.parentElement?.querySelector('[data-testid="activity-export-bundle"]')
    ).toBeTruthy();

    wrapper.unmount();
  });
});
