<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";

import SparklineStatTile from "./SparklineStatTile.vue";

// The "Files uploaded" stat tile (DESIGN s8.3 header aggregates): how many FILES
// went up in the recent window, with the last few minutes of file counts filling
// the tile behind it. The sibling of the throughput tile - same window, same
// buckets, same chart - answering "how many files" where that one answers "how
// fast".
//
// The chart itself lives in SparklineStatTile (shared with ThroughputStatTile).
// What is specific here is the unit: a COUNT, not a rate. So a bucket is printed
// as the plain number of files in it - NOT divided by the bucket seconds, which
// would turn a naturally lumpy count into a meaningless fractional "files per
// second" - and the count is pluralized and formatted with `Intl.NumberFormat`
// so a locale that groups digits differently reads correctly (DESIGN s8.7).

const props = defineProps<{
  /** Files uploaded per bucket over the rolling window, OLDEST first. */
  series: number[];
  /** Width of one bucket in ms (the series' time resolution). */
  bucketMs: number;
  /** Files uploaded over the headline window, or null while it is unknown. */
  filesUploaded: number | null;
}>();

const { t, locale } = useI18n();

const numberFormatter = computed(() => new Intl.NumberFormat(locale.value));

/** Format a file count the way this tile words it ("1 file" / "1,204 files").
 * The plural form is chosen from the RAW count while the interpolated value is
 * the locale-grouped one, so grouping never breaks pluralization. */
function formatFiles(count: number): string {
  const safe = Math.max(0, Math.round(count));
  return t("activity.summary.filesValue", { count: numberFormatter.value.format(safe) }, safe);
}

/** The headline count, formatted, or null when there is nothing to show yet. */
const headline = computed<string | null>(() =>
  props.filesUploaded === null ? null : formatFiles(props.filesUploaded)
);

/** Non-visual summary of the plot, so the trend is available to a screen reader
 * (and to anyone for whom the wash behind the number is not legible). The peak
 * is described per BUCKET rather than per second, matching how the tile reads. */
const chartLabel = computed<string>(() =>
  t("activity.summary.filesSparklineLabel", {
    minutes: Math.round((props.series.length * props.bucketMs) / 60000),
    peak: numberFormatter.value.format(Math.max(0, ...props.series)),
    seconds: numberFormatter.value.format(Math.round(props.bucketMs / 1000)),
  })
);
</script>

<template>
  <SparklineStatTile
    :label="t('activity.summary.filesUploaded')"
    :series="series"
    :bucket-ms="bucketMs"
    :headline="headline"
    :empty-label="t('activity.summary.noThroughput')"
    :format-bucket="formatFiles"
    :chart-label="chartLabel"
    testid-prefix="files-uploaded"
  />
</template>
