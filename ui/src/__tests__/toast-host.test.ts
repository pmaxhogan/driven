// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

// ToastHost tests: the rendering half (a pushed toast appears, the X dismisses
// it, the live region is always present) AND the wiring half - proving that a
// BACKEND event turns into a toast. The backend seam is `@tauri-apps/api/event`'s
// `listen`, mocked the same way the other event-driven tests mock it, but here we
// KEEP the registered handler per channel so the test can play events into the
// component exactly as Tauri would.

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

import { i18n } from "../i18n";
import ToastHost from "../components/ToastHost.vue";
import { useToastsStore } from "../stores/toasts";
import { RUN_TOAST_DEBOUNCE_MS } from "../composables/useBackupToasts";

const HOST = '[data-testid="toast-host"]';
const TOAST = '[data-testid="toast"]';
const DISMISS = '[data-testid="toast-dismiss"]';

/** Play a `sync:status_changed` event for one account, as the backend emits it. */
function emitStatus(accountId: string, state: Record<string, unknown>): void {
  handlers.get("sync:status_changed")?.({ payload: { account_id: accountId, state } });
}

/** Play an `activity:new` event carrying one activity row. */
function emitActivity(entry: Record<string, unknown>): void {
  handlers.get("activity:new")?.({ payload: entry });
}

async function mountHost() {
  const pinia = createPinia();
  setActivePinia(pinia);
  const store = useToastsStore();
  const wrapper = mount(ToastHost, { global: { plugins: [pinia, i18n] } });
  // Let the composable's onMounted `listen` calls resolve so the handlers are
  // registered before a test plays an event.
  await vi.waitFor(() => expect(handlers.has("activity:new")).toBe(true));
  return { store, wrapper };
}

