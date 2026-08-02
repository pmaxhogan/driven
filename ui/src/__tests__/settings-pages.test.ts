// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";
import type { MenuBarSettings, ScheduleSettings, SettingsDto } from "../ipc/types";

// Mount tests for the seven Rules-tab page components (SDD 2026-08-02
// settings-sidebar-ia, task 2). This file is a PORT, not a rewrite: every test
// in settings-components.test.ts's old "Settings Rules tab" describe (which
// mounted the whole Settings.vue with `tab: "rules"` and drove the giant
// stacked form) moved here into a describe for the page that now owns the
// control it exercises - re-pointed to mount that page directly, with
// fixtures copied across unchanged. A handful of tests that exercised TWO
// fields now owned by two different pages (the original round-trip test, and
// the numeric-clamp regression test) are split into one test per page; no
// assertion from either half was weakened. One new test (the schedule
// null-guard) is added per the Task 1 deviation note. The old describe in
// settings-components.test.ts is deleted now that its coverage lives here.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/app", () => ({
  getVersion: vi.fn().mockResolvedValue("0.1.0"),
}));
// AccountsPage mounts AccountList (task 4), whose onMounted subscribes to the
// account:needs_reauth / oauth:complete events - stub `listen` so that
// subscription resolves instead of throwing on the missing Tauri runtime.
vi.mock("@tauri-apps/api/event", () => ({
  listen: () => Promise.resolve(() => {}),
}));
// Settings.vue (the shell, mounted only by the de-dupe regression test below)
// now renders real routed children through <RouterView> and a <SettingsNav>
// full of <RouterLink>s (SDD 2026-08-02 settings-sidebar-ia, task 3) - so the
// real router primitives are kept alongside the fake `useRouter`/`useRoute`
// (RouterView/RouterLink resolve the router via internal inject keys, not
// these public composables, so overriding them here is still safe).
vi.mock("vue-router", async () => {
  const actual = await vi.importActual<typeof import("vue-router")>("vue-router");
  return {
    ...actual,
    useRouter: () => ({ push: vi.fn() }),
    useRoute: () => ({ params: {} }),
  };
});

import { createMemoryHistory } from "vue-router";
import StartupCard from "../views/settings/StartupCard.vue";
import GeneralPage from "../views/settings/GeneralPage.vue";
import SchedulePowerPage from "../views/settings/SchedulePowerPage.vue";
import PerformancePage from "../views/settings/PerformancePage.vue";
import PlatformPage from "../views/settings/PlatformPage.vue";
import NetworkPage from "../views/settings/NetworkPage.vue";
import PrivacyPage from "../views/settings/PrivacyPage.vue";
import AdvancedPage from "../views/settings/AdvancedPage.vue";
import AccountsPage from "../views/settings/AccountsPage.vue";
import SourcesPage from "../views/settings/SourcesPage.vue";
import AboutPage from "../views/settings/AboutPage.vue";
import Settings from "../views/Settings.vue";
import { createAppRouter } from "../router";

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
    },
    telemetry: {
      enabled: true,
      installId: "id",
      endpoint: "https://example.test/ping",
    },
    updater: { channel: "stable", checkIntervalSecs: 21600 },
    ui: { trayLeftClickOpens: "activity", locale: "en-US", colorMode: "system" },
    windows: { vssMode: "auto", vssHelper: false },
    // null off macOS - the default fixture stands in for a non-mac host, so the
    // APFS block is absent unless a test opts in with `makeSettings({ macos })`.
    macos: null,
    bundleSmallFiles: false,
    scrub: { enabled: true, intervalSecs: 604800, sliceSize: 500, deepSample: 0 },
    ...over,
  };
}

const globalMountOptions = { plugins: [i18n] };

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
});

describe("StartupCard", () => {
  // SDD 2026-08-02 settings-sidebar-ia, task 7 fix (Linux launch-at-login
  // reachability). StartupCard is the extracted Startup section, now shared
  // between PlatformPage (macOS/Windows) and GeneralPage (Linux, where
  // PlatformPage itself is hidden from the nav). This is a direct unit test
  // of the extracted component, independent of either page.
  it("toggling auto-start patches global.autoStartOnLogin", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(StartupCard, { global: globalMountOptions });
    await flushPromises();

    const toggle = wrapper.get('[data-testid="autostart-toggle"]');
    await toggle.setValue(true);
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { autoStartOnLogin: true } },
    });
  });
});

describe("GeneralPage", () => {
  it("clamps an out-of-range scan interval to the backend range before patching", async () => {
    // Regression (moved from the old combined clamp test): a plausible
    // out-of-range value (a 10s scan interval) must be clamped client-side so
    // it never round-trips to a backend rejection - that rejection used to
    // brick the entire Rules form.
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      // GeneralPage also loads the update channel on mount (task 5) - stub it
      // so the channel select never renders with an undefined value here.
      if (cmd === "get_update_channel") return Promise.resolve("stable");
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(GeneralPage, { global: globalMountOptions });
    await flushPromises();

    const input = wrapper.get('[data-testid="rules-form"]').findAll('input[type="number"]')[0];
    await input.setValue("10"); // scan interval, backend min 30
    await input.trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { scanIntervalSecs: 30 } },
    });
  });

  it("channel select round-trips through updater.setChannel (moved from About, task 5)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_update_channel") return Promise.resolve("stable");
      if (cmd === "set_update_channel") return Promise.resolve("dev");
      if (cmd === "list_releases") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(GeneralPage, { global: globalMountOptions });
    await flushPromises();

    const select = wrapper.get('[data-testid="channel-select"]');
    expect((select.element as HTMLSelectElement).value).toBe("stable");

    await select.setValue("dev");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("set_update_channel", { channel: "dev" });
  });

  it("the check-for-updates action calls check_for_update and shows the result (task 5)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_update_channel") return Promise.resolve("stable");
      if (cmd === "check_for_update") return Promise.resolve(null);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(GeneralPage, { global: globalMountOptions });
    await flushPromises();

    await wrapper.get('[data-testid="check-updates"]').trigger("click");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("check_for_update", undefined);
    expect(wrapper.find('[data-testid="check-uptodate"]').exists()).toBe(true);
  });

  // SDD 2026-08-02 settings-sidebar-ia, task 7 fix. On Linux, `macos` and
  // `windows` are BOTH null, so SettingsNav hides the Platform nav item -
  // making the Startup card (rendered only on PlatformPage until this fix)
  // unreachable there. GeneralPage renders the same StartupCard, but only
  // exactly when PlatformPage is hidden, so exactly one copy of the card is
  // reachable on every OS.
  it("shows the Startup card when both macos and windows are null (Linux reachability)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings({ windows: null }));
      if (cmd === "get_update_channel") return Promise.resolve("stable");
      return Promise.resolve(undefined);
    });
    const wrapper = mount(GeneralPage, { global: globalMountOptions });
    await flushPromises();

    expect(wrapper.find('[data-testid="startup-setting"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="autostart-toggle"]').exists()).toBe(true);
  });

  it("hides the Startup card when a platform settings group is present (already reachable via Platform)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      // The default fixture already has `windows` non-null (a non-mac host),
      // which alone should be enough to suppress GeneralPage's copy.
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_update_channel") return Promise.resolve("stable");
      return Promise.resolve(undefined);
    });
    const wrapper = mount(GeneralPage, { global: globalMountOptions });
    await flushPromises();

    expect(wrapper.find('[data-testid="startup-setting"]').exists()).toBe(false);
  });
});

