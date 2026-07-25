// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { mount } from "@vue/test-utils";
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

function mountBar() {
  const pinia = createPinia();
  setActivePinia(pinia);
  const store = useProgressStore();
  const wrapper = mount(GlobalProgressBar, { global: { plugins: [pinia, i18n] } });
  return { store, wrapper };
}

const BAR = '[role="progressbar"]';
const INDETERMINATE = ".global-progress__indeterminate";
const PHASE_LABEL = '[data-testid="global-progress-label"]';

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
    await wrapper.vm.$nextTick();

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
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BAR).attributes("aria-valuenow")).toBe("33");
    expect(wrapper.find(BAR).attributes("aria-label")).toBe("Backing up - 33%");
  });

  it("shows an indeterminate sweep (no aria-valuenow) during scan/plan/verify", async () => {
    const { store, wrapper } = mountBar();
    store.ingest(perAccount("a", scanning()));
    await wrapper.vm.$nextTick();

    const bar = wrapper.find(BAR);
    expect(bar.exists()).toBe(true);
    expect(bar.attributes("aria-valuenow")).toBeUndefined();
    expect(wrapper.find(INDETERMINATE).exists()).toBe(true);
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
      await wrapper.vm.$nextTick();

      expect(wrapper.find(PHASE_LABEL).text()).toBe("Scanning for changes...");
      expect(wrapper.find(BAR).attributes("aria-label")).toBe("Scanning for changes...");
    });

    it("streams the live scanned file count into the readout", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", scanning(512)));
      await wrapper.vm.$nextTick();
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Scanning for changes - 512 files");

      // A later scan tick updates the same readout in place (locale-grouped).
      store.ingest(perAccount("a", scanning(12401)));
      await wrapper.vm.$nextTick();
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Scanning for changes - 12,401 files");
    });

    it("names the planning phase with the planned change count", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", planning(1200, 34)));
      await wrapper.vm.$nextTick();
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Preparing 1,234 changes");
    });

    it("names the verifying phase with the sampled file count", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", verifying(42)));
      await wrapper.vm.$nextTick();
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Verifying backup - 42 files");
    });

    it("names the pre-flight power check", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", powerCheck()));
      await wrapper.vm.$nextTick();
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Starting backup...");
    });

    it("shows the upload percent once execution starts", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", executing({ bytes_done: 1, bytes_total: 4 })));
      await wrapper.vm.$nextTick();
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Backing up - 25%");
    });

    it("reports the executing account while another is still scanning", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", scanning(900)));
      store.ingest(perAccount("b", executing({ bytes_done: 1, bytes_total: 2 })));
      await wrapper.vm.$nextTick();
      // Executing outranks scanning: it is the phase with a real percent.
      expect(wrapper.find(PHASE_LABEL).text()).toBe("Backing up - 50%");
    });

    it("renders no readout at all while idle", async () => {
      const { store, wrapper } = mountBar();
      store.ingest(perAccount("a", idle()));
      await wrapper.vm.$nextTick();
      expect(wrapper.find(PHASE_LABEL).exists()).toBe(false);
    });
  });

  it("appears and disappears reactively as a run starts then finishes", async () => {
    const { store, wrapper } = mountBar();
    expect(wrapper.find(BAR).exists()).toBe(false);

    store.ingest(perAccount("a", executing({ bytes_done: 1, bytes_total: 4 })));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BAR).exists()).toBe(true);
    expect(wrapper.find(BAR).attributes("aria-valuenow")).toBe("25");

    store.ingest(perAccount("a", idle()));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BAR).exists()).toBe(false);
  });
});
