// @vitest-environment jsdom
import { describe, it, expect, beforeEach, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

// FdaBanner tests: the wiring half (a BACKEND `activity:new` row carrying
// `local.permission_denied` raises the banner) and the behaviour half (the
// per-file dedupe that stops a re-scanned denial inflating the count every
// cycle, the exact System Settings deep link, and the per-session dismiss).
//
// Two seams are mocked, both the way the existing tests mock them:
// `@tauri-apps/api/event`'s `listen` is kept per channel so a test can play an
// event into the mounted component exactly as Tauri would (toast-host.test.ts),
// and `@tauri-apps/plugin-opener`'s `openUrl` is stubbed so asserting the deep
// link never launches a real System Settings window (setup-wizard.test.ts).

const handlers = new Map<string, (event: { payload: unknown }) => void>();
const unlisten = vi.fn();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async () => undefined),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, handler: (event: { payload: unknown }) => void) => {
    handlers.set(name, handler);
    return unlisten;
  }),
}));

const openUrlMock = vi.fn();
vi.mock("@tauri-apps/plugin-opener", () => ({
  openUrl: (url: string) => openUrlMock(url),
}));

import { i18n } from "../i18n";
import FdaBanner from "../components/FdaBanner.vue";
import { useFdaBannerStore } from "../stores/fdaBanner";

const BANNER = '[data-testid="fda-banner"]';
const OPEN = '[data-testid="fda-banner-open"]';
const DISMISS = '[data-testid="fda-banner-dismiss"]';
const UNSIGNED_NOTE = '[data-testid="fda-banner-unsigned-note"]';

/** The one string this whole feature exists to deliver. A wrong anchor lands the
 * user on the Privacy & Security root instead of Full Disk Access, which looks
 * identical in a screenshot - hence asserting it literally. */
const FDA_URL = "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

/** One activity row, with the fields ActivityEntry declares. */
function row(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    id: 1,
    ts: 1_700_000_000_000,
    sourceId: "src-1",
    level: "warn",
    eventType: "local.permission_denied",
    fileCount: null,
    bytes: null,
    message: "/Users/me/Documents/taxes.pdf",
    ...overrides,
  };
}

/** Play an `activity:new` event carrying one activity row. */
function emitActivity(entry: Record<string, unknown>): void {
  handlers.get("activity:new")?.({ payload: entry });
}

async function mountBanner() {
  const pinia = createPinia();
  setActivePinia(pinia);
  const store = useFdaBannerStore();
  const wrapper = mount(FdaBanner, { global: { plugins: [pinia, i18n] } });
  // Let the onMounted `listen` call resolve so the handler is registered before
  // a test plays an event.
  await vi.waitFor(() => expect(handlers.has("activity:new")).toBe(true));
  return { store, wrapper };
}

beforeEach(() => {
  handlers.clear();
  unlisten.mockClear();
  openUrlMock.mockClear();
  openUrlMock.mockResolvedValue(undefined);
});

