// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";
import { useActivityStore } from "../stores/activity";

// R2-P2-4: the event-type FILTER dropdown must render LOCALIZED labels (via the
// shared activityEventLabel helper) in the option text while keeping the raw
// backend code as the option value + title - matching the localized table path.
// This mounts the real Activity.vue against faked IPC/event seams and asserts
// the rendered option text is the localized label, not the raw code.

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

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  // Every IPC call the view fires on mount resolves to a benign empty shape.
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
        return Promise.resolve(["upload_done", "some.unknown_code"]);
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

describe("Activity event-type filter dropdown (R2-P2-4)", () => {
  it("renders LOCALIZED option labels with the raw code as value + title", async () => {
    const wrapper = mount(Activity, {
      global: { plugins: [i18n] },
    });
    // Let onMounted IPC (loadEventTypeOptions etc.) settle.
    await flushPromises();

    // Sanity: the store holds the backend codes.
    const store = useActivityStore();
    expect(store.eventTypeOptions).toEqual(["upload_done", "some.unknown_code"]);

    // The known code localizes to its activity.events label ("Uploaded"),
    // NOT the raw "upload_done"; the value + title keep the raw code.
    const options = wrapper.findAll("option");
    const uploadOption = options.find((o) => o.element.value === "upload_done");
    expect(uploadOption).toBeTruthy();
    expect(uploadOption!.text()).toBe("Uploaded");
    expect(uploadOption!.text()).not.toBe("upload_done");
    expect(uploadOption!.attributes("title")).toBe("upload_done");

    // An unknown code falls back to the raw code (the helper's safe fallback),
    // still carried as value + title.
    const unknownOption = options.find((o) => o.element.value === "some.unknown_code");
    expect(unknownOption).toBeTruthy();
    expect(unknownOption!.text()).toBe("some.unknown_code");
    expect(unknownOption!.attributes("title")).toBe("some.unknown_code");

    wrapper.unmount();
  });
});