beforeEach(() => {
  handlers.clear();
  unlisten.mockClear();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("ToastHost", () => {
  it("renders the polite live region even with nothing to show", async () => {
    const { wrapper } = await mountHost();

    const host = wrapper.find(HOST);
    expect(host.exists()).toBe(true);
    expect(host.attributes("aria-live")).toBe("polite");
    expect(wrapper.findAll(TOAST)).toHaveLength(0);
  });

  it("renders a pushed toast with its message", async () => {
    const { store, wrapper } = await mountHost();
    store.push({ kind: "success", message: "Rules saved" });
    await wrapper.vm.$nextTick();

    const toasts = wrapper.findAll(TOAST);
    expect(toasts).toHaveLength(1);
    expect(toasts[0].text()).toContain("Rules saved");
    // The severity is also announced, not conveyed by color alone.
    expect(toasts[0].text()).toContain("Success");
  });

  it("dismisses a toast when its X is clicked", async () => {
    const { store, wrapper } = await mountHost();
    store.push({ message: "Backup started" });
    await wrapper.vm.$nextTick();

    const dismiss = wrapper.find(DISMISS);
    expect(dismiss.attributes("aria-label")).toBe("Dismiss notification");
    await dismiss.trigger("click");

    expect(store.toasts).toHaveLength(0);
    expect(wrapper.findAll(TOAST)).toHaveLength(0);
  });

  it("pauses the dismissal timer while the pointer is over a toast", async () => {
    vi.useFakeTimers();
    const { store, wrapper } = await mountHost();
    store.push({ message: "read me", timeoutMs: 5_000 });
    await wrapper.vm.$nextTick();

    await wrapper.find(TOAST).trigger("mouseenter");
    await vi.advanceTimersByTimeAsync(20_000);
    expect(store.toasts).toHaveLength(1);

    await wrapper.find(TOAST).trigger("mouseleave");
    await vi.advanceTimersByTimeAsync(5_000);
    expect(store.toasts).toHaveLength(0);
  });

  it("stacks the newest toast last", async () => {
    const { store, wrapper } = await mountHost();
    store.push({ message: "first" });
    store.push({ message: "second" });
    await wrapper.vm.$nextTick();

    const rendered = wrapper.findAll(TOAST).map((toast) => toast.text());
    expect(rendered[0]).toContain("first");
    expect(rendered[1]).toContain("second");
  });
});

describe("ToastHost backup-event wiring", () => {
  it("toasts 'Backup started' when a planned run reaches executing", async () => {
    const { store, wrapper } = await mountHost();

    emitStatus("acct-1", { state: "planning", plan: { uploads: 2, trashes: 0 } });
    emitStatus("acct-1", { state: "executing", progress: {} });
    await wrapper.vm.$nextTick();

    expect(store.toasts).toHaveLength(1);
    expect(store.toasts[0].message).toBe("Backup started");
    expect(wrapper.find(TOAST).text()).toContain("Backup started");
  });

  it("stays silent for an idle cycle whose plan produced no ops", async () => {
    const { store } = await mountHost();

    emitStatus("acct-1", { state: "planning", plan: { uploads: 0, trashes: 0 } });
    emitStatus("acct-1", { state: "executing", progress: {} });
    emitStatus("acct-1", { state: "idle" });

    expect(store.toasts).toHaveLength(0);
  });

  it("toasts 'Backup started' only once for a burst of accounts", async () => {
    const { store } = await mountHost();

    for (const account of ["acct-1", "acct-2", "acct-3"]) {
      emitStatus(account, { state: "planning", plan: { uploads: 1, trashes: 0 } });
      emitStatus(account, { state: "executing", progress: {} });
    }

    expect(store.toasts).toHaveLength(1);
  });

  it("toasts again for the next run once the debounce window has passed", async () => {
    vi.useFakeTimers();
    const { store } = await mountHost();

    emitStatus("acct-1", { state: "planning", plan: { uploads: 1, trashes: 0 } });
    emitStatus("acct-1", { state: "executing", progress: {} });
    emitStatus("acct-1", { state: "idle" });
    expect(store.toasts).toHaveLength(1);

    await vi.advanceTimersByTimeAsync(RUN_TOAST_DEBOUNCE_MS + 1);
    emitStatus("acct-1", { state: "planning", plan: { uploads: 1, trashes: 0 } });
    emitStatus("acct-1", { state: "executing", progress: {} });

    expect(store.toasts).toHaveLength(1);
    // The first one auto-dismissed while we waited; this is a fresh toast.
    expect(store.toasts[0].message).toBe("Backup started");
  });

  it("toasts 'Backup complete' with the file count on a backup_done row", async () => {
    const { store, wrapper } = await mountHost();

    emitActivity({
      id: 1,
      ts: Date.now(),
      sourceId: "src-1",
      level: "info",
      eventType: "backup_done",
      fileCount: 3,
      bytes: 1234,
      message: null,
    });
    await wrapper.vm.$nextTick();

    expect(store.toasts).toHaveLength(1);
    expect(store.toasts[0].kind).toBe("success");
    expect(store.toasts[0].message).toBe("Backup complete - 3 files uploaded");
  });

  it("drops the count when a completed run uploaded nothing", async () => {
    const { store } = await mountHost();

    emitActivity({
      id: 2,
      ts: Date.now(),
      sourceId: "src-1",
      level: "info",
      eventType: "backup_done",
      fileCount: 0,
      bytes: 0,
      message: null,
    });

    expect(store.toasts[0].message).toBe("Backup complete");
  });

  it("ignores activity rows that are not backup_done", async () => {
    const { store } = await mountHost();

    emitActivity({
      id: 3,
      ts: Date.now(),
      sourceId: "src-1",
      level: "info",
      eventType: "upload_done",
      fileCount: 1,
      bytes: 10,
      message: null,
    });

    expect(store.toasts).toHaveLength(0);
  });

  it("collapses a multi-source run's backup_done rows into one toast", async () => {
    const { store } = await mountHost();

    for (const id of [4, 5, 6]) {
      emitActivity({
        id,
        ts: Date.now(),
        sourceId: `src-${id}`,
        level: "info",
        eventType: "backup_done",
        fileCount: 1,
        bytes: 10,
        message: null,
      });
    }

    expect(store.toasts).toHaveLength(1);
  });

  it("unsubscribes from the backend events when the host unmounts", async () => {
    const { wrapper } = await mountHost();
    wrapper.unmount();
    expect(unlisten).toHaveBeenCalledTimes(2);
  });
});
