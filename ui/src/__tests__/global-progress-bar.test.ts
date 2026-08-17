// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { mount, type VueWrapper } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

import { i18n } from "../i18n";
import GlobalProgressBar from "../components/GlobalProgressBar.vue";
import { useProgressStore } from "../stores/progress";
import type { ExecProgress, OrchestratorState } from "../ipc/types";

// GlobalProgressBar tests (issue #46). The component is a pure render of the
// progress store: a thin top-of-app bar shown ONLY while a backup/sync run is
// active, DETERMINATE (a teal fill sized to the percent, with progressbar aria
// values) while executing with a known total, and INDETERMINATE (an animated
// sweep, no aria-valuenow) during scan/plan/verify. These mount the real
// component and drive it through the store so every render branch is covered.
//
// Smoke fix (2026-08-01 pause-banner branch): a paused account's periodic
// orchestrator tick briefly passes through `PowerCheck` before re-pausing,
// flickering `progress.active` true for tens of ms with no real work
// happening. The bar now debounces `active` on BOTH edges (500ms,
// useDebouncedFlag), so every test that crosses an edge (idle/hidden <->
// active/visible) must advance fake timers past that delay before asserting
// the resulting visibility - real content updates within an already-visible
// bar stay instant and need no advance.

const DEBOUNCE_MS = 500;

function scanning(scanned = 0): OrchestratorState {
  return { state: "scanning", source_id: "src-1", scanned };
}
function planning(uploads: number, trashes = 0): OrchestratorState {
  return { state: "planning", plan: { uploads, trashes, bytes: 0 } };
}
function verifying(sampled: number): OrchestratorState {
  return { state: "verifying", sampled, mismatches: 0 };
}
function powerCheck(): OrchestratorState {
  return { state: "power_check" };
}
function idle(): OrchestratorState {
  return { state: "idle", last_run_at: null };
}
function recovering(
  bytesDone: number,
  bytesTotal: number,
  opsDone = 0,
  opsTotal = 0
): OrchestratorState {
  return {
    state: "recovering",
    source_id: "src-1",
    path: "dev-drives/dev.vhdx",
    bytes_done: bytesDone,
    bytes_total: bytesTotal,
    ops_done: opsDone,
    ops_total: opsTotal,
  };
}
function executing(p: Partial<ExecProgress>): OrchestratorState {
  const progress: ExecProgress = {
    files_done: 0,
    files_total: 0,
    bytes_done: 0,
    bytes_total: 0,
    trashes_done: 0,
    trashes_total: 0,
    errors: 0,
    ...p,
  };
  return { state: "executing", progress };
}
function perAccount(accountId: string, state: OrchestratorState) {
  return { account_id: accountId, state };
}
/** One `sync:source_progress` tick - the ONLY carrier of the moving counters
 * (the `executing` transition itself carries `ExecProgress::zero()`). */
function tick(accountId: string, p: Partial<ExecProgress>) {
  return {
    account_id: accountId,
    source_id: "src-1",
    progress: {
      files_done: 0,
      files_total: 0,
      bytes_done: 0,
      bytes_total: 0,
      trashes_done: 0,
      trashes_total: 0,
      errors: 0,
      ...p,
    },
  };
}

function mountBar() {
  const pinia = createPinia();
  setActivePinia(pinia);
  const store = useProgressStore();
  const wrapper = mount(GlobalProgressBar, { global: { plugins: [pinia, i18n] } });
  return { store, wrapper };
}

/** Advance past the visibility debounce and flush the resulting re-render. */
async function settle(wrapper: VueWrapper, ms = DEBOUNCE_MS): Promise<void> {
  await vi.advanceTimersByTimeAsync(ms);
  await wrapper.vm.$nextTick();
}

