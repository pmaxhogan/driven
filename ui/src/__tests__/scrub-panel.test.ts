// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount, flushPromises } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

// Integrity-scrub panel + store tests. The seam is `@tauri-apps/api/core`'s
// `invoke` (every typed IPC wrapper routes through it), so the panel can be
// driven against a fake backend with no Tauri shell.
//
// The load-bearing assertions here are the two that a well-meaning refactor
// could silently break: the panel must distinguish "could not check" from
// "checked and clean" (conflating them would let a week of failed checks read
// as a week of green ones), and it must render COUNTS ONLY - the DTO carries no
// paths by design, and the panel must not start inventing them.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => vi.fn()) }));

import { i18n } from "../i18n";
import ScrubHistoryPanel from "../components/ScrubHistoryPanel.vue";
import { useScrubStore, SCRUB_HISTORY_LIMIT } from "../stores/scrub";
import type { ScrubRun } from "../ipc/types";

function run(over: Partial<ScrubRun> = {}): ScrubRun {
  return {
    id: 1,
    sourceId: "11111111-1111-4111-8111-111111111111",
    startedAt: 1_700_000_000_000,
    finishedAt: 1_700_000_060_000,
    checked: 40,
    ok: 40,
    missing: 0,
    sizeMismatch: 0,
    hashMismatch: 0,
    unverifiable: 0,
    healed: 0,
    healedBundleMembers: 0,
    unrecoverable: 0,
    deepChecked: 0,
    deepFailed: 0,
    wrapped: false,
    outcome: "clean",
    ...over,
  };
}

async function mountPanel(runs: ScrubRun[]) {
  invokeMock.mockResolvedValue(runs);
  const pinia = createPinia();
  setActivePinia(pinia);
  const wrapper = mount(ScrubHistoryPanel, { global: { plugins: [pinia, i18n] } });
  await flushPromises();
  return wrapper;
}

beforeEach(() => {
  invokeMock.mockReset();
});

describe("ScrubHistoryPanel", () => {
  it("loads the recent runs on mount through the list_scrub_runs command", async () => {
    const wrapper = await mountPanel([run()]);
    expect(invokeMock).toHaveBeenCalledWith("list_scrub_runs", {
      sourceId: undefined,
      limit: SCRUB_HISTORY_LIMIT,
    });
    expect(wrapper.findAll('[data-testid="scrub-run"]')).toHaveLength(1);
    expect(wrapper.find('[data-testid="scrub-panel"]').exists()).toBe(true);
  });

  it("shows the empty state when nothing has been scrubbed yet", async () => {
    const wrapper = await mountPanel([]);
    expect(wrapper.find('[data-testid="scrub-empty"]').text()).toBe("No scrub has run yet.");
    expect(wrapper.findAll('[data-testid="scrub-run"]')).toHaveLength(0);
  });

  it("renders a clean run as all-good with no warning banner", async () => {
    const wrapper = await mountPanel([run()]);
    const row = wrapper.find('[data-testid="scrub-run"]');
    expect(row.text()).toContain("All good");
    expect(row.text()).toContain("40 checked");
    expect(wrapper.find('[data-testid="scrub-attention"]').exists()).toBe(false);
  });

  it("distinguishes a repaired run from one that needs attention", async () => {
    const wrapper = await mountPanel([
      run({ id: 2, outcome: "drift", missing: 3, healed: 3, ok: 37 }),
      run({ id: 1, outcome: "drift", hashMismatch: 2, unrecoverable: 2, ok: 38 }),
    ]);
    const rows = wrapper.findAll('[data-testid="scrub-run"]');
    expect(rows[0].text()).toContain("Repaired");
    expect(rows[0].text()).toContain("3 re-queued");
    expect(rows[1].text()).toContain("Needs attention");
    expect(rows[1].text()).toContain("2 need attention");
  });

  it("never reports a run it could not complete as clean", async () => {
    // A failed enumeration writes an `incomplete` row precisely so that "we
    // could not check" stays distinguishable from "we checked and it was fine".
    const wrapper = await mountPanel([run({ outcome: "incomplete", checked: 0, ok: 0 })]);
    const row = wrapper.find('[data-testid="scrub-run"]');
    expect(row.text()).toContain("Could not check");
    expect(row.text()).not.toContain("All good");
  });

  it("raises a banner keyed on the newest run only, not summed across history", async () => {
    const wrapper = await mountPanel([
      run({ id: 2, outcome: "drift", hashMismatch: 1, unrecoverable: 1 }),
      run({ id: 1, outcome: "drift", sizeMismatch: 2, unrecoverable: 2 }),
    ]);
    // The newest run (id 2) has 1 unrecoverable object; the older run's 2 do
    // not add in. See #271: summing across the whole loaded window kept the
    // banner red long after the underlying drift was resolved.
    expect(wrapper.find('[data-testid="scrub-attention"]').text()).toContain("1");
    expect(wrapper.find('[data-testid="scrub-attention"]').text()).not.toContain("3");
  });

  it("clears the attention banner once a later scrub is clean (#271)", async () => {
    // The exact regression from #271, applied to the scrub panel per the
    // issue ("applies to the scrub panel too, which uses the same pattern").
    const wrapper = await mountPanel([
      run({ id: 2, outcome: "clean", ok: 40 }),
      run({ id: 1, outcome: "drift", sizeMismatch: 2, unrecoverable: 2 }),
    ]);
    expect(wrapper.find('[data-testid="scrub-attention"]').exists()).toBe(false);
  });

  it("surfaces a failed load as a translated error code, not a raw message", async () => {
    invokeMock.mockRejectedValue({ code: "state.db_corrupt", message: "backend English" });
    const pinia = createPinia();
    setActivePinia(pinia);
    const wrapper = mount(ScrubHistoryPanel, { global: { plugins: [pinia, i18n] } });
    await flushPromises();
    const err = wrapper.find('[data-testid="scrub-error"]');
    expect(err.exists()).toBe(true);
    expect(err.text()).not.toContain("backend English");
    expect(wrapper.findAll('[data-testid="scrub-run"]')).toHaveLength(0);
  });

  it("re-queries when the refresh button is pressed", async () => {
    const wrapper = await mountPanel([run()]);
    invokeMock.mockClear();
    invokeMock.mockResolvedValue([run({ id: 9, checked: 7, ok: 7 })]);
    await wrapper.find('[data-testid="scrub-refresh"]').trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(wrapper.find('[data-testid="scrub-run"]').text()).toContain("7 checked");
  });

  it("shows the deep-verification count only when a run did any", async () => {
    const shallow = await mountPanel([run()]);
    expect(shallow.find('[data-testid="scrub-run-deep"]').exists()).toBe(false);

    const deep = await mountPanel([run({ deepChecked: 5 })]);
    expect(deep.find('[data-testid="scrub-run-deep"]').text()).toContain("5");
  });
});

