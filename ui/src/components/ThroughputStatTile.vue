<script setup lang="ts">
import { computed, ref } from "vue";
import { useI18n } from "vue-i18n";

import { formatBytes } from "../stores/formatBytes";

// The "Current throughput" stat tile (DESIGN s8.3 header aggregates), rendered
// as a stat panel: the rate is the headline, and the last few minutes of upload
// throughput fill the tile behind it as a translucent area chart.
//
// Chart decisions, per the dataviz method:
// - The form is a STAT TILE with a trend, not a chart with a caption. The number
//   is the answer; the sparkline is context for it.
// - ONE series, so no legend - the tile's own label names it - and one hue (the
//   app's teal accent) rather than a categorical assignment.
// - The value and label wear TEXT tokens, never the series color. A teal number
//   over a teal wash would fail contrast and read as decoration; identity comes
//   from the mark behind the text, not from tinting the text.
// - Marks are thin: a 2px line with `non-scaling-stroke` so the non-uniform
//   viewBox scale cannot fatten it, over a low-opacity fill.
// - The plot has a hover layer (crosshair + tooltip): a chart the user can point
//   at is the default, not an upgrade.
//
// Everything is plain SVG + CSS. The repo ships no charting library and a
// 30-point sparkline is not a reason to add one.

const props = defineProps<{
  /** Bytes uploaded per bucket over the rolling window, OLDEST first. */
  series: number[];
  /** Width of one bucket in ms (the series' time resolution). */
  bucketMs: number;
  /** The headline rate in bytes/sec, or null while it is unknown. */
  ratePerSecond: number | null;
}>();

const { t, locale } = useI18n();

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

/** The headline rate, formatted, or null when there is nothing to show yet. */
const rateLabel = computed<string | null>(() =>
  props.ratePerSecond === null ? null : formatBytes(props.ratePerSecond, locale.value)
);

/** True when there is no shape to draw: no buckets, or every bucket idle. A
 * flat all-zero series is NOT drawn as a line pinned to the baseline - that
 * reads as "throughput is exactly this low", when the honest statement is
 * "nothing was uploaded". */
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
  return props.series.map((bytes, i) => {
    const fraction = max > 0 ? Math.max(0, bytes) / max : 0;
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

/** Bytes in the hovered bucket, or null when nothing is hovered. */
const hoveredBytes = computed<number | null>(() => {
  const i = hoverIndex.value;
  return i === null ? null : (props.series[i] ?? null);
});

/** The hovered bucket's rate, formatted as bytes/sec (the same unit as the
 * headline, so the tooltip and the big number are directly comparable). */
const hoveredRate = computed<string | null>(() => {
  const bytes = hoveredBytes.value;
  if (bytes === null || props.bucketMs <= 0) return null;
  return formatBytes(bytes / (props.bucketMs / 1000), locale.value);
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

/** Non-visual summary of the plot, so the trend is available to a screen reader
 * (and to anyone for whom the wash behind the number is not legible). */
const chartSummary = computed<string>(() =>
  t("activity.summary.sparklineLabel", {
    minutes: Math.round((props.series.length * props.bucketMs) / 60000),
    peak: formatBytes(props.bucketMs > 0 ? peak.value / (props.bucketMs / 1000) : 0, locale.value),
  })
);

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
    data-testid="throughput-tile"
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
      :aria-label="chartSummary"
      data-testid="throughput-sparkline"
    >
      <path :d="areaPath" class="fill-teal-500/15 dark:fill-teal-400/15" />
      <path
        :d="linePath"
        fill="none"
        stroke-width="2"
        stroke-linejoin="round"
        stroke-linecap="round"
        vector-effect="non-scaling-stroke"
        class="stroke-teal-500/70 dark:stroke-teal-400/70"
      />
    </svg>

    <!-- Hover crosshair + point. Plain HTML rather than SVG marks: the viewBox
         is stretched non-uniformly, which would squash a circle into an ellipse. -->
    <template v-if="hoverIndex !== null && !isEmpty">
      <div
        class="pointer-events-none absolute inset-y-0 w-px bg-teal-500/40 dark:bg-teal-400/40"
        :style="{ left: `${hoverLeftPct}%` }"
        aria-hidden="true"
      ></div>
      <div
        class="pointer-events-none absolute h-2 w-2 -translate-x-1/2 -translate-y-1/2 rounded-full bg-teal-600 ring-2 ring-white dark:bg-teal-400 dark:ring-zinc-900"
        :style="{ left: `${hoverLeftPct}%`, top: `${hoverTopPct}%` }"
        aria-hidden="true"
      ></div>
    </template>

    <div class="relative">
      <dt class="text-xs text-zinc-500 dark:text-zinc-400">
        {{ t("activity.summary.throughput") }}
      </dt>
      <dd class="mt-1 text-lg font-semibold">
        <span v-if="hoveredRate !== null" data-testid="throughput-hover">
          {{ t("activity.summary.perSecond", { rate: hoveredRate }) }}
          <span class="ml-1 text-xs font-normal text-zinc-500 dark:text-zinc-400">
            {{ hoveredAge }}
          </span>
        </span>
        <span v-else-if="rateLabel !== null" data-testid="throughput-rate">
          {{ t("activity.summary.perSecond", { rate: rateLabel }) }}
        </span>
        <span v-else class="text-zinc-400 dark:text-zinc-500" data-testid="throughput-unknown">
          {{ t("activity.summary.noThroughput") }}
        </span>
      </dd>
    </div>
  </div>
</template>
