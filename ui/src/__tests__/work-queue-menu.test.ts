// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";
import type { QueueSnapshot, WorkItem, WorkKind } from "../ipc/types";

// Work-queue menu tests (issue #304). These mount the REAL component over the
// real stores with a faked `invoke`, so they cover the dropdown primitive's
// accessibility contract (aria-expanded, Escape, click-outside), the badge
// count, every row shape (running with progress, each pending kind), the empty
// state, and that the X / Clear all buttons reach the right IPC command.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => undefined),
}));

import WorkQueueMenu from "../components/WorkQueueMenu.vue";
import { useProgressStore } from "../stores/progress";
import { useWorkQueueStore } from "../stores/workQueue";

const TRIGGER = '[data-testid="dropdown-trigger"]';
const PANEL = '[data-testid="dropdown-panel"]';
const BADGE = '[data-testid="work-queue-badge"]';
const ITEM = '[data-testid="work-queue-item"]';
const EMPTY = '[data-testid="work-queue-empty"]';
const FOOTER = '[data-testid="work-queue-footer"]';
const CANCEL = '[data-testid="work-queue-cancel"]';
const CLEAR_ALL = '[data-testid="work-queue-clear-all"]';
const PROGRESS = '[data-testid="work-queue-progress"]';

function item(id: number, kind: WorkKind, sourceId: string | null = null): WorkItem {
  return { id, kind, source_id: sourceId, enqueued_at: Date.now(), tick: "manual" };
}

function snapshot(accountId: string, partial: Partial<QueueSnapshot> = {}): QueueSnapshot {
  return {
    account_id: accountId,
    running: null,
    running_cancelled: false,
    pending: [],
    next_scheduled_at: null,
    ...partial,
  };
}

function mountMenu() {
  return mount(WorkQueueMenu, {
    attachTo: document.body,
    global: { plugins: [i18n] },
  });
}

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  document.body.innerHTML = "";
});

describe("work queue menu", () => {
  it("starts collapsed with no badge when nothing is queued", () => {
    const w = mountMenu();
    expect(w.find(PANEL).exists()).toBe(false);
    expect(w.find(BADGE).exists()).toBe(false);
    expect(w.find(TRIGGER).attributes("aria-expanded")).toBe("false");
  });

  it("badges pending PLUS running - the count before closing the app", async () => {
    const queue = useWorkQueueStore();
    queue.ingest(snapshot("a", { running: item(1, "manual"), pending: [item(2, "watcher")] }));
    const w = mountMenu();
    await flushPromises();
    expect(w.find(BADGE).text()).toBe("2");
  });

  it("opens on click and closes on Escape, returning focus to the trigger", async () => {
    const w = mountMenu();
    await w.find(TRIGGER).trigger("click");
    expect(w.find(PANEL).exists()).toBe(true);
    expect(w.find(TRIGGER).attributes("aria-expanded")).toBe("true");
    // aria-controls must name the panel that actually rendered.
    expect(w.find(TRIGGER).attributes("aria-controls")).toBe(w.find(PANEL).attributes("id"));

    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    await flushPromises();
    expect(w.find(PANEL).exists()).toBe(false);
    expect(document.activeElement).toBe(w.find(TRIGGER).element);
  });

  it("closes when a pointer lands outside it", async () => {
    const w = mountMenu();
    await w.find(TRIGGER).trigger("click");
    expect(w.find(PANEL).exists()).toBe(true);

    document.body.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
    await flushPromises();
    expect(w.find(PANEL).exists()).toBe(false);
  });

  it("shows the empty state with the next scheduled backup, and no footer", async () => {
    const queue = useWorkQueueStore();
    queue.ingest(snapshot("a", { next_scheduled_at: new Date("2026-08-17T21:00:00").getTime() }));
    const w = mountMenu();
    await w.find(TRIGGER).trigger("click");

    const empty = w.find(EMPTY).text();
    expect(empty).toContain("No pending work");
    // The time is rendered in the runtime locale, so assert the hour/minute
    // rather than a fixed format string.
    expect(empty).toMatch(/9:00|21:00/);
    expect(w.find(FOOTER).exists()).toBe(false);
    expect(w.find(CLEAR_ALL).exists()).toBe(false);
  });

  it("names the running item after its source and renders its live progress", async () => {
    const queue = useWorkQueueStore();
    queue.sourceNames = { "src-1": "Documents" };
    queue.ingest(snapshot("acct-1", { running: item(1, "manual", "src-1") }));
    const progress = useProgressStore();
    progress.ingest({
      account_id: "acct-1",
      state: { state: "executing", progress: {} } as never,
    });
    progress.ingestProgress({
      account_id: "acct-1",
      source_id: "src-1",
      progress: {
        files_done: 12_410,
        files_total: 48_112,
        bytes_done: 1_000_000_000,
        bytes_total: 4_000_000_000,
        trashes_done: 0,
        trashes_total: 0,
        errors: 0,
      },
    });

    const w = mountMenu();
    await w.find(TRIGGER).trigger("click");

    const row = w.find(ITEM);
    expect(row.text()).toContain("Backing up");
    expect(row.text()).toContain("Documents");
    expect(row.text()).toContain("48,112");
    expect(w.find(PROGRESS).attributes("aria-valuenow")).toBe("25");
    expect(w.find(FOOTER).text()).toBe("Items run one at a time per account.");
  });

  it("labels each pending kind so the list is scannable", async () => {
    const queue = useWorkQueueStore();
    queue.sourceNames = { photos: "Photos" };
    queue.ingest(
      snapshot("a", {
        pending: [
          item(1, "recovery", "photos"),
          item(2, "watcher"),
          item(3, "manual"),
          item(4, "scheduled"),
        ],
      })
    );
    const w = mountMenu();
    await w.find(TRIGGER).trigger("click");

    const rows = w.findAll(ITEM);
    expect(rows).toHaveLength(4);
    expect(rows[0].text()).toContain("Recover interrupted backup");
    expect(rows[0].text()).toContain("Photos");
    expect(rows[1].text()).toContain("Changed files");
    expect(rows[2].text()).toContain("Backup now");
    expect(rows[3].text()).toContain("Scheduled backup");
    // Age labels read in whole units, never a ticking seconds counter.
    expect(rows[2].text()).toContain("just now");
  });

  it("cancels the clicked item by (account, id)", async () => {
    const queue = useWorkQueueStore();
    queue.ingest(snapshot("acct-9", { pending: [item(42, "watcher")] }));
    const w = mountMenu();
    await w.find(TRIGGER).trigger("click");

    await w.find(CANCEL).trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("cancel_work_item", {
      accountId: "acct-9",
      itemId: 42,
    });
  });

  it("disables the X on an item that is already draining", async () => {
    const queue = useWorkQueueStore();
    queue.ingest(snapshot("a", { running: item(1, "manual"), running_cancelled: true }));
    const w = mountMenu();
    await w.find(TRIGGER).trigger("click");

    expect(w.find(ITEM).text()).toContain("Finishing up");
    expect(w.find(CANCEL).attributes("disabled")).toBeDefined();
  });

  it("clears every account from the header button", async () => {
    const queue = useWorkQueueStore();
    queue.ingest(snapshot("a", { pending: [item(1, "manual"), item(2, "watcher")] }));
    const w = mountMenu();
    await w.find(TRIGGER).trigger("click");

    await w.find(CLEAR_ALL).trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledWith("clear_work_queue", { accountId: null });
  });
});
