// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";

// UI-VIEWS-A: the Restore source <select> must explain itself when there are no
// backup sources to browse, instead of rendering a blank dropdown. With zero
// sources the select is disabled and shows a single non-selectable placeholder
// option ("No sources yet - add one in Settings"); once a source exists it is
// enabled and lists the real sources. This mounts the real Restore.vue against
// faked IPC/event seams and asserts both cases.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => undefined),
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: vi.fn(),
  save: vi.fn(),
}));

import Restore from "../views/Restore.vue";
import type { RemoteEntryDto, SourceDto } from "../ipc/types";

function source(id: string, name: string): SourceDto {
  return {
    id,
    accountId: "acct-1",
    displayName: name,
    enabled: true,
    localPath: "/home/u/" + name,
    driveFolderId: "drive-" + id,
    driveFolderPath: name,
    encryptionEnabled: false,
    respectGitignore: true,
    includePatterns: [],
    excludePatterns: [],
    placeholderPolicy: "skip",
    deepVerifyIntervalSecs: 604800,
    lastFullScanAt: null,
    createdAt: 0,
    pendingRecoveryAck: false,
  };
}

/** Install the on-mount IPC fakes for the given source list. A non-empty list
 * triggers an auto-select -> list_remote_tree, which we resolve to one folder. */
function installInvoke(sources: SourceDto[]): void {
  const folder: RemoteEntryDto = {
    relativePath: "photos",
    name: "photos",
    isDir: true,
    size: 0,
    status: null,
    restorable: false,
  };
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case "list_sources":
        return Promise.resolve(sources);
      case "list_remote_tree":
        return Promise.resolve({ entries: [folder], truncated: false });
      case "get_restore_job":
        return Promise.resolve(undefined);
      default:
        return Promise.resolve(undefined);
    }
  });
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
});

describe("Restore source empty-dropdown placeholder (UI-VIEWS-A)", () => {
  it("disables the source select and shows a placeholder when there are no sources", async () => {
    installInvoke([]);
    const wrapper = mount(Restore, { global: { plugins: [i18n] } });
    await flushPromises();

    const select = wrapper.get('[data-testid="restore-source"]');
    expect((select.element as HTMLSelectElement).disabled).toBe(true);
    const options = select.findAll("option");
    expect(options).toHaveLength(1);
    expect(options[0].text()).toBe(i18n.global.t("restore.noSourcesYet"));
    expect(options[0].attributes("disabled")).toBeDefined();

    // The search box is also disabled while there is nothing to search.
    const searchInput = wrapper.get('[data-testid="restore-search-input"]');
    expect((searchInput.element as HTMLInputElement).disabled).toBe(true);

    // The empty-state panel explains there are no sources rather than implying
    // an empty folder.
    expect(wrapper.get('[data-testid="restore-empty"]').text()).toBe(
      i18n.global.t("restore.empty.noSources")
    );

    wrapper.unmount();
  });

  it("enables the source select and lists real sources once one exists", async () => {
    installInvoke([source("src-1", "Docs"), source("src-2", "Photos")]);
    const wrapper = mount(Restore, { global: { plugins: [i18n] } });
    await flushPromises();

    const select = wrapper.get('[data-testid="restore-source"]');
    expect((select.element as HTMLSelectElement).disabled).toBe(false);
    const options = select.findAll("option");
    expect(options.map((o) => o.element.value)).toEqual(["src-1", "src-2"]);
    expect(options.every((o) => o.attributes("disabled") === undefined)).toBe(true);

    const searchInput = wrapper.get('[data-testid="restore-search-input"]');
    expect((searchInput.element as HTMLInputElement).disabled).toBe(false);

    wrapper.unmount();
  });
});
