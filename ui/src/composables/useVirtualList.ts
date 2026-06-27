import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import type { ComputedRef, Ref } from "vue";

// Self-contained list virtualization (windowing) for the Restore browser.
//
// A backed-up folder can hold thousands of files; mounting one <li> per row
// makes the view crawl. This composable renders only the rows inside (or near)
// the viewport and replaces the rest with two spacer paddings on the list
// container, so the scrollbar stays the size of the full list while the DOM node
// count stays bounded.
//
// Intentionally dependency-free (no virtual-scroll npm package): the repo tracks
// every dependency via Dependabot and favors a minimal supply chain. The model
// is fixed-height rows + window scrolling (the app's content scrolls on the
// document, with a sticky action bar), so the windowing math only needs the
// list's viewport-relative top, the viewport height, the row count, and a fixed
// per-row height. The pure `computeVirtualRange` is exported so the math is unit
// testable without any DOM.

/** The mounted slice [startIndex, endIndex) plus the spacer paddings (px) that
 * stand in for the rows above / below the window. */
export interface VirtualRange {
  /** First row index to render (inclusive). */
  startIndex: number;
  /** One past the last row index to render (exclusive). */
  endIndex: number;
  /** Height (px) of the top spacer = the rows scrolled above the window. */
  paddingTop: number;
  /** Height (px) of the bottom spacer = the rows below the window. */
  paddingBottom: number;
}

/**
 * Pure windowing math (no DOM).
 *
 * @param listTop        The list container's top relative to the scroll viewport
 *                       top, in px (i.e. `getBoundingClientRect().top`). Negative
 *                       once the list has scrolled up past the viewport top.
 * @param viewportHeight The scroll viewport height in px.
 * @param itemCount      The total number of rows in the full list.
 * @param itemHeight     The fixed per-row height in px.
 * @param overscan       Extra rows to render on each side of the visible window
 *                       (smooths fast scrolling; absorbs sub-pixel drift).
 *
 * The returned spacers always satisfy
 * `paddingTop + (endIndex - startIndex) * itemHeight + paddingBottom ===
 * itemCount * itemHeight`, so the scrollbar matches the full list.
 */
export function computeVirtualRange(
  listTop: number,
  viewportHeight: number,
  itemCount: number,
  itemHeight: number,
  overscan: number
): VirtualRange {
  if (itemCount <= 0) {
    return { startIndex: 0, endIndex: 0, paddingTop: 0, paddingBottom: 0 };
  }
  // Without a usable measurement (no layout yet, or a zero-height viewport) fall
  // back to rendering the whole list rather than nothing - correctness first.
  if (itemHeight <= 0 || viewportHeight <= 0) {
    return { startIndex: 0, endIndex: itemCount, paddingTop: 0, paddingBottom: 0 };
  }
  const contentHeight = itemCount * itemHeight;
  const clamp = (v: number, lo: number, hi: number): number => Math.min(Math.max(v, lo), hi);
  // The visible band expressed in the list's own content coordinates (0 = list
  // top). `-listTop` is how far the list top sits above the viewport top.
  const visibleTop = clamp(-listTop, 0, contentHeight);
  const visibleBottom = clamp(viewportHeight - listTop, 0, contentHeight);
  const startIndex = clamp(Math.floor(visibleTop / itemHeight) - overscan, 0, itemCount);
  const endIndex = clamp(Math.ceil(visibleBottom / itemHeight) + overscan, startIndex, itemCount);
  return {
    startIndex,
    endIndex,
    paddingTop: startIndex * itemHeight,
    paddingBottom: (itemCount - endIndex) * itemHeight,
  };
}

/** What `useVirtualList` hands back to a component. */
export interface UseVirtualList {
  /** Bind to the list container element (the scrolled list's wrapper). */
  containerRef: Ref<HTMLElement | null>;
  /** The reactive window to render (slice bounds + spacer paddings). */
  range: ComputedRef<VirtualRange>;
  /** Force a re-measure (e.g. after content above the list changes height). */
  measure: () => void;
}

/**
 * Window-scroll virtualization for a fixed-row-height list.
 *
 * @param itemCount  A getter for the total row count (reactive source).
 * @param itemHeight The fixed per-row height in px (the list MUST render each row
 *                   at exactly this height for the spacer math to line up).
 * @param overscan   Extra rows rendered on each side of the window (default 6).
 */
export function useVirtualList(
  itemCount: () => number,
  itemHeight: number,
  overscan = 6
): UseVirtualList {
  const containerRef = ref<HTMLElement | null>(null);
  const listTop = ref(0);
  const viewportHeight = ref(typeof window !== "undefined" ? window.innerHeight : 0);

  function measure(): void {
    if (typeof window === "undefined") return;
    viewportHeight.value = window.innerHeight;
    const el = containerRef.value;
    if (el) listTop.value = el.getBoundingClientRect().top;
  }

  // Throttle scroll/resize work to one measure per animation frame so a fast
  // scroll does not run the math on every event.
  let frame = 0;
  function onScrollOrResize(): void {
    if (typeof window === "undefined") return;
    if (typeof window.requestAnimationFrame !== "function") {
      measure();
      return;
    }
    if (frame !== 0) return;
    frame = window.requestAnimationFrame(() => {
      frame = 0;
      measure();
    });
  }

  const range = computed<VirtualRange>(() =>
    computeVirtualRange(listTop.value, viewportHeight.value, itemCount(), itemHeight, overscan)
  );

  // Re-measure after the row count changes (folder navigation / a new search):
  // the list element may have just (re)appeared or moved, and the window must be
  // recomputed against its new position on the next frame.
  watch(itemCount, () => {
    void nextTick(() => measure());
  });

  onMounted(() => {
    measure();
    window.addEventListener("scroll", onScrollOrResize, { passive: true });
    window.addEventListener("resize", onScrollOrResize);
  });

  onUnmounted(() => {
    if (typeof window === "undefined") return;
    window.removeEventListener("scroll", onScrollOrResize);
    window.removeEventListener("resize", onScrollOrResize);
    if (frame !== 0) window.cancelAnimationFrame(frame);
  });

  return { containerRef, range, measure };
}
