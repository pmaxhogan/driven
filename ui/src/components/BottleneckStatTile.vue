<script setup lang="ts">
import { computed } from "vue";
import { useI18n } from "vue-i18n";

import { formatBytes } from "../stores/formatBytes";
import type { BottleneckSnapshot } from "../ipc/types";

// The "Bottleneck" stat tile (issue #308): which pipeline stage is presently
// limiting throughput, as a sibling of the other Activity dashboard stat
// tiles - same STAT_TILE chrome and dt/dd typography as ThroughputStatTile /
// the "Files by status" tile. Unlike the throughput tiles this one carries no
// time series (the backend classifies one CURRENT state, not a trend), so it
// renders as a plain value + sub-line rather than a sparkline.

const props = defineProps<{
  /** The debounced snapshot from `stores/bottleneck.ts`, or null before the
   * first classification has arrived. */
  snapshot: BottleneckSnapshot | null;
}>();

const { t, locale } = useI18n();

const STAT_TILE =
  "rounded-lg border border-zinc-200 bg-white p-3 shadow-xs dark:border-zinc-800 dark:bg-zinc-900";

/** Format a bytes/sec rate the way the throughput tiles word it ("1.5 KB/s"),
 * so a number here reads identically to the same rate elsewhere on the page. */
function formatRate(bytesPerSecond: number): string {
  return t("activity.summary.perSecond", {
    rate: formatBytes(bytesPerSecond, locale.value),
  });
}

/** The tile's headline value: the state's display name. */
const value = computed<string>(() => {
  switch (props.snapshot?.state) {
    case "disk":
      return t("activity.summary.bottleneckDisk");
    case "network":
      return t("activity.summary.bottleneckNetwork");
    case "api":
      return t("activity.summary.bottleneckApi");
    case "cpu":
      return t("activity.summary.bottleneckCpu");
    case "mixed":
      return t("activity.summary.bottleneckMixed");
    case "not_backing_up":
      return t("activity.summary.bottleneckNotBackingUp");
    default:
      // No snapshot has arrived yet (store not yet hydrated).
      return t("activity.summary.bottleneckUnknown");
  }
});

/** The tile's sub-line: the driving rate or backoff detail, when the state
 * carries one. Null renders no sub-line at all (NotBackingUp and the
 * not-yet-hydrated state). */
const subline = computed<string | null>(() => {
  const s = props.snapshot;
  if (!s) return null;
  switch (s.state) {
    case "disk":
      return s.rateBytesPerSec === null
        ? null
        : t("activity.summary.bottleneckDiskSub", { rate: formatRate(s.rateBytesPerSec) });
    case "network":
      return s.rateBytesPerSec === null
        ? null
        : t("activity.summary.bottleneckNetworkSub", { rate: formatRate(s.rateBytesPerSec) });
    case "cpu":
      return s.rateBytesPerSec === null
        ? null
        : t("activity.summary.bottleneckCpuSub", { rate: formatRate(s.rateBytesPerSec) });
    case "api": {
      const backend = s.backend ?? t("activity.summary.bottleneckApiGenericBackend");
      const seconds =
        s.backoffRemainingMs === null ? 0 : Math.max(0, Math.round(s.backoffRemainingMs / 1000));
      return t("activity.summary.bottleneckApiSub", { backend, seconds });
    }
    case "mixed":
      return t("activity.summary.bottleneckMixedSub");
    case "not_backing_up":
    default:
      return null;
  }
});
</script>

<template>
  <div :class="STAT_TILE" data-testid="bottleneck-tile">
    <dt class="text-xs text-zinc-500 dark:text-zinc-400">
      {{ t("activity.summary.bottleneck") }}
    </dt>
    <dd class="mt-1 text-lg font-semibold" data-testid="bottleneck-value">
      {{ value }}
    </dd>
    <p
      v-if="subline"
      class="mt-0.5 text-xs font-normal text-zinc-500 dark:text-zinc-400"
      data-testid="bottleneck-sub"
    >
      {{ subline }}
    </p>
  </div>
</template>
