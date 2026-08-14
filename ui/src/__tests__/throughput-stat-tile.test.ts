// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { mount } from "@vue/test-utils";

import { i18n } from "../i18n";
import ThroughputStatTile from "../components/ThroughputStatTile.vue";
import { pointerMoveAt } from "./pointer";

// ThroughputStatTile tests. The tile is a pure render of its props (series +
// bucket width + headline rate), so every branch - the plotted shape, the empty
// state, the unknown-rate state, and the hover readout - is drivable without a
// backend. The geometry assertions pin the y-scale: a sparkline whose peak
// silently stops scaling is a chart that lies.
//
// The chart itself now lives in the shared SparklineStatTile, so these mount the
// wrapper and assert THROUGH it - which is the point: they pin the behaviour a
// user sees, and would catch the shared tile being wired up wrong.

const SVG = '[data-testid="throughput-sparkline"]';
const TILE = '[data-testid="throughput-tile"]';
const RATE = '[data-testid="throughput-value"]';
const HOVER = '[data-testid="throughput-hover"]';
const UNKNOWN = '[data-testid="throughput-unknown"]';

/** The plot's baseline and its peak, in viewBox units (PEAK_HEIGHT = 0.72). */
const BASELINE_Y = 100;
const PEAK_Y = 28;

