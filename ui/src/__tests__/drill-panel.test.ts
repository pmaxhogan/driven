// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { mount, flushPromises } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

// Restore-drill panel + store tests. The seam is `@tauri-apps/api/core`'s
// `invoke` (every typed IPC wrapper routes through it), so the panel can be
// driven against a fake backend with no Tauri shell.
//
// The load-bearing assertion here is the one a well-meaning refactor could
// silently break: an INCONCLUSIVE run - one that verified nothing, because
// there was nothing restorable or every candidate was skipped for an
// unavailable key - must never render as a pass. "We restored nothing" reading
// as "we restored everything successfully" is the exact lie this whole feature
// exists to prevent. The panel must also render COUNTS and closed-vocabulary
// error CODES only; the DTO carries no paths by design, and the panel must not
// start inventing them.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => vi.fn()) }));

import { i18n } from "../i18n";
import DrillHistoryPanel from "../components/DrillHistoryPanel.vue";
import { useDrillStore, DRILL_HISTORY_LIMIT } from "../stores/drill";
import type { DrillRun } from "../ipc/types";

function run(over: Partial<DrillRun> = {}): DrillRun {
  return {
    id: 1,
    sourceId: "11111111-1111-4111-8111-111111111111",
    startedAt: 1_700_000_000_000,
    finishedAt: 1_700_000_060_000,
    sampled: 3,
    verified: 3,
    skipped: 0,
    failed: 0,
    failureCodes: [],
    outcome: "passed",
    ...over,
  };
}

async function mountPanel(runs: DrillRun[]) {
  invokeMock.mockResolvedValue(runs);
  const pinia = createPinia();
  setActivePinia(pinia);
  const wrapper = mount(DrillHistoryPanel, { global: { plugins: [pinia, i18n] } });
  await flushPromises();
  return wrapper;
}

beforeEach(() => {
  invokeMock.mockReset();
});

describe("DrillHistoryPanel", () => {
  it("loads the recent runs on mount through the list_drill_runs command", async () => {
    const wrapper = await mountPanel([run()]);
    expect(invokeMock).toHaveBeenCalledWith("list_drill_runs", {
      sourceId: undefined,
      limit: DRILL_HISTORY_LIMIT,
    });
    expect(wrapper.findAll('[data-testid="drill-run"]')).toHaveLength(1);
    expect(wrapper.find('[data-testid="drill-panel"]').exists()).toBe(true);
  });

  it("shows the empty state when no drill has run yet", async () => {
    const wrapper = await mountPanel([]);
    expect(wrapper.find('[data-testid="drill-empty"]').text()).toBe(
      "No restore drill has run yet."
    );
    expect(wrapper.findAll('[data-testid="drill-run"]')).toHaveLength(0);
  });

  it("renders a passing run as all-good with no warning banner", async () => {
    const wrapper = await mountPanel([run()]);
    const row = wrapper.find('[data-testid="drill-run"]');
    expect(row.text()).toContain("All good");
    expect(row.text()).toContain("3 restored");
    expect(wrapper.find('[data-testid="drill-attention"]').exists()).toBe(false);
    expect(wrapper.find('[data-testid="drill-inconclusive"]').exists()).toBe(false);
  });

  it("never reports a run that verified nothing as a pass", async () => {
    // The core records `inconclusive` precisely so "we could not check" stays
    // distinguishable from "we checked and it was fine". Collapsing the two
    // would give a user a green light their backup has not earned.
    const wrapper = await mountPanel([run({ outcome: "inconclusive", verified: 0, skipped: 3 })]);
    const row = wrapper.find('[data-testid="drill-run"]');
    expect(row.text()).toContain("Could not check");
    expect(row.text()).not.toContain("All good");
    // And it is called out above the list, without the red data-loss framing.
    expect(wrapper.find('[data-testid="drill-inconclusive"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="drill-attention"]').exists()).toBe(false);
  });

  it("renders a failed run with its counts and its failure codes", async () => {
    const wrapper = await mountPanel([
      run({
        outcome: "failed",
        verified: 1,
        skipped: 0,
        failed: 2,
        failureCodes: [{ code: "crypto.decrypt_failed", count: 2 }],
      }),
    ]);
    const row = wrapper.find('[data-testid="drill-run"]');
    expect(row.text()).toContain("Needs attention");
    expect(wrapper.find('[data-testid="drill-run-failed"]').text()).toContain("2");
    expect(wrapper.find('[data-testid="drill-run-codes"]').text()).toContain(
      "crypto.decrypt_failed x2"
    );
  });

  it("shows the skipped count only when a run skipped anything", async () => {
    const none = await mountPanel([run()]);
    expect(none.find('[data-testid="drill-run-skipped"]').exists()).toBe(false);

    const some = await mountPanel([run({ outcome: "passed", verified: 1, skipped: 2 })]);
    expect(some.find('[data-testid="drill-run-skipped"]').text()).toContain("2");
  });

  it("raises a banner summarising files that could not be restored", async () => {
    const wrapper = await mountPanel([
      run({ id: 2, outcome: "failed", verified: 2, failed: 1 }),
      run({ id: 1, outcome: "failed", verified: 1, failed: 2 }),
    ]);
    // Summed across the loaded window: a file that would not come back two
    // drills ago is just as unrestorable today unless something was done.
    expect(wrapper.find('[data-testid="drill-attention"]').text()).toContain("3");
  });

  it("renders counts and codes only, never a path", async () => {
    const wrapper = await mountPanel([
      run({
        outcome: "failed",
        verified: 0,
        failed: 3,
        failureCodes: [{ code: "drive.unreachable", count: 3 }],
      }),
    ]);
    const text = wrapper.text();
    expect(text).not.toContain("/");
    expect(text).not.toContain("\\");
    expect(text).toContain("drive.unreachable");
  });

  it("surfaces a failed load as a translated error code, not a raw message", async () => {
    invokeMock.mockRejectedValue({ code: "state.db_corrupt", message: "backend English" });
    const pinia = createPinia();
    setActivePinia(pinia);
    const wrapper = mount(DrillHistoryPanel, { global: { plugins: [pinia, i18n] } });
    await flushPromises();
    const err = wrapper.find('[data-testid="drill-error"]');
    expect(err.exists()).toBe(true);
    expect(err.text()).not.toContain("backend English");
    expect(wrapper.findAll('[data-testid="drill-run"]')).toHaveLength(0);
  });

  it("re-queries when the refresh button is pressed", async () => {
    const wrapper = await mountPanel([run()]);
    invokeMock.mockClear();
    invokeMock.mockResolvedValue([run({ id: 9, sampled: 7, verified: 7 })]);
    await wrapper.find('[data-testid="drill-refresh"]').trigger("click");
    await flushPromises();
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(wrapper.find('[data-testid="drill-run"]').text()).toContain("7 restored");
  });
});