const BAR = '[role="progressbar"]';
const INDETERMINATE = ".global-progress__indeterminate";
const PHASE_LABEL = '[data-testid="global-progress-label"]';

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("GlobalProgressBar", () => {
  it("renders nothing while idle (no run active)", async () => {
    const { store, wrapper } = mountBar();
    store.ingest(perAccount("a", idle()));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BAR).exists()).toBe(false);
  });

  it("shows a determinate fill sized to the byte percent while executing", async () => {
    const { store, wrapper } = mountBar();
    store.ingest(perAccount("a", executing({ bytes_done: 512, bytes_total: 1024 })));
    await settle(wrapper);

    const bar = wrapper.find(BAR);
    expect(bar.exists()).toBe(true);
    expect(bar.attributes("aria-valuenow")).toBe("50");
    expect(bar.attributes("aria-valuemin")).toBe("0");
    expect(bar.attributes("aria-valuemax")).toBe("100");
    expect(bar.attributes("aria-label")).toBe("Backing up - 50%");
    // The determinate fill carries an inline width; the indeterminate sweep is absent.
    expect(bar.find("div").attributes("style")).toContain("width: 50%");
    expect(wrapper.find(INDETERMINATE).exists()).toBe(false);
  });

  it("rounds the determinate percent to a whole number", async () => {
    const { store, wrapper } = mountBar();
    // 1/3 -> 33%
    store.ingest(perAccount("a", executing({ bytes_done: 1, bytes_total: 3 })));
    await settle(wrapper);
    expect(wrapper.find(BAR).attributes("aria-valuenow")).toBe("33");
    expect(wrapper.find(BAR).attributes("aria-label")).toBe("Backing up - 33%");
  });

  it("shows an indeterminate sweep (no aria-valuenow) during scan/plan/verify", async () => {
    const { store, wrapper } = mountBar();
    store.ingest(perAccount("a", scanning()));
    await settle(wrapper);

    const bar = wrapper.find(BAR);
    expect(bar.exists()).toBe(true);
    expect(bar.attributes("aria-valuenow")).toBeUndefined();
    expect(wrapper.find(INDETERMINATE).exists()).toBe(true);
  });

  // The scan phase must never LOOK like a finished bar. The sweep segment is a
  // partial-width element carrying no inline width, so nothing about it can
  // render as a full determinate fill; the determinate branch (the only thing
  // that sets an inline width) is absent entirely.
  //
  // Only the template half of that guarantee is unit-testable: the scoped
  // `prefers-reduced-motion` fallback is plain CSS, which jsdom does not apply.
  // Keeping the width in a Tailwind class on the element (rather than something
  // the media query stretches to 100%) is what makes the partial width hold in
  // BOTH motion modes.
  it("renders the scan sweep as a partial-width segment with no inline width", async () => {
    const { store, wrapper } = mountBar();
    store.ingest(perAccount("a", scanning(120)));
    await settle(wrapper);

    const sweep = wrapper.find('[data-testid="global-progress-indeterminate"]');
    expect(sweep.exists()).toBe(true);
    expect(sweep.classes()).toContain("w-2/5");
    expect(sweep.attributes("style")).toBeUndefined();
    // No determinate fill alongside it (that branch is the one with a width).
    expect(wrapper.find(BAR).findAll("div[style]").length).toBe(0);
  });

  // The "Run now looks dead during the scan" fix: the bar carries a visible
  // phase readout, and the scan phase streams a live file count into it. Before
  // this, every pre-upload phase rendered the same bare "Backing up..." on a
  // 4px sliver, so the ~10s scan of a large tree was indistinguishable from a
  // hung app.
  describe("phase readout", () => {
    it("names the scan phase and hides the count until the first tick lands", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", scanning(0)));
      await settle(wrapper);

      expect(wrapper.find(PHASE_LABEL).text()).toBe("Scanning for changes...");
      expect(wrapper.find(BAR).attributes("aria-label")).toBe("Scanning for changes...");
    });

    it("streams the live scanned file count into the readout", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", scanning(512)));
      await settle(wrapper);
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Scanning for changes - 512 files");

      // A later scan tick updates the same readout in place (locale-grouped).
      // No edge crossing here (still active), so no further advance needed.
      store.ingest(perAccount("a", scanning(12401)));
      await wrapper.vm.$nextTick();
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Scanning for changes - 12,401 files");
    });

    it("names the planning phase with the planned change count", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", planning(1200, 34)));
      await settle(wrapper);
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Preparing 1,234 changes");
    });

    it("names the verifying phase with the sampled file count", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", verifying(42)));
      await settle(wrapper);
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Verifying backup - 42 files");
    });

    it("names the pre-flight power check", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", powerCheck()));
      await settle(wrapper);
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Starting backup...");
    });

    it("names the reconcile-phase recovery with its byte totals, determinate", async () => {
      // 2026-08-14 follow-up: an interrupted-upload resume used to render as
      // the bare indeterminate "Starting backup..." sweep for its whole run.
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", recovering(25 * 1024 * 1024, 100 * 1024 * 1024)));
      await settle(wrapper);
      expect(wrapper.find(PHASE_LABEL).text()).toBe(
        "Recovering interrupted upload - 25 MB of 100 MB"
      );
      const bar = wrapper.find('[role="progressbar"]');
      expect(bar.attributes("aria-valuenow")).toBe("25");
    });

    it("falls back to the bare recovering label when totals are unknown", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", recovering(0, 0)));
      await settle(wrapper);
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Recovering an interrupted upload...");
    });

    it("names the op counts for the byte-free part of the recovery (issue #301)", async () => {
      // Most of a reconcile is one remote lookup per interrupted upload - no
      // bytes move, so before #301 this rendered as a generic "Starting
      // backup..." for a measured 65 seconds.
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", recovering(0, 0, 7, 18)));
      await settle(wrapper);
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Recovering - resuming 7 of 18 uploads");
      const bar = wrapper.find('[role="progressbar"]');
      expect(bar.attributes("aria-valuenow")).toBe("39");
    });

    it("shows the upload percent once execution starts", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", executing({ bytes_done: 1, bytes_total: 4 })));
      await settle(wrapper);
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Backing up - 25%");
    });

    it("reports the executing account while another is still scanning", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", scanning(900)));
      await settle(wrapper);
      // A second account starting to execute is not a NEW edge (still active).
      store.ingest(perAccount("b", executing({ bytes_done: 1, bytes_total: 2 })));
      await wrapper.vm.$nextTick();
      // Executing outranks scanning: it is the phase with a real percent.
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Backing up - 50%");
    });

    it("names the file counts once the live ticks carry a total", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", executing({})));
      await settle(wrapper);
      // No progress tick yet, so no measurable total: indeterminate.
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Backing up...");

      store.ingestProgress(tick("a", { bytes_done: 512, bytes_total: 1024 }));
      await wrapper.vm.$nextTick();
      // No file total yet (a delete-only plan uploads nothing): bare percent.
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Backing up - 50%");

      store.ingestProgress(
        tick("a", { bytes_done: 512, bytes_total: 1024, files_done: 1234, files_total: 3000 })
      );
      await wrapper.vm.$nextTick();
      // Locale-grouped counts, so the run's scale is legible at a glance.
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Backing up - 50% (1,234 of 3,000 files)");
      expect(wrapper.find(BAR).attributes("aria-label")).toBe(
        "Backing up - 50% (1,234 of 3,000 files)"
      );
    });

    it("renders no readout at all while idle", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", idle()));
      await wrapper.vm.$nextTick();
      expect(wrapper.find(PHASE_LABEL).exists()).toBe(false);
    });
  });

  // The bug this fixes: in production the `executing` state ALWAYS arrives with
  // a zeroed ExecProgress, so before the ticks were bridged the bar rendered the
  // indeterminate sweep for the entire upload.
  it("goes from indeterminate to determinate when the first live tick lands", async () => {
    const { store, wrapper } = mountBar();
    store.ingest(perAccount("a", executing({})));
    await settle(wrapper);
    expect(wrapper.find(INDETERMINATE).exists()).toBe(true);
    expect(wrapper.find(BAR).attributes("aria-valuenow")).toBeUndefined();

    store.ingestProgress(tick("a", { bytes_done: 300, bytes_total: 1200 }));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(INDETERMINATE).exists()).toBe(false);
    expect(wrapper.find(BAR).attributes("aria-valuenow")).toBe("25");
    expect(wrapper.find(BAR).find("div").attributes("style")).toContain("width: 25%");
  });

  it("appears and disappears reactively as a run starts then finishes", async () => {
    const { store, wrapper } = mountBar();
    expect(wrapper.find(BAR).exists()).toBe(false);

    store.ingest(perAccount("a", executing({ bytes_done: 1, bytes_total: 4 })));
    await settle(wrapper);
    expect(wrapper.find(BAR).exists()).toBe(true);
    expect(wrapper.find(BAR).attributes("aria-valuenow")).toBe("25");

    store.ingest(perAccount("a", idle()));
    await settle(wrapper);
    expect(wrapper.find(BAR).exists()).toBe(false);
  });

  // Smoke fix regression coverage: the exact flicker reported in the field.
  describe("visibility debounce (smoke fix)", () => {
    it("never renders a blip shorter than 500ms (e.g. the paused-account PowerCheck tick)", async () => {
      const { store, wrapper } = mountBar();
      expect(wrapper.find(BAR).exists()).toBe(false);

      // Simulates a paused account's tick: a working state for 200ms, then
      // straight back to paused - well under the debounce window.
      store.ingest(perAccount("a", powerCheck()));
      await vi.advanceTimersByTimeAsync(200);
      expect(wrapper.find(BAR).exists()).toBe(false);

      store.ingest(perAccount("a", { state: "paused", reason: "battery" }));
      await vi.advanceTimersByTimeAsync(1_000);
      await wrapper.vm.$nextTick();
      expect(wrapper.find(BAR).exists()).toBe(false);
    });

    it("renders once a real run holds active for the full 500ms", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", scanning(1)));

      await vi.advanceTimersByTimeAsync(499);
      expect(wrapper.find(BAR).exists()).toBe(false);

      await vi.advanceTimersByTimeAsync(1);
      await wrapper.vm.$nextTick();
      expect(wrapper.find(BAR).exists()).toBe(true);
    });
  });
});