describe("SchedulePowerPage", () => {
  it("loads settings and round-trips the skip-on-battery toggle", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(SchedulePowerPage, { global: globalMountOptions });
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("get_settings", undefined);
    const form = wrapper.get('[data-testid="rules-form"]');

    // skipOnBattery is the FIRST checkbox on this page (the Startup
    // auto-start toggle that used to precede it in the old combined form now
    // lives on PlatformPage).
    const batteryCheckbox = form.findAll('input[type="checkbox"]')[0];
    await batteryCheckbox.setValue(false);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { skipOnBattery: false } },
    });
  });

  it("pause-when-offline toggle patches global.pauseWhenOffline", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(SchedulePowerPage, { global: globalMountOptions });
    await flushPromises();

    const toggle = wrapper.get('[data-testid="pause-when-offline-toggle"]');
    // Checked by default (makeSettings() defaults pauseWhenOffline: true) -
    // pins the :checked binding, not just the @change handler, so a wired-
    // backwards or missing binding would fail here even though the change
    // event alone can't distinguish it.
    expect((toggle.element as HTMLInputElement).checked).toBe(true);

    await toggle.setValue(false);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { pauseWhenOffline: false } },
    });
  });

  it("renders the pause-when-offline toggle unchecked when the setting is off", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") {
        return Promise.resolve(
          makeSettings({
            global: { ...makeSettings().global, pauseWhenOffline: false },
          })
        );
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(SchedulePowerPage, { global: globalMountOptions });
    await flushPromises();

    const toggle = wrapper.get('[data-testid="pause-when-offline-toggle"]');
    expect((toggle.element as HTMLInputElement).checked).toBe(false);
  });

  it("schedule window: enable, edit time, and toggle a day each patch the schedule", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(SchedulePowerPage, { global: globalMountOptions });
    await flushPromises();

    type SchedPatch = { patch?: { global?: { schedule?: ScheduleSettings } } };
    const lastSchedule = (): ScheduleSettings | undefined =>
      invokeMock.mock.calls
        .filter((c) => c[0] === "update_settings" && (c[1] as SchedPatch).patch?.global?.schedule)
        .map((c) => (c[1] as SchedPatch).patch!.global!.schedule!)
        .pop();

    // The time/day controls are hidden until the schedule is enabled.
    expect(wrapper.find('input[type="time"]').exists()).toBe(false);

    await wrapper.get('[data-testid="schedule-enabled"]').setValue(true);
    await flushPromises();
    const enabled = lastSchedule();
    expect(enabled?.enabled).toBe(true);
    expect(enabled?.days).toHaveLength(7);
    expect(typeof enabled?.utcOffsetMinutes).toBe("number");

    // The window controls are now visible; editing the start time re-patches
    // the local minute-of-day (09:30 -> 570).
    const start = wrapper.get('input[type="time"]');
    await start.setValue("09:30");
    await start.trigger("change");
    await flushPromises();
    expect(lastSchedule()?.startMinute).toBe(9 * 60 + 30);

    // Toggling the Sunday (index 0) button flips that day off.
    const dayButtons = wrapper.findAll('[data-testid="schedule-setting"] button');
    expect(dayButtons).toHaveLength(7);
    await dayButtons[0].trigger("click");
    await flushPromises();
    expect(lastSchedule()?.days[0]).toBe(false);
  });

  it("an invalid schedule time commits nothing (Task 1 hhmmToMinutes null deviation)", async () => {
    // DEVIATION from the pre-task-1 form: hhmmToMinutes now returns `null` for
    // an unparseable "HH:MM" string (rather than silently coercing to
    // midnight), and the schedule commit here skips entirely on null - the
    // field's last-good persisted value is left untouched.
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({
            global: {
              ...makeSettings().global,
              schedule: {
                enabled: true,
                startMinute: 9 * 60,
                endMinute: 10 * 60,
                days: [true, true, true, true, true, true, true],
                utcOffsetMinutes: 0,
              },
            },
          })
        );
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(SchedulePowerPage, { global: globalMountOptions });
    await flushPromises();

    const start = wrapper.get('input[type="time"]');
    expect((start.element as HTMLInputElement).value).toBe("09:00");

    // Clearing the field to "" is what a native <input type="time"> reports
    // once emptied - hhmmToMinutes("") -> null, so the commit must be skipped
    // entirely (no update_settings call carrying a schedule patch).
    await start.setValue("");
    await start.trigger("change");
    await flushPromises();

    type SchedPatch = { patch?: { global?: { schedule?: unknown } } };
    const sentASchedulePatch = invokeMock.mock.calls.some(
      (c) =>
        c[0] === "update_settings" && (c[1] as SchedPatch).patch?.global?.schedule !== undefined
    );
    expect(sentASchedulePatch).toBe(false);
  });

  it("metered: switching to throttle patches the mode and reveals the cap input", async () => {
    // Deep-merge the global on round-trip so the metered section (gated on
    // skipOnMetered) stays rendered after the mode patch.
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: { global?: Record<string, unknown> } }).patch;
        const base = makeSettings();
        return Promise.resolve({
          ...base,
          global: { ...base.global, ...(patch.global ?? {}) },
        });
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(SchedulePowerPage, { global: globalMountOptions });
    await flushPromises();

    // In pause mode the throttle cap input is hidden.
    expect(wrapper.find('[data-testid="metered-setting"] input[type="number"]').exists()).toBe(
      false
    );

    await wrapper.get('[data-testid="metered-mode"]').setValue("throttle");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { meteredMode: "throttle" } },
    });

    // The cap input now appears; setting it patches the metered cap.
    const cap = wrapper.get('[data-testid="metered-setting"] input[type="number"]');
    await cap.setValue("5");
    await cap.trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { meteredBandwidthCapMbps: 5 } },
    });
  });

  it("keeps the Rules form visible with a localized banner when a patch is rejected", async () => {
    // Regression: a rejected patch must NOT replace the whole form with the raw
    // error ("[object Object]") and brick the page. The form stays mounted and an
    // inline, localized error banner appears so the user can correct the value.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings")
        return Promise.reject({ code: "internal.invalid_input", message: "out of range" });
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SchedulePowerPage, { global: globalMountOptions });
    await flushPromises();
    // Any commit that patches: toggle "pause on battery" (the first checkbox
    // on this page).
    const battery = wrapper.get('[data-testid="rules-form"]').findAll('input[type="checkbox"]')[0];
    await battery.setValue(false);
    await battery.trigger("change");
    await flushPromises();
    // The form is STILL mounted (not bricked) ...
    expect(wrapper.find('[data-testid="rules-form"]').exists()).toBe(true);
    // ... and a localized banner shows the error - never "[object Object]".
    const banner = wrapper.get('[data-testid="rules-error"]');
    expect(banner.text().length).toBeGreaterThan(0);
    expect(banner.text()).not.toContain("[object Object]");
  });
});

