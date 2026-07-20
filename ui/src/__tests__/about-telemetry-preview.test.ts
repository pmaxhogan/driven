// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { setActivePinia, createPinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

// About.vue telemetry preview (SPEC s16 preview; #34). About.vue has its own
// Privacy section (telemetry toggle + install id) separate from the
// Settings-tab one, so it gets the same "Preview data" link, reusing
// TelemetryPreviewModal and its localized strings. This mirrors the mount
// test added for Settings.vue's button.

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
import { useSettingsStore } from "../stores/settings";

function mountAbout() {
  return mount(About, {
    global: { plugins: [i18n] },
  });
}

const previewPayload = {
  install_id: "9f8e7d6c-5b4a-4392-8170-0a1b2c3d4e5f",
  ts: 1_700_000_000_000,
  version: "0.1.0",
  os: "windows",
  os_version: "11.26200",
  arch: "x86_64",
  channel: "stable",
  events_24h: { files_uploaded: 0, bytes_uploaded: 0, errors_by_class: {} },
  latency_p50_p95_ms: { scan: [], upload_per_mb: [] },
};

beforeEach(() => {
  invokeMock.mockReset();
  setActivePinia(createPinia());
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case "get_settings":
        return Promise.resolve({ telemetry: { enabled: false } });
      case "get_update_channel":
        return Promise.resolve("stable");
      case "list_releases":
        return Promise.resolve([]);
      case "preview_telemetry_ping":
        return Promise.resolve(previewPayload);
      default:
        return Promise.resolve(null);
    }
  });
});

describe("About.vue telemetry preview (#34)", () => {
  it("opens the preview modal from the Privacy section even while telemetry is OFF", async () => {
    const settings = useSettingsStore();
    settings.settings = { telemetry: { enabled: false } } as never;
    const wrapper = mountAbout();
    await flushPromises();

    // The toggle reflects the disabled state, but the preview link is present
    // and usable regardless.
    const toggle = wrapper.get('input[type="checkbox"]');
    expect((toggle.element as HTMLInputElement).checked).toBe(false);
    expect(wrapper.find('[data-testid="telemetry-preview-modal"]').exists()).toBe(false);

    await wrapper.get('[data-testid="telemetry-preview-open"]').trigger("click");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("preview_telemetry_ping", undefined);
    const modal = wrapper.get('[data-testid="telemetry-preview-modal"]');
    expect(modal.get('[data-testid="telemetry-preview-json"]').text()).toContain(
      '"install_id": "9f8e7d6c-5b4a-4392-8170-0a1b2c3d4e5f"'
    );
    expect(modal.text()).toContain(i18n.global.t("telemetryPreview.caption"));

    const closeBtn = modal
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.close"));
    await closeBtn!.trigger("click");
    await flushPromises();
    expect(wrapper.find('[data-testid="telemetry-preview-modal"]').exists()).toBe(false);
  });
});
