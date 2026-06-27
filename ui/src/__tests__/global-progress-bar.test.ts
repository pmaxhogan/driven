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

function scanning(): OrchestratorState {
  return { state: "scanning", source_id: "src-1", scanned: 0 };
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
    expect(bar.attributes("aria-label")).toBe("Backing up...");
    expect(wrapper.find(INDETERMINATE).exists()).toBe(true);
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