describe("PerformancePage", () => {
  it("round-trips the bandwidth cap numeric field", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(PerformancePage, { global: globalMountOptions });
    await flushPromises();

    // Set the bandwidth cap (empty = unlimited -> 50 Mbps).
    const form = wrapper.get('[data-testid="rules-form"]');
    const numberInputs = form.findAll('input[type="number"]');
    const bandwidthInput = numberInputs[0];
    await bandwidthInput.setValue("50");
    await bandwidthInput.trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { bandwidthCapMbps: 50 } },
    });
  });

  it("an empty bandwidth cap patches null (unlimited)", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({
            global: { ...makeSettings().global, bandwidthCapMbps: 25 },
          })
        );
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PerformancePage, { global: globalMountOptions });
    await flushPromises();
    const form = wrapper.get('[data-testid="rules-form"]');
    const bandwidthInput = form.findAll('input[type="number"]')[0];
    await bandwidthInput.setValue("");
    await bandwidthInput.trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { bandwidthCapMbps: null } },
    });
  });

  it("clamps an out-of-range concurrent-uploads value to the backend range before patching", async () => {
    // Regression (moved from the old combined clamp test): a plausible
    // out-of-range value (100 concurrent uploads, backend max 32) must be
    // clamped client-side so it never round-trips to a backend rejection.
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PerformancePage, { global: globalMountOptions });
    await flushPromises();
    const nums = wrapper.get('[data-testid="rules-form"]').findAll('input[type="number"]');
    // Order on this page: [bandwidth, concurrent].
    await nums[1].setValue("100"); // concurrent uploads, backend max 32
    await nums[1].trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { defaultConcurrentUploads: 32 } },
    });
  });

  it("toggles adaptive upload parallelism (DESIGN 11.4.7)", async () => {
    // The kill-switch reflects the persisted value and patches on toggle. Starts
    // ON (makeSettings default), so unchecking it sends `false`.
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PerformancePage, { global: globalMountOptions });
    await flushPromises();
    const toggle = wrapper.get('[data-testid="adaptive-parallelism-toggle"]');
    expect((toggle.element as HTMLInputElement).checked).toBe(true);
    await toggle.setValue(false);
    await toggle.trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { adaptiveParallelismEnabled: false } },
    });
  });
});