function mountTile(props: { series: number[]; bucketMs?: number; ratePerSecond?: number | null }) {
  return mount(ThroughputStatTile, {
    props: {
      bucketMs: 10_000,
      ratePerSecond: 0,
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

describe("ThroughputStatTile", () => {
  it("leads with the headline rate", () => {
    const wrapper = mountTile({ series: [0, 1024, 2048], ratePerSecond: 1536 });
    expect(wrapper.find(RATE).text()).toBe("1.5 KB/s");
  });

  it("says so when the rate is not known yet", () => {
    const wrapper = mountTile({ series: [], ratePerSecond: null });
    expect(wrapper.find(UNKNOWN).text()).toBe("No recent uploads");
    expect(wrapper.find(RATE).exists()).toBe(false);
  });

  it("plots the series as an area plus a trend line", () => {
    const wrapper = mountTile({ series: [0, 500, 1000] });
    const svg = wrapper.find(SVG);
    expect(svg.exists()).toBe(true);

    const paths = svg.findAll("path");
    expect(paths).toHaveLength(2);
    // The area is closed back down to the baseline; the line is not.
    expect(paths[0].attributes("d")).toMatch(/^M0,100 /);
    expect(paths[0].attributes("d")).toMatch(/Z$/);
    expect(paths[1].attributes("d")).toMatch(/^M0\.00,/);
    expect(paths[1].attributes("d")).not.toMatch(/Z$/);
  });

  it("scales the peak bucket to the top of the plot and idle buckets to the baseline", () => {
    const wrapper = mountTile({ series: [0, 250, 1000] });
    const line = wrapper.findAll(`${SVG} path`)[1].attributes("d");
    // Three buckets across a 100-wide viewBox: x = 0, 50, 100.
    // y: 0 bytes -> baseline; 1000 (the peak) -> PEAK_Y; 250 -> a quarter up.
    expect(line).toBe(`M0.00,${BASELINE_Y}.00 L50.00,82.00 L100.00,${PEAK_Y}.00`);
  });

  it("keeps a 2px non-scaling stroke so the stretched viewBox cannot fatten it", () => {
    const wrapper = mountTile({ series: [0, 1000] });
    const line = wrapper.findAll(`${SVG} path`)[1];
    expect(line.attributes("stroke-width")).toBe("2");
    expect(line.attributes("vector-effect")).toBe("non-scaling-stroke");
  });

  it("draws nothing when every bucket is idle (rather than a line pinned to zero)", () => {
    const wrapper = mountTile({ series: [0, 0, 0, 0] });
    expect(wrapper.find(SVG).exists()).toBe(false);
    // The tile itself still renders, with its headline rate.
    expect(wrapper.find(TILE).exists()).toBe(true);
    expect(wrapper.find(RATE).text()).toBe("0 B/s");
  });

  it("draws nothing when the series is too short to have a shape", () => {
    expect(mountTile({ series: [] }).find(SVG).exists()).toBe(false);
    expect(
      mountTile({ series: [4096] })
        .find(SVG)
        .exists()
    ).toBe(false);
  });

  it("describes the plot for a screen reader", () => {
    const wrapper = mountTile({ series: [0, 600_000], bucketMs: 10_000 });
    const label = wrapper.find(SVG).attributes("aria-label");
    // 2 buckets x 10s ~= 0 minutes; the peak is 600000 bytes over 10s = 60 KB/s.
    expect(label).toContain("Upload throughput over the last");
    expect(label).toContain("58.6 KB per second");
    expect(wrapper.find(SVG).attributes("role")).toBe("img");
  });

  describe("hover layer", () => {
    it("swaps the headline for the hovered bucket's rate and age", async () => {
      // 5 buckets of 10s. Hovering the far left is the oldest (40s ago).
      const wrapper = mountTile({
        series: [10_240, 0, 0, 0, 2048],
        bucketMs: 10_000,
        ratePerSecond: 204,
      });
      await pointerMoveAt(wrapper.find(TILE).element, 0);

      const hover = wrapper.find(HOVER);
      expect(hover.exists()).toBe(true);
      // 10240 bytes over a 10s bucket -> 1 KB/s.
      expect(hover.text()).toContain("1 KB/s");
      expect(hover.text()).toContain("40 seconds ago");
      // The steady-state rate is hidden while a bucket is hovered.
      expect(wrapper.find(RATE).exists()).toBe(false);
    });

    it("snaps to the nearest bucket across the plot's width", async () => {
      const wrapper = mountTile({ series: [1024, 0, 20_480], bucketMs: 10_000 });
      // 200px wide, 3 buckets -> the right edge is the newest bucket.
      await pointerMoveAt(wrapper.find(TILE).element, 200);
      expect(wrapper.find(HOVER).text()).toContain("2 KB/s");
      expect(wrapper.find(HOVER).text()).toContain("now");
    });

    it("restores the headline rate when the pointer leaves", async () => {
      const wrapper = mountTile({ series: [1024, 2048], ratePerSecond: 512 });
      await pointerMoveAt(wrapper.find(TILE).element, 10);
      expect(wrapper.find(HOVER).exists()).toBe(true);

      await wrapper.find(TILE).trigger("pointerleave");
      expect(wrapper.find(HOVER).exists()).toBe(false);
      expect(wrapper.find(RATE).text()).toBe("512 B/s");
    });

    it("ignores hover on an empty plot (there is nothing to point at)", async () => {
      const wrapper = mountTile({ series: [0, 0, 0], ratePerSecond: 0 });
      await pointerMoveAt(wrapper.find(TILE).element, 100);
      expect(wrapper.find(HOVER).exists()).toBe(false);
      expect(wrapper.find(RATE).exists()).toBe(true);
    });
  });
});

// --- 2026-08-14 follow-up: the disk variant of the same tile ----------------

describe("disk variant", () => {
  it("renders under disk testids with the disk copy, leaving net untouched", () => {
    const wrapper = mount(ThroughputStatTile, {
      props: {
        series: [1000, 2000],
        bucketMs: 1000,
        ratePerSecond: 1500,
        variant: "disk" as const,
      },
      global: { plugins: [i18n] },
    });
    expect(wrapper.find('[data-testid="disk-throughput-tile"]').exists()).toBe(true);
    expect(wrapper.find('[data-testid="throughput-tile"]').exists()).toBe(false);
    expect(wrapper.text()).toContain("Disk read");
    wrapper.unmount();
  });

  it("defaults to the network variant (the original tile, unchanged)", () => {
    const wrapper = mountTile({ series: [1000], ratePerSecond: 100 });
    expect(wrapper.find('[data-testid="throughput-tile"]').exists()).toBe(true);
    expect(wrapper.text()).toContain("Network throughput");
    wrapper.unmount();
  });
});

