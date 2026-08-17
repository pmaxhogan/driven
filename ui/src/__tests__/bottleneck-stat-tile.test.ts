// @vitest-environment jsdom
import { describe, it, expect } from "vitest";
import { mount } from "@vue/test-utils";

import { i18n } from "../i18n";
import BottleneckStatTile from "../components/BottleneckStatTile.vue";
import type { BottleneckSnapshot } from "../ipc/types";

// BottleneckStatTile tests (issue #308): the tile is a pure render of the
// debounced snapshot prop, so every one of the six states - plus the
// not-yet-hydrated (null) state - is drivable without a backend or store.

const TILE = '[data-testid="bottleneck-tile"]';
const VALUE = '[data-testid="bottleneck-value"]';
const SUB = '[data-testid="bottleneck-sub"]';

function snap(state: BottleneckSnapshot["state"], extra: Partial<BottleneckSnapshot> = {}) {
  return {
    tsMs: 0,
    state,
    rateBytesPerSec: null,
    backend: null,
    backoffRemainingMs: null,
    ...extra,
  };
}

function mountTile(snapshot: BottleneckSnapshot | null) {
  return mount(BottleneckStatTile, {
    props: { snapshot },
    global: { plugins: [i18n] },
  });
}

describe("BottleneckStatTile", () => {
  it("renders a placeholder before the store has hydrated", () => {
    const wrapper = mountTile(null);
    expect(wrapper.find(TILE).exists()).toBe(true);
    expect(wrapper.find(VALUE).text()).toBe("...");
    expect(wrapper.find(SUB).exists()).toBe(false);
  });

  it("not_backing_up: no sub-line", () => {
    const wrapper = mountTile(snap("not_backing_up"));
    expect(wrapper.find(VALUE).text()).toBe("Not backing up");
    expect(wrapper.find(SUB).exists()).toBe(false);
  });

  it("disk: names the rate as read-bound", () => {
    const wrapper = mountTile(snap("disk", { rateBytesPerSec: 210_000_000 }));
    expect(wrapper.find(VALUE).text()).toBe("Disk");
    expect(wrapper.find(SUB).text()).toBe("read-bound · 200.3 MB/s");
  });

  it("network: names the rate as upload-bound", () => {
    const wrapper = mountTile(snap("network", { rateBytesPerSec: 42_000_000 }));
    expect(wrapper.find(VALUE).text()).toBe("Network");
    expect(wrapper.find(SUB).text()).toBe("upload-bound · 40.1 MB/s");
  });

  it("cpu: names the rate as hash-bound", () => {
    const wrapper = mountTile(snap("cpu", { rateBytesPerSec: 900_000_000 }));
    expect(wrapper.find(VALUE).text()).toBe("CPU");
    expect(wrapper.find(SUB).text()).toBe("hash-bound · 858.3 MB/s");
  });

  it("mixed: no clear limiter", () => {
    const wrapper = mountTile(snap("mixed"));
    expect(wrapper.find(VALUE).text()).toBe("Mixed");
    expect(wrapper.find(SUB).text()).toBe("no clear limiter");
  });

  it("api: names the backend and the remaining backoff in whole seconds", () => {
    const wrapper = mountTile(snap("api", { backend: "Drive", backoffRemainingMs: 8_400 }));
    expect(wrapper.find(VALUE).text()).toBe("API");
    expect(wrapper.find(SUB).text()).toBe("Drive rate-limited · backing off 8s");
  });

  it("api: falls back to a generic backend label when the wire omits it", () => {
    const wrapper = mountTile(snap("api", { backend: null, backoffRemainingMs: 1_000 }));
    expect(wrapper.find(SUB).text()).toBe("the destination rate-limited · backing off 1s");
  });

  it("a rate-bearing state with no rate yet renders no sub-line rather than a bogus one", () => {
    const wrapper = mountTile(snap("disk", { rateBytesPerSec: null }));
    expect(wrapper.find(SUB).exists()).toBe(false);
  });
});
