<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";

// The Grafana-style STAT TILE WITH A SPARKLINE used by the Activity dashboard
// header (DESIGN s8.3 header aggregates): a headline number with the last few
// minutes of the same measure filling the tile behind it as a translucent area
// chart. It owns the geometry, the plot, the hover layer and the empty state;
// each concrete tile (throughput, files uploaded) is a thin wrapper that decides
// what the numbers MEAN and how they are worded.
//
// Chart decisions, per the dataviz method:
// - The form is a STAT TILE with a trend, not a chart with a caption. The number
//   is the answer; the sparkline is context for it.
// - ONE series per tile, so no legend - the tile's own label names it - and one
//   hue rather than a categorical assignment. Two tiles side by side keep the
//   SAME hue on purpose: they are two independent stat tiles, not two categories
//   of one chart, so a second hue would imply a comparison that is not there.
// - The value and label wear TEXT tokens, never the series color. A teal number
//   over a teal wash would fail contrast and read as decoration; identity comes
//   from the mark behind the text, not from tinting the text.
// - Marks are thin: a 2px line with `non-scaling-stroke` so the non-uniform
//   viewBox scale cannot fatten it, over a low-opacity fill.
// - The hue is teal-600 (#0d9488) in BOTH modes, which is the step that clears
//   the palette validator's lightness band and its >= 3:1 contrast check against
//   the light AND dark chart surfaces. The app's usual dark accent (teal-400)
//   fails the dark lightness band, and teal-500 only warns on light contrast, so
//   the line is deliberately one step off the shell's accent. The area keeps a
//   low-opacity wash - it is the recessive half of the mark, and the line is
//   what carries the reading.
// - The plot has a hover layer (crosshair + tooltip): a chart the user can point
//   at is the default, not an upgrade.
//
// Everything is plain SVG + CSS. The repo ships no charting library and a
// 30-point sparkline is not a reason to add one.

const props = defineProps<{
  /** The measure's name, printed as the tile's label. */
  label: string;
  /** The measure per bucket over the rolling window, OLDEST first. */
  series: number[];
  /** Width of one bucket in ms (the series' time resolution). */
  bucketMs: number;
  /** The headline, ALREADY formatted by the wrapper, or null while it is
   * unknown (the tile then prints `emptyLabel` in a muted tone). The wrapper
   * formats it so the big number and the hover readout - which it formats with
   * the same function - can never drift into different units. */
  headline: string | null;
  /** What to print instead of the headline while it is unknown. */
  emptyLabel: string;
  /** Formats one bucket's raw value for the hover readout. */
  formatBucket: (value: number) => string;
  /** Non-visual description of the whole plot (the SVG's aria-label). */
  chartLabel: string;
  /** Prefix for this tile's `data-testid`s, so two tiles on one page stay
   * individually addressable. */
  testidPrefix: string;
}>();

const { locale } = useI18n();

// Shared design-system tile chrome - the same string the other stat tiles in
// Activity.vue use, plus the clipping + stacking context the chart needs.
const STAT_TILE =
  "relative overflow-hidden rounded-lg border border-zinc-200 bg-white p-3 shadow-xs dark:border-zinc-800 dark:bg-zinc-900";

/** Plot geometry, in viewBox units. The viewBox is stretched to the tile with
 * `preserveAspectRatio="none"`, so these are effectively percentages. */
const VIEW_W = 100;
const VIEW_H = 100;
/** Fraction of the plot height the tallest bucket occupies. The headroom keeps
 * the peak from colliding with the value printed over it. */
const PEAK_HEIGHT = 0.72;

const hoverIndex = ref<number | null>(null);

/** True when there is no shape to draw: no buckets, or every bucket idle. A
 * flat all-zero series is NOT drawn as a line pinned to the baseline - that
 * reads as "the measure is exactly this low", when the honest statement is
 * "nothing happened". */
const isEmpty = computed<boolean>(
  () => props.series.length < 2 || props.series.every((b) => b <= 0)
);

/** Largest bucket in the window; the y-scale's top. */
const peak = computed<number>(() => Math.max(0, ...props.series));

/** Each bucket as an {x, y} point in viewBox units, oldest to newest. */
const points = computed<{ x: number; y: number }[]>(() => {
  const n = props.series.length;
  if (n < 2) return [];
  const max = peak.value;
  const step = VIEW_W / (n - 1);
  return props.series.map((value, i) => {
    const fraction = max > 0 ? Math.max(0, value) / max : 0;
    return { x: i * step, y: VIEW_H - fraction * VIEW_H * PEAK_HEIGHT };
  });
});

/** The trend line: a polyline through every bucket. */
const linePath = computed<string>(() =>
  points.value.map((p, i) => `${i === 0 ? "M" : "L"}${p.x.toFixed(2)},${p.y.toFixed(2)}`).join(" ")
);