describe("FdaBanner", () => {
  it("shows nothing until macOS has actually refused a read", async () => {
    const { wrapper } = await mountBanner();
    expect(wrapper.find(BANNER).exists()).toBe(false);
  });

  it("appears when a local.permission_denied row arrives", async () => {
    const { wrapper } = await mountBanner();

    emitActivity(row());
    await wrapper.vm.$nextTick();

    const banner = wrapper.find(BANNER);
    expect(banner.exists()).toBe(true);
    expect(banner.text()).toContain("macOS is blocking Driven from reading some files");
  });

  it("ignores activity rows that are not permission denials", async () => {
    const { store, wrapper } = await mountBanner();

    emitActivity(row({ id: 2, eventType: "backup_done", message: null }));
    emitActivity(row({ id: 3, eventType: "local.file_locked" }));
    await wrapper.vm.$nextTick();

    expect(store.deniedCount).toBe(0);
    expect(wrapper.find(BANNER).exists()).toBe(false);
  });

  // A TCC denial is permanent, so the SAME file is re-denied and re-logged on
  // every scan. Without the dedupe the banner would read "150 files" for one
  // file after a day of cycles.
  it("counts a file re-denied on every cycle exactly once", async () => {
    const { store, wrapper } = await mountBanner();

    for (const id of [10, 11, 12]) {
      emitActivity(row({ id, message: "/Users/me/Documents/taxes.pdf" }));
    }
    await wrapper.vm.$nextTick();

    expect(store.deniedCount).toBe(1);
    expect(wrapper.find(BANNER).text()).toContain(
      "macOS privacy protection is stopping Driven from reading 1 file."
    );
  });

  it("counts two distinct files separately", async () => {
    const { store, wrapper } = await mountBanner();

    emitActivity(row({ id: 20, message: "/Users/me/Documents/taxes.pdf" }));
    emitActivity(row({ id: 21, message: "/Users/me/Desktop/notes.txt" }));
    await wrapper.vm.$nextTick();

    expect(store.deniedCount).toBe(2);
    expect(wrapper.find(BANNER).text()).toContain(
      "macOS privacy protection is stopping Driven from reading 2 files."
    );
  });

  it("opens the Full Disk Access pane with the exact deep link", async () => {
    const { wrapper } = await mountBanner();
    emitActivity(row());
    await wrapper.vm.$nextTick();

    await wrapper.find(OPEN).trigger("click");

    expect(openUrlMock).toHaveBeenCalledTimes(1);
    expect(openUrlMock).toHaveBeenCalledWith(FDA_URL);
  });

  it("stays up when the opener rejects", async () => {
    openUrlMock.mockRejectedValue(new Error("no opener"));
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    const { wrapper } = await mountBanner();
    emitActivity(row());
    await wrapper.vm.$nextTick();

    await wrapper.find(OPEN).trigger("click");
    await wrapper.vm.$nextTick();

    expect(wrapper.find(BANNER).exists()).toBe(true);
    errorSpy.mockRestore();
  });

  // Dismissal must survive the very next cycle's denial rows, or the button
  // would be useless: a denied file is re-reported minutes later, forever.
  it("stays dismissed when further denials arrive", async () => {
    const { store, wrapper } = await mountBanner();
    emitActivity(row({ id: 30, message: "/Users/me/Documents/taxes.pdf" }));
    await wrapper.vm.$nextTick();

    await wrapper.find(DISMISS).trigger("click");
    expect(wrapper.find(BANNER).exists()).toBe(false);

    emitActivity(row({ id: 31, message: "/Users/me/Documents/taxes.pdf" }));
    emitActivity(row({ id: 32, message: "/Users/me/Pictures/scan.png" }));
    await wrapper.vm.$nextTick();

    expect(wrapper.find(BANNER).exists()).toBe(false);
    // The denials still COUNT while hidden, so a later reset shows the truth.
    expect(store.deniedCount).toBe(2);
  });

  it("explains that an unsigned build can lose the permission on update", async () => {
    const { wrapper } = await mountBanner();
    emitActivity(row());
    await wrapper.vm.$nextTick();

    const note = wrapper.find(UNSIGNED_NOTE);
    expect(note.exists()).toBe(true);
    expect(note.text()).toContain("not signed yet");
    expect(note.text()).toContain("remove Driven from the Full Disk Access list and add it back");
  });

  it("labels its actions", async () => {
    const { wrapper } = await mountBanner();
    emitActivity(row());
    await wrapper.vm.$nextTick();

    expect(wrapper.find(OPEN).text()).toBe("Open Full Disk Access settings");
    expect(wrapper.find(DISMISS).text()).toBe("Close");
  });

  it("unsubscribes from the activity stream when it unmounts", async () => {
    const { wrapper } = await mountBanner();
    wrapper.unmount();
    expect(unlisten).toHaveBeenCalledTimes(1);
  });

  it("survives a failed subscription without throwing", async () => {
    const { listen } = await import("@tauri-apps/api/event");
    const listenMock = vi.mocked(listen);
    listenMock.mockRejectedValueOnce(new Error("listen failed"));
    const errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});

    const pinia = createPinia();
    setActivePinia(pinia);
    const wrapper = mount(FdaBanner, { global: { plugins: [pinia, i18n] } });
    await vi.waitFor(() => expect(errorSpy).toHaveBeenCalled());

    expect(wrapper.find(BANNER).exists()).toBe(false);
    wrapper.unmount();
    errorSpy.mockRestore();
  });
});

describe("fdaBanner store", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
  });

  it("falls back to the row id when a denial row carries no message", () => {
    const store = useFdaBannerStore();

    store.noteDenial(row({ id: 40, message: null }) as never);
    store.noteDenial(row({ id: 41, message: null }) as never);

    // No path to dedupe on, so each row counts - better than collapsing two
    // different files into one.
    expect(store.deniedCount).toBe(2);
    expect(store.visible).toBe(true);
  });

  it("hides the banner once dismissed and restores it on reset", () => {
    const store = useFdaBannerStore();
    store.noteDenial(row() as never);
    expect(store.visible).toBe(true);

    store.dismiss();
    expect(store.dismissed).toBe(true);
    expect(store.visible).toBe(false);

    store.reset();
    expect(store.deniedCount).toBe(0);
    expect(store.dismissed).toBe(false);
    expect(store.visible).toBe(false);
  });
});
