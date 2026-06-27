import { describe, it, expect } from "vitest";

import { computeVirtualRange } from "../composables/useVirtualList";

// Pure windowing math for the Restore list virtualization (no DOM). These cover
// the visible-range calculation, the spacer invariant (top + window + bottom ==
// full content height), and that a huge list mounts only a bounded slice.

const ITEM_HEIGHT = 40;
const VIEWPORT = 800; // viewport height in px
const OVERSCAN = 6;

/** The number of rows the window mounts. */
function windowSize(r: { startIndex: number; endIndex: number }): number {
  return r.endIndex - r.startIndex;
}

describe("computeVirtualRange", () => {
  it("returns an empty window for an empty list", () => {
    const r = computeVirtualRange(0, VIEWPORT, 0, ITEM_HEIGHT, OVERSCAN);
    expect(r).toEqual({ startIndex: 0, endIndex: 0, paddingTop: 0, paddingBottom: 0 });
  });

  it("renders the whole (small) list when it fits with no scroll", () => {
    const count = 5;
    const r = computeVirtualRange(0, VIEWPORT, count, ITEM_HEIGHT, OVERSCAN);
    expect(r.startIndex).toBe(0);
    expect(r.endIndex).toBe(count);
    expect(r.paddingTop).toBe(0);
    expect(r.paddingBottom).toBe(0);
  });

  it("at the top of a large list mounts only the window + overscan", () => {
    const count = 10000;
    const r = computeVirtualRange(0, VIEWPORT, count, ITEM_HEIGHT, OVERSCAN);
    expect(r.startIndex).toBe(0);
    // ceil(800/40) = 20 visible rows, + overscan below.
    expect(r.endIndex).toBe(20 + OVERSCAN);
    expect(r.paddingTop).toBe(0);
    expect(r.paddingBottom).toBe((count - r.endIndex) * ITEM_HEIGHT);
    // The DOM stays tiny relative to the list.
    expect(windowSize(r)).toBeLessThanOrEqual(20 + 2 * OVERSCAN);
    expect(windowSize(r)).toBeLessThan(count);
  });

  it("windows around the scroll position once scrolled into the list", () => {
    const count = 10000;
    // The list top has scrolled 100 rows (4000px) above the viewport top.
    const listTop = -100 * ITEM_HEIGHT;
    const r = computeVirtualRange(listTop, VIEWPORT, count, ITEM_HEIGHT, OVERSCAN);
    // floor(4000/40) = 100 first-visible, minus overscan above.
    expect(r.startIndex).toBe(100 - OVERSCAN);
    // ceil((4000 + 800) / 40) = 120 last-visible, plus overscan below.
    expect(r.endIndex).toBe(120 + OVERSCAN);
    expect(r.paddingTop).toBe(r.startIndex * ITEM_HEIGHT);
    expect(windowSize(r)).toBeLessThanOrEqual(20 + 2 * OVERSCAN);
  });

  it("clamps to the end when scrolled to the bottom of the list", () => {
    const count = 1000;
    const contentHeight = count * ITEM_HEIGHT;
    // Scrolled so the whole list is above the viewport bottom (bottom reached).
    const listTop = -(contentHeight - VIEWPORT);
    const r = computeVirtualRange(listTop, VIEWPORT, count, ITEM_HEIGHT, OVERSCAN);
    expect(r.endIndex).toBe(count);
    expect(r.paddingBottom).toBe(0);
    expect(windowSize(r)).toBeLessThanOrEqual(20 + 2 * OVERSCAN);
  });

  it("renders only a bounded slice when the list is entirely above the viewport", () => {
    const count = 5000;
    // Scrolled far past the end (list fully above the viewport top).
    const r = computeVirtualRange(-1e9, VIEWPORT, count, ITEM_HEIGHT, OVERSCAN);
    expect(windowSize(r)).toBeLessThanOrEqual(2 * OVERSCAN);
    expect(r.endIndex).toBe(count);
  });

  it("renders only a bounded slice when the list is entirely below the viewport", () => {
    const count = 5000;
    // The list starts far below the viewport bottom (not scrolled to yet).
    const r = computeVirtualRange(1e9, VIEWPORT, count, ITEM_HEIGHT, OVERSCAN);
    expect(r.startIndex).toBe(0);
    expect(windowSize(r)).toBeLessThanOrEqual(2 * OVERSCAN);
  });

  it("keeps the spacer invariant: top + window + bottom == full height", () => {
    const count = 3333;
    const contentHeight = count * ITEM_HEIGHT;
    for (const listTop of [0, -37, -4000, -123456, 250, -(contentHeight - VIEWPORT)]) {
      const r = computeVirtualRange(listTop, VIEWPORT, count, ITEM_HEIGHT, OVERSCAN);
      const windowHeight = windowSize(r) * ITEM_HEIGHT;
      expect(r.paddingTop + windowHeight + r.paddingBottom).toBe(contentHeight);
      expect(r.startIndex).toBeGreaterThanOrEqual(0);
      expect(r.endIndex).toBeLessThanOrEqual(count);
      expect(r.endIndex).toBeGreaterThanOrEqual(r.startIndex);
    }
  });

  it("falls back to rendering everything when it cannot measure (zero height)", () => {
    const count = 42;
    // Zero item height or zero viewport => render all (correctness over savings).
    expect(computeVirtualRange(0, VIEWPORT, count, 0, OVERSCAN)).toEqual({
      startIndex: 0,
      endIndex: count,
      paddingTop: 0,
      paddingBottom: 0,
    });
    expect(computeVirtualRange(0, 0, count, ITEM_HEIGHT, OVERSCAN)).toEqual({
      startIndex: 0,
      endIndex: count,
      paddingTop: 0,
      paddingBottom: 0,
    });
  });
});
