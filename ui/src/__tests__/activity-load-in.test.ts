// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";

// Feature B: switching to the Activity tab used to flash - an empty header, an
// empty filter bar and "Showing 0 of 0" painted first, then each swapped out as
// its query landed. The view now holds a skeleton of its own shape until the
// first load settles, then fades the real content in.
//
// This also covers the files-uploaded tile's presence in the view (Feature A):
// it must appear right after the throughput tile, fed from the store's files
// series and the summary's window file count.

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

const SUMMARY = '[data-testid="activity-summary"]';
const SUMMARY_SKELETON = '[data-testid="activity-summary-skeleton"]';
const FILTERS = '[data-testid="activity-filters"]';
const FILTERS_SKELETON = '[data-testid="activity-filters-skeleton"]';
const TABLE_SKELETON = '[data-testid="activity-table-skeleton"]';
const COUNT = '[data-testid="activity-count"]';
const FILES_TILE = '[data-testid="files-uploaded-tile"]';
const FILES_VALUE = '[data-testid="files-uploaded-value"]';
const FILES_SPARKLINE = '[data-testid="files-uploaded-sparkline"]';
const THROUGHPUT_TILE = '[data-testid="throughput-tile"]';

/** Resolves every on-mount command. `gate`, when supplied, is awaited before the
 * first history page resolves, so the test can inspect the still-loading view. */
function stubBackend(gate?: Promise<void>): void {
  invokeMock.mockImplementation(async (cmd: string) => {
    switch (cmd) {
      case "query_activity":
        if (gate) await gate;
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
          bytesToday: 4096,
          bytesWeek: 8192,
          fileStatusCounts: [],
          throughputWindowBytes: 60_000,
          throughputWindowFiles: 12,
          throughputWindowMs: 60_000,
        };
      case "activity_throughput_series":
        return { bytes: [0, 1024, 2048], files: [0, 1, 4] };
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

describe("Activity load-in (Feature B)", () => {
  it("shows a skeleton of the page's shape while the first load is in flight", async () => {
    let release = (): void => undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    stubBackend(gate);

    const wrapper = mount(Activity, { global: { plugins: [i18n] } });
    await flushPromises();

    // Still loading: skeletons stand in for the tiles, the filter bar and the
    // table, and none of the real content (which would be wrong-and-then-right)
    // is painted yet.
    expect(wrapper.find(SUMMARY_SKELETON).exists()).toBe(true);
    expect(wrapper.find(FILTERS_SKELETON).exists()).toBe(true);
    expect(wrapper.find(TABLE_SKELETON).exists()).toBe(true);
    expect(wrapper.find(SUMMARY).exists()).toBe(false);
    expect(wrapper.find(FILTERS).exists()).toBe(false);
    expect(wrapper.find(COUNT).exists()).toBe(false);
    // The section announces the in-flight load instead of the skeleton bars
    // being read out as content.
    expect(wrapper.find("section").attributes("aria-busy")).toBe("true");
    expect(wrapper.find(SUMMARY_SKELETON).attributes("aria-hidden")).toBe("true");

    release();
    await flushPromises();

    // Settled: the skeletons are gone and the real content is in, carrying the
    // one-shot load-in animation class.
    expect(wrapper.find(SUMMARY_SKELETON).exists()).toBe(false);
    expect(wrapper.find(FILTERS_SKELETON).exists()).toBe(false);
    expect(wrapper.find(TABLE_SKELETON).exists()).toBe(false);
    expect(wrapper.find(SUMMARY).exists()).toBe(true);
    expect(wrapper.find(FILTERS).exists()).toBe(true);
    expect(wrapper.find(COUNT).exists()).toBe(true);
    expect(wrapper.find(SUMMARY).classes()).toContain("activity-load-in");
    expect(wrapper.find("section").attributes("aria-busy")).toBe("false");

    wrapper.unmount();
  });

  it("retires the skeleton even when the first load fails", async () => {
    // A failed load must not leave the view pulsing forever - it has to fall
    // through to the error / empty state like any other settled load.
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "query_activity") throw new Error("backend gone");
      if (cmd === "list_sources") return [];
      return undefined;
    });

    const wrapper = mount(Activity, { global: { plugins: [i18n] } });
    await flushPromises();

    expect(wrapper.find(SUMMARY_SKELETON).exists()).toBe(false);
    expect(wrapper.find(TABLE_SKELETON).exists()).toBe(false);
    expect(wrapper.find('[data-testid="activity-error"]').exists()).toBe(true);

    wrapper.unmount();
  });
});

describe("Activity files-uploaded tile (Feature A)", () => {
  it("renders the files tile right after the throughput tile", async () => {
    stubBackend();
    const wrapper = mount(Activity, { global: { plugins: [i18n] } });
    await flushPromises();

    const tiles = Array.from(wrapper.find(SUMMARY).element.children);
    const throughputAt = tiles.findIndex((el) => el.matches(THROUGHPUT_TILE));
    const filesAt = tiles.findIndex((el) => el.matches(FILES_TILE));
    const diskAt = tiles.findIndex((el) => el.matches('[data-testid="disk-throughput-tile"]'));
    expect(throughputAt).toBeGreaterThanOrEqual(0);
    // 2026-08-14 follow-up: the disk tile sits between the network tile and
    // the files tile.
    expect(diskAt).toBe(throughputAt + 1);
    expect(filesAt).toBe(diskAt + 1);

    wrapper.unmount();
  });

  it("headlines the window's file count and plots the store's files series", async () => {
    stubBackend();
    const wrapper = mount(Activity, { global: { plugins: [i18n] } });
    await flushPromises();

    // The summary's throughputWindowFiles, not a count derived from the chart.
    expect(wrapper.find(FILES_VALUE).text()).toBe("12 files");
    // The files series [0, 1, 4] has a shape, so the sparkline is drawn.
    expect(wrapper.find(FILES_SPARKLINE).exists()).toBe(true);

    wrapper.unmount();
  });
});