/** The filled area: the same line, closed down to the baseline. */
const areaPath = computed<string>(() => {
  const pts = points.value;
  if (pts.length === 0) return "";
  return `M0,${VIEW_H} ${linePath.value.slice(1)} L${VIEW_W},${VIEW_H} Z`;
});

/** The hovered bucket's value, formatted by the wrapper's `formatBucket` (so it
 * reads in the same unit as the headline). Null when nothing is hovered. */
const hoveredValue = computed<string | null>(() => {
  const i = hoverIndex.value;
  if (i === null) return null;
  const raw = props.series[i];
  return raw === undefined ? null : props.formatBucket(raw);
});

/** How long ago the hovered bucket was, localized ("2 minutes ago"). */
const hoveredAge = computed<string | null>(() => {
  const i = hoverIndex.value;
  if (i === null) return null;
  const secondsAgo = Math.round(((props.series.length - 1 - i) * props.bucketMs) / 1000);
  const rtf = new Intl.RelativeTimeFormat(locale.value, { numeric: "auto" });
  return secondsAgo >= 60
    ? rtf.format(-Math.round(secondsAgo / 60), "minute")
    : rtf.format(-secondsAgo, "second");
});

/** Horizontal position of the hovered bucket, as a CSS percentage. */
const hoverLeftPct = computed<number>(() => {
  const i = hoverIndex.value;
  if (i === null || props.series.length < 2) return 0;
  return (i / (props.series.length - 1)) * 100;
});

/** Vertical position of the hovered point, as a CSS percentage. */
const hoverTopPct = computed<number>(() => {
  const i = hoverIndex.value;
  return i === null ? 0 : (points.value[i]?.y ?? VIEW_H);
});

/** Map a pointer position over the plot to the nearest bucket. */
function onPointerMove(event: PointerEvent): void {
  const n = props.series.length;
  if (n < 2 || isEmpty.value) return;
  const rect = (event.currentTarget as HTMLElement).getBoundingClientRect();
  if (rect.width <= 0) return;
  const fraction = (event.clientX - rect.left) / rect.width;
  const i = Math.round(fraction * (n - 1));
  hoverIndex.value = Math.min(n - 1, Math.max(0, i));
}

function clearHover(): void {
  hoverIndex.value = null;
}
</script>

<template>
  <div
    :class="STAT_TILE"
    :data-testid="`${testidPrefix}-tile`"
    @pointermove="onPointerMove"
    @pointerleave="clearHover"
  >
    <!-- The plot, behind the text. Absolutely positioned so the tile's height is
         set by its label + value, exactly like the sibling stat tiles. -->
    <svg
      v-if="!isEmpty"
      class="pointer-events-none absolute inset-x-0 bottom-0 h-full w-full"
      :viewBox="`0 0 ${VIEW_W} ${VIEW_H}`"
      preserveAspectRatio="none"
      role="img"
      :aria-label="chartLabel"
      :data-testid="`${testidPrefix}-sparkline`"
    >
      <path :d="areaPath" class="fill-teal-600/15 dark:fill-teal-500/20" />
      <path
        :d="linePath"
        fill="none"
        stroke-width="2"
        stroke-linejoin="round"
        stroke-linecap="round"
        vector-effect="non-scaling-stroke"
        class="stroke-teal-600"
      />
    </svg>

    <!-- Hover crosshair + point. Plain HTML rather than SVG marks: the viewBox
         is stretched non-uniformly, which would squash a circle into an ellipse. -->
    <template v-if="hoverIndex !== null && !isEmpty">
      <div
        class="pointer-events-none absolute inset-y-0 w-px bg-teal-600/40"
        :style="{ left: `${hoverLeftPct}%` }"
        aria-hidden="true"
      ></div>
      <div
        class="pointer-events-none absolute h-2 w-2 -translate-x-1/2 -translate-y-1/2 rounded-full bg-teal-600 ring-2 ring-white dark:ring-zinc-900"
        :style="{ left: `${hoverLeftPct}%`, top: `${hoverTopPct}%` }"
        aria-hidden="true"
      ></div>
    </template>

    <div class="relative">
      <dt class="text-xs text-zinc-500 dark:text-zinc-400">
        {{ label }}
      </dt>
      <dd class="mt-1 text-lg font-semibold">
        <span v-if="hoveredValue !== null" :data-testid="`${testidPrefix}-hover`">
          {{ hoveredValue }}
          <span class="ml-1 text-xs font-normal text-zinc-500 dark:text-zinc-400">
            {{ hoveredAge }}
          </span>
        </span>
        <span v-else-if="headline !== null" :data-testid="`${testidPrefix}-value`">
          {{ headline }}
        </span>
        <span
          v-else
          class="text-zinc-400 dark:text-zinc-500"
          :data-testid="`${testidPrefix}-unknown`"
        >
          {{ emptyLabel }}
        </span>
      </dd>
    </div>
  </div>
</template>
