// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { mount } from "@vue/test-utils";

import { i18n } from "../i18n";
import SparklineStatTile from "../components/SparklineStatTile.vue";

// SparklineStatTile tests. This is the shared chart both header tiles are built
// from, so it is tested on its OWN contract - geometry, hover mapping, the empty
// state, the testid prefixing - with deliberately unit-free props, rather than
// through either wrapper's wording. A regression here would otherwise only show
// up as a subtly wrong shape in two places at once.

const TILE = '[data-testid="demo-tile"]';
const SVG = '[data-testid="demo-sparkline"]';
const VALUE = '[data-testid="demo-value"]';
const HOVER = '[data-testid="demo-hover"]';
const UNKNOWN = '[data-testid="demo-unknown"]';

/** The plot's baseline and its peak, in viewBox units (PEAK_HEIGHT = 0.72). */
const BASELINE_Y = 100;
const PEAK_Y = 28;

function mountTile(props: {
  series: number[];
  bucketMs?: number;
  headline?: string | null;
  formatBucket?: (v: number) => string;
  testidPrefix?: string;
}) {
  return mount(SparklineStatTile, {
    props: {
      label: "Demo",
      bucketMs: 10_000,
      headline: "42",
      emptyLabel: "Nothing yet",
      formatBucket: (v: number) => `${v} units`,
      chartLabel: "Demo chart",
      testidPrefix: "demo",
      ...props,
    },
    global: { plugins: [i18n] },
  });
}

/** jsdom gives every element a zero-size rect, which the pointer handler treats
 * as "not laid out yet" and ignores. Give the tile a real 200px width so a
 * pointer at clientX maps to a bucket the way it would in a browser. */
function stubLayout(width = 200): void {
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockReturnValue({
    x: 0,
    y: 0,
    width,
    height: 60,
    top: 0,
    left: 0,
    right: width,
    bottom: 60,
    toJSON: () => ({}),
  } as DOMRect);
}

beforeEach(() => {
  stubLayout();
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("SparklineStatTile", () => {
  it("prints the label and the pre-formatted headline", () => {
    const wrapper = mountTile({ series: [0, 1], headline: "42" });
    expect(wrapper.find(TILE).text()).toContain("Demo");
    expect(wrapper.find(VALUE).text()).toBe("42");
  });

  it("falls back to the empty label when the headline is unknown", () => {
    const wrapper = mountTile({ series: [], headline: null });
    expect(wrapper.find(UNKNOWN).text()).toBe("Nothing yet");
    expect(wrapper.find(VALUE).exists()).toBe(false);
  });

  it("scales the peak bucket to the top of the plot and idle buckets to the baseline", () => {
    const wrapper = mountTile({ series: [0, 250, 1000] });
    const line = wrapper.findAll(`${SVG} path`)[1].attributes("d");
    // Three buckets across a 100-wide viewBox: x = 0, 50, 100.
    // y: 0 -> baseline; 1000 (the peak) -> PEAK_Y; 250 -> a quarter up.
    expect(line).toBe(`M0.00,${BASELINE_Y}.00 L50.00,82.00 L100.00,${PEAK_Y}.00`);
  });

  it("keeps a 2px non-scaling stroke so the stretched viewBox cannot fatten it", () => {
    const wrapper = mountTile({ series: [0, 1000] });
    const line = wrapper.findAll(`${SVG} path`)[1];
    expect(line.attributes("stroke-width")).toBe("2");
    expect(line.attributes("vector-effect")).toBe("non-scaling-stroke");
  });

  it("draws nothing when every bucket is idle or the series is too short", () => {
    // A flat all-zero series must NOT plot a line pinned to the baseline - that
    // reads as "the measure is exactly this low" rather than "nothing happened".
    expect(mountTile({ series: [0, 0, 0] }).find(SVG).exists()).toBe(false);
    expect(mountTile({ series: [] }).find(SVG).exists()).toBe(false);
    expect(mountTile({ series: [4096] }).find(SVG).exists()).toBe(false);
  });

  it("exposes the caller's chart description to assistive tech", () => {
    const wrapper = mountTile({ series: [0, 5] });
    expect(wrapper.find(SVG).attributes("aria-label")).toBe("Demo chart");
    expect(wrapper.find(SVG).attributes("role")).toBe("img");
  });

  it("snaps hover to the nearest bucket and formats it with the caller's function", async () => {
    const wrapper = mountTile({ series: [10, 0, 30], bucketMs: 10_000 });

    // 200px wide, 3 buckets -> the right edge is the newest bucket.
    await wrapper.find(TILE).trigger("pointermove", { clientX: 200 });
    expect(wrapper.find(HOVER).text()).toContain("30 units");
    expect(wrapper.find(HOVER).text()).toContain("now");

    // The far left is the oldest of 3 buckets: 2 x 10s ago.
    await wrapper.find(TILE).trigger("pointermove", { clientX: 0 });
    expect(wrapper.find(HOVER).text()).toContain("10 units");
    expect(wrapper.find(HOVER).text()).toContain("20 seconds ago");
  });

  it("restores the headline when the pointer leaves", async () => {
    const wrapper = mountTile({ series: [10, 20], headline: "42" });
    await wrapper.find(TILE).trigger("pointermove", { clientX: 10 });
    expect(wrapper.find(HOVER).exists()).toBe(true);

    await wrapper.find(TILE).trigger("pointerleave");
    expect(wrapper.find(HOVER).exists()).toBe(false);
    expect(wrapper.find(VALUE).text()).toBe("42");
  });

  it("ignores hover on an empty plot (there is nothing to point at)", async () => {
    const wrapper = mountTile({ series: [0, 0, 0] });
    await wrapper.find(TILE).trigger("pointermove", { clientX: 100 });
    expect(wrapper.find(HOVER).exists()).toBe(false);
    expect(wrapper.find(VALUE).exists()).toBe(true);
  });

  it("namespaces every testid by the prefix so two tiles stay addressable", () => {
    const wrapper = mountTile({ series: [0, 1], testidPrefix: "other" });
    expect(wrapper.find('[data-testid="other-tile"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="other-sparkline"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="other-value"]').exists()).toBe(true);
    expect(wrapper.find(TILE).exists()).toBe(false);
  });
});