describe("useScrubStore", () => {
  it("starts empty and unloaded", () => {
    setActivePinia(createPinia());
    const store = useScrubStore();
    expect(store.runs).toEqual([]);
    expect(store.loaded).toBe(false);
    expect(store.latest).toBeNull();
    expect(store.needsAttention).toBe(false);
  });

  it("exposes the newest run and keys unrecoverableTotal off it alone", async () => {
    invokeMock.mockResolvedValue([
      run({ id: 3, unrecoverable: 1 }),
      run({ id: 2, unrecoverable: 4 }),
      run({ id: 1 }),
    ]);
    setActivePinia(createPinia());
    const store = useScrubStore();
    await store.refresh();
    expect(store.latest?.id).toBe(3);
    // Only the newest run's unrecoverable count, not the 4 from run 2 - #271.
    expect(store.unrecoverableTotal).toBe(1);
    expect(store.needsAttention).toBe(true);
    expect(store.loaded).toBe(true);
    expect(store.errorCode).toBeNull();
  });

  it("clears needsAttention the moment the newest loaded run is clean (#271)", async () => {
    // Regression coverage for the store layer, independent of the panel: a
    // run with drift followed by a refresh that returns a clean newest run
    // must flip needsAttention back to false, not keep it pinned by history.
    invokeMock.mockResolvedValue([run({ id: 1, outcome: "drift", unrecoverable: 3 })]);
    setActivePinia(createPinia());
    const store = useScrubStore();
    await store.refresh();
    expect(store.needsAttention).toBe(true);
    expect(store.unrecoverableTotal).toBe(3);

    invokeMock.mockResolvedValue([
      run({ id: 2, outcome: "clean", ok: 40 }),
      run({ id: 1, outcome: "drift", unrecoverable: 3 }),
    ]);
    await store.refresh();
    expect(store.needsAttention).toBe(false);
    expect(store.unrecoverableTotal).toBe(0);
  });

  it("keeps the previous runs and records a code when a reload fails", async () => {
    invokeMock.mockResolvedValue([run()]);
    setActivePinia(createPinia());
    const store = useScrubStore();
    await store.refresh();
    invokeMock.mockRejectedValue({ code: "drive.unreachable" });
    await store.refresh();
    expect(store.errorCode).toBe("drive.unreachable");
    expect(store.runs).toHaveLength(1);
    expect(store.loading).toBe(false);
  });

  it("falls back to a stable code when the rejection carries none", async () => {
    invokeMock.mockRejectedValue(new Error("boom"));
    setActivePinia(createPinia());
    const store = useScrubStore();
    await store.refresh();
    expect(store.errorCode).toBe("internal.bug");
  });
});