describe("PlatformPage", () => {
  it("shows the degraded locked-file-backup banner when the helper status says so", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_vss_helper_status")
        return Promise.resolve({
          supported: true,
          elevated: false,
          helperEnabled: false,
          helperAlive: false,
          helperLaunchable: false,
          lockedFileBackupDegraded: true,
        });
      return Promise.resolve(undefined);
    });

    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("get_vss_helper_status", undefined);
    expect(wrapper.find('[data-testid="vss-degraded-banner"]').exists()).toBe(true);
    expect(wrapper.get('[data-testid="vss-degraded-banner"]').text()).toContain(
      "Locked files are being skipped"
    );
  });

  it("hides the degraded banner when locked-file backup is available", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_vss_helper_status")
        return Promise.resolve({
          supported: true,
          elevated: true,
          helperEnabled: false,
          helperAlive: false,
          helperLaunchable: false,
          lockedFileBackupDegraded: false,
        });
      return Promise.resolve(undefined);
    });

    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    expect(wrapper.find('[data-testid="vss-degraded-banner"]').exists()).toBe(false);
  });

  it("startup: auto-start renders ON by default and toggling it patches the preference", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({ global: { ...makeSettings().global, autoStartOnLogin: true } })
        );
      if (cmd === "update_settings") {
        const patch = (args as { patch: { global?: Record<string, unknown> } }).patch;
        const base = makeSettings();
        return Promise.resolve({
          ...base,
          global: { ...base.global, ...(patch.global ?? {}) },
        });
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    // Default ON: the toggle reflects the persisted preference.
    const toggle = wrapper.get('[data-testid="autostart-toggle"]');
    expect((toggle.element as HTMLInputElement).checked).toBe(true);

    // Turning it off patches the persisted preference (the backend then
    // unregisters the OS startup entry).
    await toggle.setValue(false);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { autoStartOnLogin: false } },
    });
  });

  it("changes the Windows VSS mode when the windows settings group is present", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();
    const vssSelect = wrapper.get('[data-testid="vss-mode"]');
    await vssSelect.setValue("never");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { windows: { vssMode: "never" } },
    });
  });

  it("issue #25: renders the VSS helper toggle and toggling it patches windows.vssHelper", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_vss_helper_status")
        return Promise.resolve({
          supported: true,
          elevated: false,
          helperEnabled: false,
          helperAlive: false,
          helperLaunchable: true,
          lockedFileBackupDegraded: false,
        });
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    const toggle = wrapper.get('[data-testid="vss-helper-toggle"]');
    // Reflects the stored setting (default off).
    expect((toggle.element as HTMLInputElement).checked).toBe(false);
    await toggle.setValue(true);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { windows: { vssHelper: true } },
    });
  });

  it("issue #25: toggling the VSS helper survives a failing status re-fetch", async () => {
    // The setVssHelper handler re-fetches get_vss_helper_status after committing;
    // a rejection there must be swallowed (no unhandled rejection, no crash) - the
    // commit still lands.
    let statusCalls = 0;
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_vss_helper_status") {
        statusCalls += 1;
        // First call (on mount) resolves; the post-toggle re-fetch rejects.
        return statusCalls === 1
          ? Promise.resolve({
              supported: true,
              elevated: false,
              helperEnabled: false,
              helperAlive: false,
              helperLaunchable: true,
              lockedFileBackupDegraded: false,
            })
          : Promise.reject(new Error("status unavailable"));
      }
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    const toggle = wrapper.get('[data-testid="vss-helper-toggle"]');
    await toggle.setValue(true);
    await flushPromises();

    // The commit still happened despite the failing status re-fetch.
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { windows: { vssHelper: true } },
    });
    // The degraded banner is not shown (status went null on the rejection).
    expect(wrapper.find('[data-testid="vss-degraded-banner"]').exists()).toBe(false);
  });

  it("issue #25: enabling the helper shows the waiting-for-approval hint, then resolves on poll", async () => {
    vi.useFakeTimers();
    try {
      let statusCall = 0;
      invokeMock.mockImplementation((cmd: string, args: unknown) => {
        if (cmd === "get_settings") return Promise.resolve(makeSettings());
        if (cmd === "get_vss_helper_status") {
          statusCall += 1;
          // On mount: not degraded. After enabling: pending. On the first poll:
          // declined (the user dismissed the UAC prompt).
          if (statusCall === 1) {
            return Promise.resolve({
              supported: true,
              elevated: false,
              helperEnabled: false,
              helperAlive: false,
              helperLaunchable: true,
              launchPending: false,
              launchDeclined: false,
              lockedFileBackupDegraded: false,
            });
          }
          if (statusCall === 2) {
            return Promise.resolve({
              supported: true,
              elevated: false,
              helperEnabled: true,
              helperAlive: false,
              helperLaunchable: true,
              launchPending: true,
              launchDeclined: false,
              lockedFileBackupDegraded: false,
            });
          }
          return Promise.resolve({
            supported: true,
            elevated: false,
            helperEnabled: true,
            helperAlive: false,
            helperLaunchable: false,
            launchPending: false,
            launchDeclined: true,
            lockedFileBackupDegraded: true,
          });
        }
        if (cmd === "update_settings") {
          const patch = (args as { patch: Record<string, unknown> }).patch;
          return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
        }
        return Promise.resolve(undefined);
      });
      const wrapper = mount(PlatformPage, { global: globalMountOptions });
      await flushPromises();

      const toggle = wrapper.get('[data-testid="vss-helper-toggle"]');
      await toggle.setValue(true);
      await flushPromises();

      // The eager enable committed and the pending hint is shown.
      expect(invokeMock).toHaveBeenCalledWith("update_settings", {
        patch: { windows: { vssHelper: true } },
      });
      expect(wrapper.find('[data-testid="vss-helper-pending"]').exists()).toBe(true);

      // Advance the poll: the launch resolves to declined -> declined hint shown.
      await vi.advanceTimersByTimeAsync(1600);
      await flushPromises();
      expect(wrapper.find('[data-testid="vss-helper-pending"]').exists()).toBe(false);
      expect(wrapper.find('[data-testid="vss-helper-declined"]').exists()).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  // --- macOS APFS-snapshot locked-file backup (DESIGN s5.3.2) ---
  //
  // The mac twin of the VSS helper block above. `settings.macos` is non-null ONLY
  // on macOS, and that nullability IS the platform check - no userAgent sniffing.

  function apfsStatus(over: Record<string, unknown> = {}): Record<string, unknown> {
    return {
      supported: true,
      helperEnabled: false,
      helperAlive: false,
      helperLaunchable: true,
      launchPending: false,
      launchDeclined: false,
      lockedFileBackupDegraded: false,
      ...over,
    };
  }

  // Menu bar extra config (spec 2026-07-31 s2) - `MacosSettings.menuBar` is
  // required now, so every `macos: {...}` fixture below needs one.
  function menuBarSettings(over: Partial<MenuBarSettings> = {}): MenuBarSettings {
    return {
      showUploadSpeed: false,
      showPercent: false,
      showFiles: false,
      showEta: false,
      idle: "none",
      ...over,
    };
  }

  it("DESIGN s5.3.2: renders the APFS snapshot toggle on macOS and toggling it patches macos.apfsSnapshot", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({ macos: { apfsSnapshot: false, menuBar: menuBarSettings() } })
        );
      if (cmd === "get_vss_helper_status") return Promise.resolve(undefined);
      if (cmd === "get_apfs_helper_status") return Promise.resolve(apfsStatus());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("get_apfs_helper_status", undefined);
    expect(wrapper.find('[data-testid="apfs-snapshot-setting"]').exists()).toBe(true);
    // The TCC caveat is always shown next to the toggle: a snapshot cannot read
    // around a macOS privacy denial, so that case still needs Full Disk Access.
    const tccNote = wrapper.get('[data-testid="apfs-snapshot-tcc-note"]');
    expect(tccNote.text()).toContain("Full Disk Access");

    const toggle = wrapper.get('[data-testid="apfs-snapshot-toggle"]');
    // Reflects the stored setting (default off).
    expect((toggle.element as HTMLInputElement).checked).toBe(false);
    await toggle.setValue(true);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { macos: { apfsSnapshot: true } },
    });
  });

  it("DESIGN s5.3.2: hides the APFS snapshot toggle when macos is null (off macOS)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      // The default fixture has `macos: null` - i.e. a non-mac host.
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_apfs_helper_status")
        return Promise.resolve(apfsStatus({ supported: false, helperLaunchable: false }));
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    expect(wrapper.find('[data-testid="apfs-snapshot-setting"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="apfs-snapshot-toggle"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="apfs-snapshot-tcc-note"]').exists()).toBe(false);
  });

  it("DESIGN s5.3.2: enabling the APFS snapshot shows the waiting-for-approval hint, then resolves on poll", async () => {
    vi.useFakeTimers();
    try {
      let statusCall = 0;
      invokeMock.mockImplementation((cmd: string, args: unknown) => {
        if (cmd === "get_settings")
          return Promise.resolve(
            makeSettings({ macos: { apfsSnapshot: false, menuBar: menuBarSettings() } })
          );
        if (cmd === "get_apfs_helper_status") {
          statusCall += 1;
          // On mount: idle. After enabling: pending (the admin prompt is up).
          // Still pending on the first poll (so the poll re-arms itself), then
          // declined on the second (the user dismissed the prompt).
          if (statusCall === 1) return Promise.resolve(apfsStatus());
          if (statusCall <= 3)
            return Promise.resolve(apfsStatus({ helperEnabled: true, launchPending: true }));
          return Promise.resolve(
            apfsStatus({
              helperEnabled: true,
              helperLaunchable: false,
              launchDeclined: true,
              lockedFileBackupDegraded: true,
            })
          );
        }
        if (cmd === "update_settings") {
          const patch = (args as { patch: Record<string, unknown> }).patch;
          return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
        }
        return Promise.resolve(undefined);
      });
      const wrapper = mount(PlatformPage, { global: globalMountOptions });
      await flushPromises();

      const toggle = wrapper.get('[data-testid="apfs-snapshot-toggle"]');
      await toggle.setValue(true);
      await flushPromises();

      // The eager enable committed and the pending hint is shown.
      expect(invokeMock).toHaveBeenCalledWith("update_settings", {
        patch: { macos: { apfsSnapshot: true } },
      });
      expect(wrapper.find('[data-testid="apfs-helper-pending"]').exists()).toBe(true);

      // First poll tick: still pending, so the poll re-arms itself.
      await vi.advanceTimersByTimeAsync(1600);
      await flushPromises();
      expect(wrapper.find('[data-testid="apfs-helper-pending"]').exists()).toBe(true);

      // Second tick: the launch resolves to declined -> declined hint shown.
      await vi.advanceTimersByTimeAsync(1600);
      await flushPromises();
      expect(wrapper.find('[data-testid="apfs-helper-pending"]').exists()).toBe(false);
      expect(wrapper.find('[data-testid="apfs-helper-declined"]').exists()).toBe(true);

      // Unmounting with a spent poll handle still in hand clears it, so no timer
      // is orphaned when the user navigates away mid-launch.
      wrapper.unmount();
      await vi.advanceTimersByTimeAsync(5000);
      await flushPromises();
    } finally {
      vi.useRealTimers();
    }
  });

  it("DESIGN s5.3.2: an unavailable APFS status on tab load hides both hints", async () => {
    // IPC unavailable (e.g. a browser dev shell): the page must still render
    // with no hints rather than surface an error.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({ macos: { apfsSnapshot: true, menuBar: menuBarSettings() } })
        );
      if (cmd === "get_apfs_helper_status") return Promise.reject(new Error("status unavailable"));
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    expect(wrapper.find('[data-testid="apfs-snapshot-toggle"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="apfs-helper-pending"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="apfs-helper-declined"]').exists()).toBe(false);
  });

  it("DESIGN s5.3.2: toggling the APFS snapshot survives a failing status re-fetch", async () => {
    // The setApfsSnapshot handler re-fetches get_apfs_helper_status after
    // committing; a rejection there must be swallowed (no unhandled rejection, no
    // crash) - the commit still lands and no hint is shown.
    let statusCalls = 0;
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({ macos: { apfsSnapshot: false, menuBar: menuBarSettings() } })
        );
      if (cmd === "get_apfs_helper_status") {
        statusCalls += 1;
        // First call (on mount) resolves; the post-toggle re-fetch rejects.
        return statusCalls === 1
          ? Promise.resolve(apfsStatus())
          : Promise.reject(new Error("status unavailable"));
      }
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    const toggle = wrapper.get('[data-testid="apfs-snapshot-toggle"]');
    await toggle.setValue(true);
    await flushPromises();

    // The commit still happened despite the failing status re-fetch.
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { macos: { apfsSnapshot: true } },
    });
    // Status went null on the rejection, so neither hint is rendered.
    expect(wrapper.find('[data-testid="apfs-helper-pending"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="apfs-helper-declined"]').exists()).toBe(false);
  });

  // --- macOS menu bar extra (spec 2026-07-31 s2) ---

  it("menu bar card toggles a metric via a macos.menuBar patch", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({
            macos: {
              apfsSnapshot: false,
              menuBar: menuBarSettings({
                showUploadSpeed: true,
                showPercent: true,
                showFiles: false,
                showEta: false,
                idle: "none",
              }),
            },
          })
        );
      if (cmd === "get_apfs_helper_status") return Promise.resolve(apfsStatus());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    expect(wrapper.find('[data-testid="menubar-setting"]').exists()).toBe(true);
    const filesToggle = wrapper.get('[data-testid="menubar-files-toggle"]');
    expect((filesToggle.element as HTMLInputElement).checked).toBe(false);
    await filesToggle.setValue(true);
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { macos: { menuBar: { showFiles: true } } },
    });

    // The other three toggles each patch their own single field the same way.
    // NOTE: the `update_settings` mock above echoes back ONLY the patched keys
    // (it replaces `macos` wholesale rather than merging), so after the files
    // toggle every menuBar field but `showFiles` reads back as unset/false -
    // toggle the rest ON (true) rather than off, or vue-test-utils' setChecked
    // silently no-ops when the checkbox is already in the target state.
    await wrapper.get('[data-testid="menubar-speed-toggle"]').setValue(true);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { macos: { menuBar: { showUploadSpeed: true } } },
    });

    await wrapper.get('[data-testid="menubar-percent-toggle"]').setValue(true);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { macos: { menuBar: { showPercent: true } } },
    });

    await wrapper.get('[data-testid="menubar-eta-toggle"]').setValue(true);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { macos: { menuBar: { showEta: true } } },
    });
  });

  it("menu bar idle select patches macos.menuBar.idle", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({ macos: { apfsSnapshot: false, menuBar: menuBarSettings() } })
        );
      if (cmd === "get_apfs_helper_status") return Promise.resolve(apfsStatus());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    const idleSelect = wrapper.get('[data-testid="menubar-idle-select"]');
    await idleSelect.setValue("uploadedToday");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { macos: { menuBar: { idle: "uploadedToday" } } },
    });
  });

  it("menu bar shows the width hint once 3 or more metrics are enabled", async () => {
    // Unlike the naive `makeSettings(patch)` echo used by the other tests here
    // (which replaces `macos` wholesale, dropping the untouched menuBar fields),
    // this test needs the enabled-count to accumulate across a real toggle, so
    // it merges the patch into the current menuBar - matching what the real
    // backend's field-wise merge (Task 2) actually returns.
    let currentMenuBar = menuBarSettings({ showUploadSpeed: true, showPercent: true });
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({ macos: { apfsSnapshot: false, menuBar: currentMenuBar } })
        );
      if (cmd === "get_apfs_helper_status") return Promise.resolve(apfsStatus());
      if (cmd === "update_settings") {
        const patch = (args as { patch: { macos?: { menuBar?: Partial<MenuBarSettings> } } }).patch;
        if (patch.macos?.menuBar) {
          currentMenuBar = { ...currentMenuBar, ...patch.macos.menuBar };
        }
        return Promise.resolve(
          makeSettings({ macos: { apfsSnapshot: false, menuBar: currentMenuBar } })
        );
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    // Two enabled metrics: no width hint yet.
    expect(wrapper.find('[data-testid="menubar-setting"]').text()).not.toContain(
      i18n.global.t("settings.rules.menuBarWidthHint")
    );

    // Enabling a third metric (files) crosses the threshold.
    const filesToggle = wrapper.get('[data-testid="menubar-files-toggle"]');
    await filesToggle.setValue(true);
    await flushPromises();

    expect(wrapper.find('[data-testid="menubar-setting"]').text()).toContain(
      i18n.global.t("settings.rules.menuBarWidthHint")
    );
  });

  it("menu bar preview reflects enabled metrics", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({
            macos: {
              apfsSnapshot: false,
              menuBar: menuBarSettings({ showUploadSpeed: true, showPercent: true }),
            },
          })
        );
      if (cmd === "get_apfs_helper_status") return Promise.resolve(apfsStatus());
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    const preview = wrapper.get('[data-testid="menubar-preview"]');
    expect(preview.text()).toContain("62%");
    expect(preview.text()).toContain("84 Mbps");
    wrapper.unmount();

    // Turning percent off in the seeded settings drops "62%" from the preview.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({
            macos: {
              apfsSnapshot: false,
              menuBar: menuBarSettings({ showUploadSpeed: true, showPercent: false }),
            },
          })
        );
      if (cmd === "get_apfs_helper_status") return Promise.resolve(apfsStatus());
      return Promise.resolve(undefined);
    });
    // Fresh Pinia: the settings store otherwise still holds the first mount's
    // snapshot - a second mount would silently reuse it.
    setActivePinia(createPinia());
    const wrapper2 = mount(PlatformPage, { global: globalMountOptions });
    await flushPromises();

    const preview2 = wrapper2.get('[data-testid="menubar-preview"]');
    expect(preview2.text()).not.toContain("62%");
    expect(preview2.text()).toContain("84 Mbps");
  });
});