describe("useDrillStore", () => {
  it("starts empty and unloaded", () => {
    setActivePinia(createPinia());
    const store = useDrillStore();
    expect(store.runs).toEqual([]);
    expect(store.loaded).toBe(false);
    expect(store.latest).toBeNull();
    expect(store.needsAttention).toBe(false);
    expect(store.inconclusive).toBe(false);
  });

  it("exposes the newest run and the summed unrestorable count", async () => {
    invokeMock.mockResolvedValue([
      run({ id: 3, outcome: "failed", verified: 2, failed: 1 }),
      run({ id: 2, outcome: "failed", verified: 0, failed: 4 }),
      run({ id: 1 }),
    ]);
    setActivePinia(createPinia());
    const store = useDrillStore();
    await store.refresh();
    expect(store.latest?.id).toBe(3);
    expect(store.failedTotal).toBe(5);
    expect(store.needsAttention).toBe(true);
    expect(store.loaded).toBe(true);
    expect(store.errorCode).toBeNull();
  });

  it("reports inconclusive off the NEWEST run only", async () => {
    // An old inconclusive run is history; the question the flag answers is
    // "did the most recent drill actually prove anything".
    invokeMock.mockResolvedValue([
      run({ id: 2 }),
      run({ id: 1, outcome: "inconclusive", verified: 0, skipped: 3 }),
    ]);
    setActivePinia(createPinia());
    const store = useDrillStore();
    await store.refresh();
    expect(store.inconclusive).toBe(false);

    invokeMock.mockResolvedValue([
      run({ id: 3, outcome: "inconclusive", verified: 0, skipped: 3 }),
    ]);
    await store.refresh();
    expect(store.inconclusive).toBe(true);
    // Inconclusive is NOT a failure: it must not raise the data-loss banner.
    expect(store.needsAttention).toBe(false);
  });

  it("keeps the previous runs and records a code when a reload fails", async () => {
    invokeMock.mockResolvedValue([run()]);
    setActivePinia(createPinia());
    const store = useDrillStore();
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
    const store = useDrillStore();
    await store.refresh();
    expect(store.errorCode).toBe("internal.bug");
  });

  it("degrades to no runs when the backend does not return an array", async () => {
    // An older backend that does not know the command must not put `undefined`
    // into a computed and blank the Activity dashboard.
    invokeMock.mockResolvedValue(undefined);
    setActivePinia(createPinia());
    const store = useDrillStore();
    await store.refresh();
    expect(store.runs).toEqual([]);
    expect(store.failedTotal).toBe(0);
    expect(store.loaded).toBe(true);
  });
});
