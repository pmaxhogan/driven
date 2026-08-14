<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";

import SparklineStatTile from "./SparklineStatTile.vue";
import { formatBytes } from "../stores/formatBytes";

// The "Current throughput" stat tile (DESIGN s8.3 header aggregates): the upload
// rate as the headline, with the last few minutes of throughput filling the tile
// behind it.
//
// The chart itself - geometry, marks, hover layer, empty state - lives in
// SparklineStatTile, which this component and FilesUploadedStatTile share. What
// is left here is the only thing that is actually about throughput: turning
// bytes into a RATE. The tile plots raw bytes-per-bucket (so the shape is the
// shape of the data), and every number the user reads is divided by the bucket
// seconds, so the headline, the hover readout and the screen-reader summary are
// all in bytes/sec and directly comparable.

const props = withDefaults(
  defineProps<{
    /** Bytes per bucket over the rolling window, OLDEST first. */
    series: number[];
    /** Width of one bucket in ms (the series' time resolution). */
    bucketMs: number;
    /** The headline rate in bytes/sec, or null while it is unknown. */
    ratePerSecond: number | null;
    /** Which throughput this tile shows (2026-08-14 follow-up: the graph
     * split). "net" = wire bytes the destination acked (the original tile);
     * "disk" = plaintext bytes Driven read from local files. Only the copy
     * and testids differ - every key is a literal at its call site so the
     * i18n no-unused-keys lint can see them. */
    variant?: "net" | "disk";
  }>(),
  { variant: "net" }
);

const { t, locale } = useI18n();

/** Seconds per bucket - the divisor that turns a bucket's bytes into a rate.
 * Zero-guarded so a degenerate bucket width can never produce Infinity. */
const bucketSeconds = computed<number>(() => (props.bucketMs > 0 ? props.bucketMs / 1000 : 0));

/** Format a bytes/sec rate the way this tile words it ("1.5 KB/s"). */
function formatRate(bytesPerSecond: number): string {
  return t("activity.summary.perSecond", {
    rate: formatBytes(bytesPerSecond, locale.value),
  });
}

/** The headline rate, formatted, or null when there is nothing to show yet. */
const headline = computed<string | null>(() =>
  props.ratePerSecond === null ? null : formatRate(props.ratePerSecond)
);

/** A hovered bucket reads as a rate too (the same unit as the headline, so the
 * tooltip and the big number are directly comparable). */
function formatBucket(bytes: number): string {
  return formatRate(bucketSeconds.value > 0 ? bytes / bucketSeconds.value : 0);
}

/** Non-visual summary of the plot, so the trend is available to a screen reader
 * (and to anyone for whom the wash behind the number is not legible). */
const chartLabel = computed<string>(() => {
  const peak = Math.max(0, ...props.series);
  const args = {
    minutes: Math.round((props.series.length * props.bucketMs) / 60000),
    peak: formatBytes(bucketSeconds.value > 0 ? peak / bucketSeconds.value : 0, locale.value),
  };
  return props.variant === "disk"
    ? t("activity.summary.diskSparklineLabel", args)
    : t("activity.summary.sparklineLabel", args);
});

const label = computed<string>(() =>
  props.variant === "disk" ? t("activity.summary.diskThroughput") : t("activity.summary.throughput")
);
const emptyLabel = computed<string>(() =>
  props.variant === "disk"
    ? t("activity.summary.noDiskThroughput")
    : t("activity.summary.noThroughput")
);
</script>

<template>
  <SparklineStatTile
    :label="label"
    :series="series"
    :bucket-ms="bucketMs"
    :headline="headline"
    :empty-label="emptyLabel"
    :format-bucket="formatBucket"
    :chart-label="chartLabel"
    :testid-prefix="variant === 'disk' ? 'disk-throughput' : 'throughput'"
  />
</template>