describe("NetworkPage", () => {
  it("custom root CA: a valid path validates, shows the cert count, and patches (issue #34)", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "validate_custom_ca") {
        expect((args as { path: string }).path).toBe("/etc/corp/ca.pem");
        return Promise.resolve({ certCount: 2 });
      }
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(NetworkPage, { global: globalMountOptions });
    await flushPromises();

    const input = wrapper.get('[data-testid="custom-ca-path"]');
    await input.setValue("/etc/corp/ca.pem");
    await input.trigger("change");
    await flushPromises();

    // Validated (cert count surfaced) AND persisted.
    expect(invokeMock).toHaveBeenCalledWith("validate_custom_ca", { path: "/etc/corp/ca.pem" });
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { customRootCaPath: "/etc/corp/ca.pem" } },
    });
    const feedback = wrapper.get('[data-testid="custom-ca-feedback"]');
    expect(feedback.text()).toContain("2");
  });

  it("custom root CA: an invalid file surfaces an error and is NOT persisted (issue #34)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "validate_custom_ca") {
        return Promise.reject({ code: "internal.invalid_input", message: "bad pem" });
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(NetworkPage, { global: globalMountOptions });
    await flushPromises();

    const input = wrapper.get('[data-testid="custom-ca-path"]');
    await input.setValue("/etc/corp/broken.pem");
    await input.trigger("change");
    await flushPromises();

    // Error feedback shown; the bad path is NOT saved (no update_settings call).
    expect(wrapper.find('[data-testid="custom-ca-feedback"]').exists()).toBe(true);
    const savedCa = invokeMock.mock.calls.some((c) => c[0] === "update_settings");
    expect(savedCa).toBe(false);
  });

  it("custom root CA: clearing the path patches null (back to system trust) (issue #34)", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({
            global: { ...makeSettings().global, customRootCaPath: "/etc/corp/ca.pem" },
          })
        );
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(NetworkPage, { global: globalMountOptions });
    await flushPromises();

    const input = wrapper.get('[data-testid="custom-ca-path"]');
    await input.setValue("   ");
    await input.trigger("change");
    await flushPromises();

    // A blank path clears the setting (null) WITHOUT calling validate.
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { customRootCaPath: null } },
    });
    expect(invokeMock.mock.calls.some((c) => c[0] === "validate_custom_ca")).toBe(false);
  });

  it("proxy: switching to 'none' patches the mode and clears the URL (issue #34)", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(NetworkPage, { global: globalMountOptions });
    await flushPromises();

    const select = wrapper.get('[data-testid="proxy-mode"]');
    await select.setValue("none");
    await select.trigger("change");
    await flushPromises();

    // 'none' commits immediately (no URL) and does NOT validate.
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { proxyMode: "none", proxyUrl: null } },
    });
    expect(invokeMock.mock.calls.some((c) => c[0] === "validate_proxy")).toBe(false);
  });

  it("proxy: a valid manual URL validates and patches (issue #34)", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "validate_proxy") {
        expect((args as { mode: string; url: string }).mode).toBe("manual");
        expect((args as { mode: string; url: string }).url).toBe("socks5://127.0.0.1:1080");
        return Promise.resolve(undefined);
      }
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(NetworkPage, { global: globalMountOptions });
    await flushPromises();

    const select = wrapper.get('[data-testid="proxy-mode"]');
    await select.setValue("manual");
    await select.trigger("change");
    await flushPromises();

    // The URL field appears in manual mode.
    const input = wrapper.get('[data-testid="proxy-url"]');
    await input.setValue("socks5://127.0.0.1:1080");
    await input.trigger("change");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("validate_proxy", {
      mode: "manual",
      url: "socks5://127.0.0.1:1080",
    });
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { proxyMode: "manual", proxyUrl: "socks5://127.0.0.1:1080" } },
    });
    expect(wrapper.get('[data-testid="proxy-feedback"]').classes()).toContain("text-teal-600");
  });

  it("proxy: an invalid manual URL surfaces an error and is NOT persisted (issue #34)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "validate_proxy") {
        return Promise.reject({ code: "internal.invalid_input", message: "bad scheme" });
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(NetworkPage, { global: globalMountOptions });
    await flushPromises();

    const select = wrapper.get('[data-testid="proxy-mode"]');
    await select.setValue("manual");
    await select.trigger("change");
    await flushPromises();
    const input = wrapper.get('[data-testid="proxy-url"]');
    await input.setValue("ftp://nope:21");
    await input.trigger("change");
    await flushPromises();

    // Error feedback shown; the bad proxy is NOT saved.
    expect(wrapper.get('[data-testid="proxy-feedback"]').classes()).toContain("text-red-600");
    expect(invokeMock.mock.calls.some((c) => c[0] === "update_settings")).toBe(false);
  });

  it("proxy: a valid PAC source validates and patches (issue #34)", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "validate_proxy") {
        expect((args as { mode: string }).mode).toBe("pac");
        return Promise.resolve(undefined);
      }
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(NetworkPage, { global: globalMountOptions });
    await flushPromises();

    const select = wrapper.get('[data-testid="proxy-mode"]');
    await select.setValue("pac");
    await select.trigger("change");
    await flushPromises();
    const input = wrapper.get('[data-testid="proxy-url"]');
    await input.setValue("http://wpad.example/proxy.pac");
    await input.trigger("change");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("validate_proxy", {
      mode: "pac",
      url: "http://wpad.example/proxy.pac",
    });
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { global: { proxyMode: "pac", proxyUrl: "http://wpad.example/proxy.pac" } },
    });
  });
});

