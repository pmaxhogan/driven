// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { mount, flushPromises } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

// PausedBanner tests. The banner is a pure render of the pause store plus its
// own one-second countdown tick, so these mount the real component and drive it
// through the store: hidden when unpaused, the timed vs indefinite copy, the
// live countdown, and the Resume button's success + failure paths. The only
// backend seam is `invoke` (resume_sync), mocked as in the store test.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => vi.fn()),
}));

import { i18n } from "../i18n";
import PausedBanner from "../components/PausedBanner.vue";
import { usePauseStore } from "../stores/pause";

const BANNER = '[data-testid="paused-banner"]';
const RESUME = '[data-testid="paused-banner-resume"]';
const ERROR = '[data-testid="paused-banner-error"]';

function mountBanner() {
  const pinia = createPinia();
  setActivePinia(pinia);
  const store = usePauseStore();
  const wrapper = mount(PausedBanner, { global: { plugins: [pinia, i18n] } });
  return { store, wrapper };
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("PausedBanner", () => {
  it("renders nothing while sync is not paused", async () => {
    const { wrapper } = mountBanner();
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).exists()).toBe(false);
  });

  it("shows the indefinite copy for a pause-until-I-resume", async () => {
    const { store, wrapper } = mountBanner();
    store.ingest({ kind: "indefinite" });
    await wrapper.vm.$nextTick();

    const banner = wrapper.find(BANNER);
    expect(banner.exists()).toBe(true);
    expect(banner.text()).toContain("Backups paused indefinitely");
    expect(banner.attributes("role")).toBe("status");
  });

  it("shows the minutes left for a timed pause", async () => {
    const { store, wrapper } = mountBanner();
    store.ingest({ kind: "timed", until_ms: Date.now() + 27 * 60_000 });
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).text()).toContain("Backups paused - 27m left");
  });

  it("counts the timed pause down on its one-second tick", async () => {
    vi.useFakeTimers();
    const { store, wrapper } = mountBanner();
    const until = Date.now() + 30 * 60_000;
    store.ingest({ kind: "timed", until_ms: until });
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).text()).toContain("30m left");

    // Advance real wall-clock time by 5 minutes, then let the component's
    // interval fire so it re-reads the clock.
    vi.setSystemTime(Date.now() + 5 * 60_000);
    await vi.advanceTimersByTimeAsync(1_000);
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).text()).toContain("25m left");
  });

  it("hides once the pause clears", async () => {
    const { store, wrapper } = mountBanner();
    store.ingest({ kind: "indefinite" });
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).exists()).toBe(true);

    store.ingest(null);
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).exists()).toBe(false);
  });

  it("resumes on a single click of the Resume button", async () => {
    const { store, wrapper } = mountBanner();
    store.ingest({ kind: "timed", until_ms: Date.now() + 10 * 60_000 });
    await wrapper.vm.$nextTick();

    await wrapper.find(RESUME).trigger("click");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("resume_sync", undefined);
    expect(wrapper.find(BANNER).exists()).toBe(false);
  });

  it("keeps the banner and surfaces the error when resuming fails", async () => {
    invokeMock.mockRejectedValue(new Error("resume blew up"));
    const { store, wrapper } = mountBanner();
    store.ingest({ kind: "indefinite" });
    await wrapper.vm.$nextTick();

    await wrapper.find(RESUME).trigger("click");
    await flushPromises();

    // Backups are still paused, so the banner must not disappear.
    expect(wrapper.find(BANNER).exists()).toBe(true);
    expect(wrapper.find(ERROR).text()).toContain("resume blew up");
    // ...and the button is usable again for a retry.
    expect(wrapper.find(RESUME).attributes("disabled")).toBeUndefined();
  });
});
