// @vitest-environment jsdom
import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { mount } from "@vue/test-utils";

import { i18n } from "../i18n";
import FilesUploadedStatTile from "../components/FilesUploadedStatTile.vue";
import { pointerMoveAt } from "./pointer";

// FilesUploadedStatTile tests. The tile is a pure render of its props (files per
// bucket + bucket width + headline count), so every branch is drivable without a
// backend. What is specific to THIS tile - as opposed to its throughput sibling,
// which the shared SparklineStatTile tests cover geometrically - is the unit: a
// count, pluralized and locale-grouped, never divided by the bucket seconds.

const SVG = '[data-testid="files-uploaded-sparkline"]';
const TILE = '[data-testid="files-uploaded-tile"]';
const VALUE = '[data-testid="files-uploaded-value"]';
const HOVER = '[data-testid="files-uploaded-hover"]';
const UNKNOWN = '[data-testid="files-uploaded-unknown"]';

function mountTile(props: { series: number[]; bucketMs?: number; filesUploaded?: number | null }) {
  return mount(FilesUploadedStatTile, {
    props: {
      bucketMs: 10_000,
      filesUploaded: 0,
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

describe("FilesUploadedStatTile", () => {
  it("leads with the file count for the window", () => {
    const wrapper = mountTile({ series: [0, 2, 5], filesUploaded: 7 });
    expect(wrapper.find(VALUE).text()).toBe("7 files");
  });

  it("says so when the count is not known yet", () => {
    const wrapper = mountTile({ series: [], filesUploaded: null });
    expect(wrapper.find(UNKNOWN).text()).toBe("No recent uploads");
    expect(wrapper.find(VALUE).exists()).toBe(false);
  });

  it("uses the singular form for exactly one file", () => {
    expect(
      mountTile({ series: [0, 1], filesUploaded: 1 })
        .find(VALUE)
        .text()
    ).toBe("1 file");
    // Zero is plural in English ("0 files"), which is what the catalog's two
    // forms produce - a bare "0 file" would read as a bug.
    expect(
      mountTile({ series: [0, 1], filesUploaded: 0 })
        .find(VALUE)
        .text()
    ).toBe("0 files");
  });

  it("groups a large count with Intl rather than printing raw digits", () => {
    const wrapper = mountTile({ series: [0, 4], filesUploaded: 12_345 });
    // en-US grouping; the assertion is that SOME locale grouping happened, not
    // that a hand-rolled formatter inserted commas.
    expect(wrapper.find(VALUE).text()).toBe(
      `${new Intl.NumberFormat("en-US").format(12_345)} files`
    );
  });

  it("plots the series as an area plus a trend line", () => {
    const wrapper = mountTile({ series: [0, 3, 9] });
    const paths = wrapper.findAll(`${SVG} path`);
    expect(paths).toHaveLength(2);
    // The area is closed back down to the baseline; the line is not.
    expect(paths[0].attributes("d")).toMatch(/^M0,100 /);
    expect(paths[0].attributes("d")).toMatch(/Z$/);
    expect(paths[1].attributes("d")).not.toMatch(/Z$/);
  });

  it("draws nothing when no files were uploaded in the window", () => {
    const wrapper = mountTile({ series: [0, 0, 0], filesUploaded: 0 });
    expect(wrapper.find(SVG).exists()).toBe(false);
    // The tile itself still renders, with its headline.
    expect(wrapper.find(TILE).exists()).toBe(true);
    expect(wrapper.find(VALUE).text()).toBe("0 files");
  });

  it("describes the plot for a screen reader in files, not a rate", () => {
    const wrapper = mountTile({ series: [0, 12], bucketMs: 10_000 });
    const label = wrapper.find(SVG).attributes("aria-label");
    expect(label).toContain("Files uploaded over the last");
    // The peak reads as a per-interval COUNT - dividing it by the bucket seconds
    // (the throughput tile's job) would print a meaningless "1.2 files/s".
    expect(label).toContain("peaking at 12 in a single 10-second interval");
    expect(wrapper.find(SVG).attributes("role")).toBe("img");
  });

  it("reads a hovered bucket as a plain count, not a per-second rate", async () => {
    // 5 buckets of 10s; hovering the far left is the oldest (40s ago).
    const wrapper = mountTile({ series: [6, 0, 0, 0, 2], bucketMs: 10_000, filesUploaded: 8 });
    await pointerMoveAt(wrapper.find(TILE).element, 0);

    const hover = wrapper.find(HOVER);
    expect(hover.exists()).toBe(true);
    // 6 files in the bucket - NOT 6/10 = 0.6 files per second.
    expect(hover.text()).toContain("6 files");
    expect(hover.text()).toContain("40 seconds ago");
    // The steady-state headline is hidden while a bucket is hovered.
    expect(wrapper.find(VALUE).exists()).toBe(false);

    await wrapper.find(TILE).trigger("pointerleave");
    expect(wrapper.find(HOVER).exists()).toBe(false);
    expect(wrapper.find(VALUE).text()).toBe("8 files");
  });
});