describe("PrivacyPage", () => {
  it("reflects telemetry default ON and toggling it calls set_telemetry_enabled (SPEC s16 R2-P1-1)", async () => {
    // M9b R2-P1-1: the "Send anonymous usage stats" toggle reflects the stored
    // telemetry.enabled (default ON) and unchecking it calls the DEDICATED
    // set_telemetry_enabled command (NOT a generic update_settings patch), so the
    // backend flips the in-flight ping cancel flag immediately (opt-out honored
    // mid-ping).
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "set_telemetry_enabled") return Promise.resolve(false);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PrivacyPage, { global: globalMountOptions });
    await flushPromises();

    const toggle = wrapper.get('[data-testid="telemetry-toggle"]');
    // Default ON: the box is checked.
    expect((toggle.element as HTMLInputElement).checked).toBe(true);
    // The privacy note is shown.
    expect(wrapper.get('[data-testid="telemetry-setting"]').text()).toContain(
      i18n.global.t("settings.rules.telemetryNote")
    );

    // Uncheck -> calls set_telemetry_enabled(false), NOT update_settings.
    await toggle.setValue(false);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("set_telemetry_enabled", {
      enabled: false,
    });
    expect(invokeMock).not.toHaveBeenCalledWith("update_settings", {
      patch: { telemetry: { enabled: false } },
    });
  });

  it("SPEC s16 preview (#34): the Preview data button opens the payload modal, even while telemetry is disabled", async () => {
    // The preview button must work regardless of the toggle state - a user
    // opts to inspect the payload BEFORE turning telemetry on.
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings")
        return Promise.resolve(
          makeSettings({ telemetry: { enabled: false, installId: "id", endpoint: "e" } })
        );
      if (cmd === "preview_telemetry_ping")
        return Promise.resolve({
          install_id: "id",
          ts: 1,
          version: "0.1.0",
          os: "windows",
          os_version: null,
          arch: "x86_64",
          channel: "stable",
          events_24h: {},
          latency_p50_p95_ms: {},
        });
      return Promise.resolve(undefined);
    });
    const wrapper = mount(PrivacyPage, { global: globalMountOptions });
    await flushPromises();

    // The modal is not rendered until opened.
    expect(wrapper.find('[data-testid="telemetry-preview-modal"]').exists()).toBe(false);

    await wrapper.get('[data-testid="telemetry-preview-open"]').trigger("click");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("preview_telemetry_ping", undefined);
    const modal = wrapper.get('[data-testid="telemetry-preview-modal"]');
    expect(modal.get('[data-testid="telemetry-preview-json"]').text()).toContain(
      '"install_id": "id"'
    );
    expect(modal.text()).toContain(i18n.global.t("telemetryPreview.caption"));

    // Closing hides it again.
    const closeBtn = modal
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.close"));
    await closeBtn!.trigger("click");
    await flushPromises();
    expect(wrapper.find('[data-testid="telemetry-preview-modal"]').exists()).toBe(false);
  });
});

