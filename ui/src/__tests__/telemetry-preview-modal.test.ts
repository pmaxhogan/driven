// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";
import TelemetryPreviewModal from "../components/TelemetryPreviewModal.vue";

// TelemetryPreviewModal tests (SPEC s16 preview; #34). The modal fetches the
// EXACT next-ping JSON payload via `preview_telemetry_ping` when opened and
// pretty-prints it - no other IPC command is called (in particular NOT
// `set_telemetry_enabled` or anything network-shaped), matching the no-side-
// effect guarantee the backend provides.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

const globalMountOptions = { plugins: [i18n] };

beforeEach(() => {
  invokeMock.mockReset();
});

describe("TelemetryPreviewModal", () => {
  it("is hidden when closed and fetches nothing", () => {
    const wrapper = mount(TelemetryPreviewModal, {
      props: { open: false },
      global: globalMountOptions,
    });
    expect(wrapper.find('[data-testid="telemetry-preview-modal"]').exists()).toBe(false);
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("fetches and pretty-prints the preview payload when opened", async () => {
    const payload = {
      install_id: "9f8e7d6c-5b4a-4392-8170-0a1b2c3d4e5f",
      ts: 1_700_000_000_000,
      version: "0.1.0",
      os: "windows",
      os_version: "11.26200",
      arch: "x86_64",
      channel: "stable",
      events_24h: { files_uploaded: 3, bytes_uploaded: 100, errors_by_class: {} },
      latency_p50_p95_ms: { scan: [], upload_per_mb: [] },
    };
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "preview_telemetry_ping") return Promise.resolve(payload);
      return Promise.resolve(undefined);
    });

    const wrapper = mount(TelemetryPreviewModal, {
      props: { open: true },
      global: globalMountOptions,
    });
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("preview_telemetry_ping", undefined);
    // Only the preview command fired - no network-shaped send, no toggle call.
    expect(invokeMock).not.toHaveBeenCalledWith("set_telemetry_enabled", expect.anything());

    const json = wrapper.get('[data-testid="telemetry-preview-json"]');
    expect(json.text()).toContain('"install_id": "9f8e7d6c-5b4a-4392-8170-0a1b2c3d4e5f"');
    expect(json.text()).toContain('"files_uploaded": 3');

    // The no-side-effect caption is shown.
    expect(wrapper.get('[data-testid="telemetry-preview-caption"]').text()).toBe(
      i18n.global.t("telemetryPreview.caption")
    );
  });

  it("shows a loading state, then an error message if the fetch rejects", async () => {
    let resolvePreview: (() => void) | null = null;
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "preview_telemetry_ping") {
        return new Promise((_resolve, reject) => {
          resolvePreview = () => reject(new Error("db unavailable"));
        });
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(TelemetryPreviewModal, {
      props: { open: true },
      global: globalMountOptions,
    });
    await flushPromises();
    expect(wrapper.find('[data-testid="telemetry-preview-loading"]').exists()).toBe(true);

    resolvePreview!();
    await flushPromises();
    expect(wrapper.find('[data-testid="telemetry-preview-loading"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="telemetry-preview-error"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="telemetry-preview-json"]').exists()).toBe(false);
  });

  it("re-fetches a fresh payload each time the modal is reopened", async () => {
    let calls = 0;
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "preview_telemetry_ping") {
        calls += 1;
        return Promise.resolve({ install_id: `id-${calls}`, ts: calls });
      }
      return Promise.resolve(undefined);
    });

    const wrapper = mount(TelemetryPreviewModal, {
      props: { open: false },
      global: globalMountOptions,
    });
    await wrapper.setProps({ open: true });
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledTimes(1);

    await wrapper.setProps({ open: false });
    await wrapper.setProps({ open: true });
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledTimes(2);
  });

  it("emits close from both the close button and the overlay click", async () => {
    invokeMock.mockResolvedValue({ install_id: "id" });
    const wrapper = mount(TelemetryPreviewModal, {
      props: { open: true },
      global: globalMountOptions,
    });
    await flushPromises();

    const closeBtn = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("common.close"));
    await closeBtn!.trigger("click");
    expect(wrapper.emitted("close")).toBeTruthy();

    await wrapper.get('[data-testid="telemetry-preview-modal"]').trigger("click");
    expect(wrapper.emitted("close")?.length).toBe(2);
  });

  it("copies the pretty-printed payload to the clipboard", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    invokeMock.mockResolvedValue({ install_id: "id", ts: 1 });

    const wrapper = mount(TelemetryPreviewModal, {
      props: { open: true },
      global: globalMountOptions,
    });
    await flushPromises();

    const copyBtn = wrapper
      .findAll("button")
      .find((b) => b.text() === i18n.global.t("telemetryPreview.copyButton"));
    expect(copyBtn).toBeTruthy();
    await copyBtn!.trigger("click");
    await flushPromises();

    expect(writeText).toHaveBeenCalledWith(JSON.stringify({ install_id: "id", ts: 1 }, null, 2));
    expect(wrapper.text()).toContain(i18n.global.t("telemetryPreview.copiedButton"));
  });
});
