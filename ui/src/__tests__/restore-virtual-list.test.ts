// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { createPinia, setActivePinia } from "pinia";
import { mount, flushPromises } from "@vue/test-utils";

import { i18n } from "../i18n";

// UI #47: the Restore file list is virtualized (windowed) so a folder with
// thousands of rows mounts only the visible window of <li> nodes (not one per
// file), and the action bar is sticky so the Restore / Cancel controls stay
// reachable without scrolling a huge folder to the bottom. This mounts the real
// Restore.vue against faked IPC/event seams with a 5000-entry folder and asserts
// both: a bounded DOM node count + a large bottom spacer, and the sticky bar.

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
    deepVerifyIntervalSecs: 604800,
    lastFullScanAt: null,
    createdAt: 0,
    pendingRecoveryAck: false,
  };
}

/** A flat folder of `count` restorable files. */
function bigFolder(count: number): RemoteEntryDto[] {
  const entries: RemoteEntryDto[] = [];
  for (let i = 0; i < count; i += 1) {
    entries.push({
      relativePath: `file-${i}.bin`,
      name: `file-${i}.bin`,
      isDir: false,
      size: 1024,
      status: "synced",
      restorable: true,
    });
  }
  return entries;
}

const FILE_COUNT = 5000;

beforeEach(() => {
  setActivePinia(createPinia());
  invokeMock.mockReset();
  // Deterministic viewport so the windowing math is exact: ceil(800/40) = 20
  // visible rows + overscan.
  Object.defineProperty(window, "innerHeight", { value: 800, configurable: true });
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case "list_sources":
        return Promise.resolve([source("s1", "Documents")]);
      case "list_remote_tree":
        return Promise.resolve({ entries: bigFolder(FILE_COUNT), truncated: false });
      case "get_restore_job":
        return Promise.resolve(undefined);
      default:
        return Promise.resolve(undefined);
    }
  });
});

describe("Restore virtualization + sticky action bar (UI #47)", () => {
  it("mounts only a bounded window of rows for a huge folder", async () => {
    const wrapper = mount(Restore, { global: { plugins: [i18n] } });
    await flushPromises();

    const list = wrapper.get('[data-testid="restore-list"]');
    const rows = list.findAll("li");
    // Far fewer than the full 5000: the visible window (20) plus overscan on each
    // side, never the whole list.
    expect(rows.length).toBeGreaterThan(0);
    expect(rows.length).toBeLessThanOrEqual(40);
    expect(rows.length).toBeLessThan(FILE_COUNT);

    // The first windowed row is the first file (top of the list, not scrolled).
    expect(rows[0].text()).toContain("file-0.bin");

    // A large bottom spacer stands in for the un-mounted rows so the scrollbar
    // still reflects the full list.
    const style = list.attributes("style") ?? "";
    expect(style).toContain("padding-bottom");
    const match = style.match(/padding-bottom:\s*(\d+)px/);
    expect(match).not.toBeNull();
    expect(Number(match?.[1])).toBeGreaterThan(10000);

    wrapper.unmount();
  });

  it("exposes a point-in-time as-of picker that drives the store (issue #36)", async () => {
    const wrapper = mount(Restore, { global: { plugins: [i18n] } });
    await flushPromises();

    const asOf = wrapper.get('[data-testid="restore-as-of"]');
    // Default: latest (no point-in-time), so the active notice is hidden.
    expect(wrapper.find('[data-testid="restore-as-of-note"]').exists()).toBe(false);

    // Setting a datetime activates point-in-time and shows the TTL notice.
    await asOf.setValue("2026-01-02T03:04");
    await asOf.trigger("change");
    await flushPromises();
    const note = wrapper.get('[data-testid="restore-as-of-note"]');
    expect(note.text()).toContain(i18n.global.t("restore.asOf.ttlHint"));

    // Clearing returns to latest and hides the notice.
    await wrapper.get('[data-testid="restore-as-of-clear"]').trigger("click");
    await flushPromises();
    expect(wrapper.find('[data-testid="restore-as-of-note"]').exists()).toBe(false);

    wrapper.unmount();
  });

  it("renders the action bar as a sticky bottom footer", async () => {
    const wrapper = mount(Restore, { global: { plugins: [i18n] } });
    await flushPromises();

    const bar = wrapper.get('[data-testid="restore-action-bar"]');
    const classes = bar.classes();
    expect(classes).toContain("sticky");
    expect(classes).toContain("bottom-0");
    // It has a background + top border so list rows do not show through / butt
    // against it as they scroll beneath, and a z-index to sit above them.
    expect(classes).toContain("border-t");
    expect(classes).toContain("z-10");
    expect(classes.some((c) => c.startsWith("bg-"))).toBe(true);

    // The primary Restore action lives inside the sticky bar.
    expect(bar.text()).toContain(i18n.global.t("restore.start"));

    wrapper.unmount();
  });
});