describe("AdvancedPage", () => {
  it("advanced: small-file bundling renders OFF by default and toggling it patches the top-level flag", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Partial<SettingsDto> }).patch;
        return Promise.resolve({ ...makeSettings(), ...patch });
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(AdvancedPage, { global: globalMountOptions });
    await flushPromises();

    // Default OFF: the frozen v1.0.0 behaviour.
    const toggle = wrapper.get('[data-testid="bundle-small-files-toggle"]');
    expect((toggle.element as HTMLInputElement).checked).toBe(false);

    // Turning it on patches the standalone top-level flag (NOT a global-group
    // field), which the backend writes to the `bundle_small_files` KV key.
    await toggle.setValue(true);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { bundleSmallFiles: true },
    });
  });

  /// Mount the Advanced page with a settings backend that echoes patches back,
  /// and return the wrapper. Shared by the integrity-scrub control tests below.
  async function mountAdvancedPage(over: Partial<SettingsDto> = {}) {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings(over));
      if (cmd === "update_settings") {
        const patch = (args as { patch: Partial<SettingsDto> }).patch;
        return Promise.resolve({ ...makeSettings(over), ...patch });
      }
      return Promise.resolve(undefined);
    });
    const wrapper = mount(AdvancedPage, { global: globalMountOptions });
    await flushPromises();
    return wrapper;
  }

  it("integrity scrub: renders the shipped policy, on and metadata-only", async () => {
    const wrapper = await mountAdvancedPage();

    // Default ON - the scrub is the remote half of the weekly deep-verify, and
    // an integrity check nobody enables detects nothing.
    const toggle = wrapper.get('[data-testid="scrub-enabled-toggle"]');
    expect((toggle.element as HTMLInputElement).checked).toBe(true);
    // The cadence is stored in SECONDS but shown in HOURS: 604800s reads as
    // "168", which a person can reason about, rather than as a wall of digits.
    expect((wrapper.get('[data-testid="scrub-interval"]').element as HTMLInputElement).value).toBe(
      "168"
    );
    expect((wrapper.get('[data-testid="scrub-slice"]').element as HTMLInputElement).value).toBe(
      "500"
    );
    // Deep sampling ships OFF: the checksum comparison already catches
    // remote-side corruption, so spending bandwidth is opt-in.
    expect(
      (wrapper.get('[data-testid="scrub-deep-sample"]').element as HTMLInputElement).value
    ).toBe("0");
  });

  it("integrity scrub: the kill-switch patches the standalone scrub group", async () => {
    const wrapper = await mountAdvancedPage();
    await wrapper.get('[data-testid="scrub-enabled-toggle"]').setValue(false);
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { scrub: { enabled: false } },
    });
  });

  it("integrity scrub: the cadence is entered in hours and patched in seconds", async () => {
    const wrapper = await mountAdvancedPage();
    const input = wrapper.get('[data-testid="scrub-interval"]');
    await input.setValue("24");
    await input.trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { scrub: { intervalSecs: 86_400 } },
    });
  });

  it("integrity scrub: an out-of-range cadence is clamped in place, not sent", async () => {
    // The backend validator would REJECT an out-of-range value and the whole
    // page would surface an error banner; clamping in the UI keeps the form
    // usable and never sends a value the backend refuses.
    const wrapper = await mountAdvancedPage();
    const input = wrapper.get('[data-testid="scrub-interval"]');
    await input.setValue("99999999");
    await input.trigger("change");
    await flushPromises();
    // 8760 hours = 1 year, the backend's SCRUB_INTERVAL_MAX.
    expect((input.element as HTMLInputElement).value).toBe("8760");
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { scrub: { intervalSecs: 8_760 * 3600 } },
    });
  });

  it("integrity scrub: the slice size clamps to the backend bounds", async () => {
    const wrapper = await mountAdvancedPage();
    const input = wrapper.get('[data-testid="scrub-slice"]');

    await input.setValue("1");
    await input.trigger("change");
    await flushPromises();
    expect((input.element as HTMLInputElement).value).toBe("10");
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { scrub: { sliceSize: 10 } },
    });

    await input.setValue("999999");
    await input.trigger("change");
    await flushPromises();
    expect((input.element as HTMLInputElement).value).toBe("10000");
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { scrub: { sliceSize: 10_000 } },
    });
  });

  it("integrity scrub: the deep sample accepts zero and clamps its ceiling", async () => {
    const wrapper = await mountAdvancedPage();
    const input = wrapper.get('[data-testid="scrub-deep-sample"]');

    // Zero is a LEGITIMATE value here (metadata-only), unlike the slice size -
    // it must survive the clamp rather than being floored to 1.
    await input.setValue("0");
    await input.trigger("change");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { scrub: { deepSample: 0 } },
    });

    await input.setValue("5000");
    await input.trigger("change");
    await flushPromises();
    expect((input.element as HTMLInputElement).value).toBe("100");
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { scrub: { deepSample: 100 } },
    });
  });

  it("integrity scrub: an emptied field clamps to the minimum and never sends NaN", async () => {
    // `<input type="number">` coerces an unparseable entry to "", so the
    // handler sees an empty string rather than junk - `Number("")` is 0, which
    // clamps to the floor. That is the SAME behaviour every other required
    // numeric control in this form has (scan interval, hook timeout), and
    // consistency across the form is worth more than a bespoke
    // fall-back-to-persisted rule here.
    //
    // The property that actually matters, and what this test pins: a garbage
    // entry can never put `NaN` on the wire, where it would serialize as `null`
    // and fail the backend's range validator with an error banner over the
    // whole page.
    const wrapper = await mountAdvancedPage();
    const input = wrapper.get('[data-testid="scrub-slice"]');
    await input.setValue("not a number");
    await input.trigger("change");
    await flushPromises();
    expect((input.element as HTMLInputElement).value).toBe("10");
    expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      patch: { scrub: { sliceSize: 10 } },
    });
  });

  it("integrity scrub: hydrates a non-default persisted policy", async () => {
    const wrapper = await mountAdvancedPage({
      scrub: { enabled: false, intervalSecs: 86_400, sliceSize: 250, deepSample: 4 },
    });
    expect(
      (wrapper.get('[data-testid="scrub-enabled-toggle"]').element as HTMLInputElement).checked
    ).toBe(false);
    expect((wrapper.get('[data-testid="scrub-interval"]').element as HTMLInputElement).value).toBe(
      "24"
    );
    expect((wrapper.get('[data-testid="scrub-slice"]').element as HTMLInputElement).value).toBe(
      "250"
    );
    expect(
      (wrapper.get('[data-testid="scrub-deep-sample"]').element as HTMLInputElement).value
    ).toBe("4");
  });

  it("backup hooks: setting a command patches it, clearing patches null", async () => {
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "update_settings") {
        const patch = (args as { patch: Record<string, unknown> }).patch;
        return Promise.resolve(makeSettings(patch as Partial<SettingsDto>));
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(AdvancedPage, { global: globalMountOptions });
    await flushPromises();

    type GlobalPatch = { patch?: { global?: Record<string, unknown> } };
    const lastGlobalPatch = (key: string): unknown =>
      invokeMock.mock.calls
        .filter(
          (c) => c[0] === "update_settings" && key in ((c[1] as GlobalPatch).patch?.global ?? {})
        )
        .map((c) => (c[1] as GlobalPatch).patch!.global![key])
        .pop();

    // Set a pre-backup hook command.
    const pre = wrapper.get('[data-testid="pre-hook"]');
    await pre.setValue("./backup-pre.sh");
    await pre.trigger("change");
    await flushPromises();
    expect(lastGlobalPatch("preBackupHook")).toBe("./backup-pre.sh");

    // Clearing the post-hook patches null (no hook).
    const post = wrapper.get('[data-testid="post-hook"]');
    await post.setValue("   ");
    await post.trigger("change");
    await flushPromises();
    expect(lastGlobalPatch("postBackupHook")).toBeNull();
  });
});

