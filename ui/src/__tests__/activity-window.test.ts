// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";
import { nextTick } from "vue";

import { i18n } from "../i18n";
import { useActivityStore, ACTIVITY_RENDER_WINDOW } from "../stores/activity";
import type { ActivityEntry } from "../ipc/types";

// Issue #45: the Activity table must NOT mount one row per accumulated entry -
// a high-rate upload can buffer ~1000 live rows, and rendering them all makes the
// page janky while scrolling. The view renders only the newest
// `ACTIVITY_RENDER_WINDOW` rows and grows the window on demand. This mounts the
// real Activity.vue against faked IPC/event seams, floods the live tail past the
// window, and asserts the MOUNTED row count stays bounded while the store holds
// strictly more entries - then that "load more" reveals the next window.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));

// Capture the `activity:new` handler so the test can fire a live burst.
let liveHandler: ((payload: unknown) => void) | null = null;
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (event: string, cb: (e: { payload: unknown }) => void) => {
    if (event === "activity:new") {
      liveHandler = (payload: unknown) => cb({ payload });
    }
    return () => undefined;
  }),
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

function makeEntry(over: Partial<ActivityEntry> = {}): ActivityEntry {
  return {
    id: 1,
    ts: 1000,
    sourceId: null,
    level: "info",
    eventType: "upload_done",
    fileCount: null,
    bytes: null,
    message: null,
    ...over,
  };
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  liveHandler = null;
  // Every on-mount IPC call resolves to a benign empty shape (no history rows).
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
        return Promise.resolve(["upload_done"]);
      case "activity_summary":
        return Promise.resolve({
          bytesToday: 0,
          bytesWeek: 0,
          fileStatusCounts: [],
          throughputWindowBytes: 0,
          throughputWindowMs: 60000,
        });
      case "list_sources":
        return Promise.resolve([]);
      default:
        return Promise.resolve(undefined);
    }
  });
});

describe("Activity render window (issue #45)", () => {
  it("bounds the mounted rows to the render window even with a much larger live tail", async () => {
    const wrapper = mount(Activity, { global: { plugins: [i18n] } });
    await flushPromises();

    const store = useActivityStore();

    // Flood the live tail with more than one render window of events.
    const burst = ACTIVITY_RENDER_WINDOW + 90;
    for (let i = 1; i <= burst; i++) {
      liveHandler?.(makeEntry({ id: i, ts: i }));
    }
    // Apply the coalesced burst, then let the view re-render.
    store.flushLive();
    await nextTick();

    // The store holds the whole tail...
    expect(store.entries.length).toBe(burst);
    // ...but the DOM only mounts the newest ACTIVITY_RENDER_WINDOW rows.
    const rows = wrapper.findAll('[data-testid="activity-row"]');
    expect(rows).toHaveLength(ACTIVITY_RENDER_WINDOW);

    // "Load more" is offered because more buffered rows exist than are rendered.
    const loadMore = wrapper.find('[data-testid="activity-load-more"]');
    expect(loadMore.exists()).toBe(true);

    // Revealing the next window grows the mounted rows by one window (capped at
    // the number of entries actually held).
    await loadMore.trigger("click");
    await nextTick();
    const rowsAfter = wrapper.findAll('[data-testid="activity-row"]');
    expect(rowsAfter.length).toBe(Math.min(burst, ACTIVITY_RENDER_WINDOW * 2));
    expect(rowsAfter.length).toBeGreaterThan(ACTIVITY_RENDER_WINDOW);

    wrapper.unmount();
  });

  it("renders all rows (no window button) when the tail fits in the window", async () => {
    const wrapper = mount(Activity, { global: { plugins: [i18n] } });
    await flushPromises();

    const store = useActivityStore();
    const fits = ACTIVITY_RENDER_WINDOW - 5;
    for (let i = 1; i <= fits; i++) {
      liveHandler?.(makeEntry({ id: i, ts: i }));
    }
    store.flushLive();
    await nextTick();

    expect(store.entries.length).toBe(fits);
    const rows = wrapper.findAll('[data-testid="activity-row"]');
    expect(rows).toHaveLength(fits);
    // Nothing more to reveal and no more history -> no "load more".
    expect(wrapper.find('[data-testid="activity-load-more"]').exists()).toBe(false);

    wrapper.unmount();
  });
});
