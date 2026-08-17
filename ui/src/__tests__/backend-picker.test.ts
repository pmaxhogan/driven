// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount, flushPromises } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

// Pluggable backup destinations: the setup wizard's destination picker renders
// the destinations the RUST FACTORY reports (never a hard-coded UI list),
// preselects Google Drive, and publishes the user's choice through its
// `selected` v-model. The setup store then stamps that choice onto
// `begin_add_account_wizard`, which is what persists it on the account row.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

import { i18n } from "../i18n";
import BackendPicker from "../components/BackendPicker.vue";
import { useSetupStore } from "../stores/setup";
import type { BackendDto } from "../ipc/types";

const DRIVE: BackendDto = {
  id: "google_drive",
  usesOauth: true,
  supportsFolderPicker: true,
  supportsVersionHistory: true,
  supportsRename: true,
  isDefault: true,
};

/** A second, hypothetical destination. The picker must be driven entirely by
 * the descriptor list, so an id it has never heard of still renders (falling
 * back to the raw id when no i18n strings are seeded for it). */
const OTHER: BackendDto = {
  id: "some_other_store",
  usesOauth: false,
  supportsFolderPicker: false,
  supportsVersionHistory: false,
  supportsRename: false,
  isDefault: false,
};

function mountPicker(backends: BackendDto[], selected = "google_drive") {
  return mount(BackendPicker, {
    props: {
      backends,
      selected,
      "onUpdate:selected": (v: string) => {
        void v;
      },
    },
    global: { plugins: [i18n] },
  });
}

describe("BackendPicker", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    setActivePinia(createPinia());
  });

  it("renders one option per advertised destination and marks the selected one", async () => {
    const wrapper = mountPicker([DRIVE, OTHER]);
    await flushPromises();

    expect(wrapper.find('[data-testid="backend-picker"]').exists()).toBe(true);
    const options = wrapper.findAll('[data-testid^="backend-option-"]');
    expect(options).toHaveLength(2);

    // Google Drive is preselected and gets its seeded human label.
    const drive = wrapper.get('[data-testid="backend-option-google_drive"]');
    expect(drive.text()).toContain(i18n.global.t("backendPicker.kind.google_drive.name"));
    expect(drive.get("input").element.checked).toBe(true);

    // The unknown destination still renders, labelled by its raw id.
    const other = wrapper.get('[data-testid="backend-option-some_other_store"]');
    expect(other.text()).toContain("some_other_store");
    expect(other.get("input").element.checked).toBe(false);
  });

  it("publishes the chosen destination through the selected v-model", async () => {
    const wrapper = mountPicker([DRIVE, OTHER]);
    await flushPromises();

    await wrapper.get('[data-testid="backend-option-some_other_store"] input').setValue(true);

    expect(wrapper.emitted("update:selected")).toEqual([["some_other_store"]]);
  });

  it("asks the parent to load descriptors when it mounts with none", async () => {
    const wrapper = mountPicker([]);
    await flushPromises();

    expect(wrapper.emitted("load")).toHaveLength(1);
    // With nothing to offer it says so rather than rendering an empty control.
    expect(wrapper.find('[data-testid="backend-picker-empty"]').exists()).toBe(true);
  });

  it("does not re-request descriptors it already has", async () => {
    const wrapper = mountPicker([DRIVE]);
    await flushPromises();
    expect(wrapper.emitted("load")).toBeUndefined();
  });
});

describe("setup store destination selection", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    setActivePinia(createPinia());
  });

  it("defaults to Google Drive before the descriptor list resolves", () => {
    const setup = useSetupStore();
    expect(setup.backendId).toBe("google_drive");
    // Unknown-until-loaded must not hide the OAuth credentials step.
    expect(setup.backendUsesOauth).toBe(true);
  });

  it("loads descriptors and keeps the advertised default selected", async () => {
    invokeMock.mockResolvedValueOnce([DRIVE, OTHER]);
    const setup = useSetupStore();
    await setup.loadBackends();

    expect(invokeMock).toHaveBeenCalledWith("list_backends", undefined);
    expect(setup.backends).toHaveLength(2);
    expect(setup.backendId).toBe("google_drive");
    expect(setup.selectedBackend?.usesOauth).toBe(true);
  });

  it("falls back to the advertised default when the current choice is not offered", async () => {
    const setup = useSetupStore();
    setup.selectBackend("some_other_store");
    invokeMock.mockResolvedValueOnce([DRIVE]);
    await setup.loadBackends();

    expect(setup.backendId).toBe("google_drive");
  });

  it("stamps the chosen destination onto begin_add_account_wizard", async () => {
    invokeMock.mockResolvedValueOnce([DRIVE, OTHER]);
    const setup = useSetupStore();
    await setup.loadBackends();
    setup.selectBackend("some_other_store");

    invokeMock.mockResolvedValueOnce("session-1");
    await setup.begin();

    expect(invokeMock).toHaveBeenLastCalledWith("begin_add_account_wizard", {
      backend: "some_other_store",
    });
    expect(setup.session).toBe("session-1");
  });

  it("surfaces a descriptor-load failure as an error code without dead-ending", async () => {
    invokeMock.mockRejectedValueOnce({ code: "internal.bug", message: "boom" });
    const setup = useSetupStore();
    await setup.loadBackends();

    expect(setup.errorCode).toBe("internal.bug");
    // The wizard still runs against the historical default.
    expect(setup.backendId).toBe("google_drive");
  });

  it("reverts the choice to the default on reset but keeps the build's list", async () => {
    invokeMock.mockResolvedValueOnce([DRIVE, OTHER]);
    const setup = useSetupStore();
    await setup.loadBackends();
    setup.selectBackend("some_other_store");

    setup.reset();

    expect(setup.backendId).toBe("google_drive");
    expect(setup.backends).toHaveLength(2);
  });
});
