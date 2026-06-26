// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";

// UI-VIEWS-A: the Activity filter dropdowns must explain themselves when empty
// instead of rendering a blank/confusing control. When there are zero sources
// (none added in Settings) the source <select> is disabled and shows a single
// non-selectable placeholder option; likewise the event-type <select> when no
// events have been logged yet. This mounts the real Activity.vue against faked
// IPC/event seams and asserts that behaviour in both the empty and non-empty
// cases.

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
vi.mock("vue-router", () => ({
  useRouter: () => ({ push: vi.fn() }),
  useRoute: () => ({ params: {} }),
}));

import Activity from "../views/Activity.vue";
import type { SourceDto } from "../ipc/types";

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
    deepVerifyIntervalSecs: 604800,
    lastFullScanAt: null,
    createdAt: 0,
    pendingRecoveryAck: false,
  };
}

/** Install the on-mount IPC fakes for the given source list + event-type set. */
function installInvoke(sources: SourceDto[], eventTypes: string[]): void {
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case "query_activity":
        return Promise.resolve({
          entries: [],
          total: 0,
          limit: 100,
          hasMore: false,
          nextBeforeTs: null,
          nextBeforeId: null,
        });
      case "distinct_activity_event_types":
        return Promise.resolve(eventTypes);
      case "activity_summary":
        return Promise.resolve({
          bytesToday: 0,
          bytesWeek: 0,
          fileStatusCounts: [],
          throughputWindowBytes: 0,
          throughputWindowMs: 60000,
        });
      case "list_sources":
        return Promise.resolve(sources);
      default:
        return Promise.resolve(undefined);
    }
  });
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
});

describe("Activity filter empty-dropdown placeholders (UI-VIEWS-A)", () => {
  it("disables the source + event-type selects and shows a placeholder when empty", async () => {
    installInvoke([], []);
    const wrapper = mount(Activity, { global: { plugins: [i18n] } });
    await flushPromises();

    const sourceSelect = wrapper.get('[data-testid="activity-filter-source"]');
    expect((sourceSelect.element as HTMLSelectElement).disabled).toBe(true);
    const sourceOptions = sourceSelect.findAll("option");
    expect(sourceOptions).toHaveLength(1);
    expect(sourceOptions[0].text()).toBe(i18n.global.t("activity.filters.noSourcesYet"));
    expect(sourceOptions[0].attributes("disabled")).toBeDefined();

    const eventSelect = wrapper.get('[data-testid="activity-filter-event-type"]');
    expect((eventSelect.element as HTMLSelectElement).disabled).toBe(true);
    const eventOptions = eventSelect.findAll("option");
    expect(eventOptions).toHaveLength(1);
    expect(eventOptions[0].text()).toBe(i18n.global.t("activity.filters.noEventsYet"));
    expect(eventOptions[0].attributes("disabled")).toBeDefined();

    // The level select always has fixed options, so it is never disabled.
    const levelSelect = wrapper.get('[data-testid="activity-filter-level"]');
    expect((levelSelect.element as HTMLSelectElement).disabled).toBe(false);

    wrapper.unmount();
  });

  it("enables the selects and drops the placeholder once sources + events exist", async () => {
    installInvoke([source("src-1", "Docs")], ["upload_done"]);
    const wrapper = mount(Activity, { global: { plugins: [i18n] } });
    await flushPromises();

    const sourceSelect = wrapper.get('[data-testid="activity-filter-source"]');
    expect((sourceSelect.element as HTMLSelectElement).disabled).toBe(false);
    const sourceOptions = sourceSelect.findAll("option");
    // "All sources" + the one real source.
    expect(sourceOptions.map((o) => o.element.value)).toEqual(["", "src-1"]);
    expect(sourceOptions.every((o) => o.attributes("disabled") === undefined)).toBe(true);

    const eventSelect = wrapper.get('[data-testid="activity-filter-event-type"]');
    expect((eventSelect.element as HTMLSelectElement).disabled).toBe(false);
    const eventOptions = eventSelect.findAll("option");
    expect(eventOptions.map((o) => o.element.value)).toEqual(["", "upload_done"]);

    wrapper.unmount();
  });
});