// Task 4 (SDD 2026-08-02 settings-sidebar-ia): AccountList, SourceTable, and
// About are now routed directly (Task 3 wired the routes straight to those
// components). These wrappers give /settings/accounts, /settings/sources, and
// /settings/about a stable per-route component so the router table matches the
// other seven settings pages, which each own a page component under
// views/settings/. AccountList/SourceTable/About already render their own
// page-level heading internally (settings.accounts.title / settings.sources.title
// / about.title) - so unlike GeneralPage etc. these wrappers add no heading of
// their own; they exist purely to give the route a dedicated component and stay
// a pure pass-through to the unchanged inner component.

describe("AccountsPage", () => {
  it("renders the wrapped AccountList unchanged", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_accounts") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(AccountsPage, { global: globalMountOptions });
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("list_accounts", undefined);
    expect(wrapper.text()).toContain(i18n.global.t("settings.accounts.title"));
    expect(wrapper.find('[data-testid="accounts-empty"]').exists()).toBe(true);
  });
});

describe("SourcesPage", () => {
  it("renders the wrapped SourceTable unchanged", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "list_sources") return Promise.resolve([]);
      if (cmd === "list_accounts") return Promise.resolve([]);
      if (cmd === "list_backends") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(SourcesPage, { global: globalMountOptions });
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("list_sources", undefined);
    expect(wrapper.text()).toContain(i18n.global.t("settings.sources.title"));
    expect(wrapper.find('[data-testid="sources-empty"]').exists()).toBe(true);
  });
});

describe("AboutPage", () => {
  it("renders the wrapped About view, now identity-only (task 5)", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      if (cmd === "get_update_channel") return Promise.resolve("stable");
      if (cmd === "list_releases") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });
    const wrapper = mount(AboutPage, { global: globalMountOptions });
    await flushPromises();

    expect(wrapper.text()).toContain(i18n.global.t("about.title"));
    // About keeps its own check-for-updates action (mock F allows the action
    // in both places) but no longer renders the channel selector (moved to
    // GeneralPage) or a telemetry toggle (PrivacyPage is the single home).
    expect(wrapper.find('[data-testid="check-updates"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="channel-select"]').exists()).toBe(false);
    expect(wrapper.find('input[type="checkbox"]').exists()).toBe(false);
  });
});

describe("Settings.vue (shell + one routed Rules page)", () => {
  // Regression, updated for SDD 2026-08-02 settings-sidebar-ia task 3: the
  // seven Rules pages no longer mount stacked together (each is now its own
  // route), but the underlying race this guards against still exists in a
  // different shape - SettingsNav and the active routed page both call
  // ensureSettingsLoaded() on mount (SettingsNav needs `settings.settings`
  // for the platform item's visibility on EVERY settings route, including
  // ones - like the default /settings/accounts - whose page never loads
  // settings itself), and Vue flushes sibling onMounted hooks in one
  // synchronous pass with no microtask gap between them. Without the
  // `!settings.loading` guard in shared.ts, that fires two concurrent
  // `get_settings` round-trips racing on which response lands last in
  // `settings.errorCode`. Pin the count so a regression here shows up
  // immediately rather than only as an intermittent error-banner flake.
  it("issues exactly one get_settings call even though the sidebar and the routed page mount at once", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.resolve(makeSettings());
      return Promise.resolve(undefined);
    });

    const router = createAppRouter(createMemoryHistory());
    await router.push("/settings/general");
    await router.isReady();
    mount(Settings, { global: { plugins: [...globalMountOptions.plugins, router] } });
    await flushPromises();

    const getSettingsCalls = invokeMock.mock.calls.filter((c) => c[0] === "get_settings");
    expect(getSettingsCalls).toHaveLength(1);
  });
});
