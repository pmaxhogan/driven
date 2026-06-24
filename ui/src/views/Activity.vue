<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
import { activityEventLabel } from "../stores/activityEventLabel";
import { useActivityStore } from "../stores/activity";
import { useSourcesStore } from "../stores/sources";
import type {
  ActivityEntry,
  ActivityLevel,
  FileStateStatus,
} from "../ipc/types";

// Activity dashboard (SPEC s11.4; DESIGN s8.3). A live tail (subscribes to
// `activity:new` and prepends new rows, deduped by id) over a paginated history
// (query_activity, pages accumulated client-side so scrolling back through
// 1000+ events never re-queries earlier pages), with filter controls (source,
// minimum level, event type) that re-query, and an empty state.
const { t, te, locale } = useI18n();
const activity = useActivityStore();
const sources = useSourcesStore();

// Filter form state (bound to the controls; applied via the store on change).
const filterSourceId = ref<string>("");
const filterLevel = ref<string>("");
const filterEventType = ref<string>("");

// Diagnostic-bundle export state (ROADMAP M7 / DESIGN s8.3 "Export diagnostic
// bundle" button; backed by the M6 export_diagnostic_bundle command). M7-P2-6:
// the error is held as a stable SPEC s24 code and localized via t(), never the
// raw `String(e)` (which can be `[object Object]` / backend English).
const exporting = ref(false);
const exportErrorCode = ref<string | null>(null);
const exportedPath = ref<string | null>(null);

// Locale-aware formatters (DESIGN s8.7: never hand-rolled English formatters).
const dateTimeFormatter = computed(
  () =>
    new Intl.DateTimeFormat(locale.value, {
      dateStyle: "medium",
      timeStyle: "medium",
    }),
);
const numberFormatter = computed(() => new Intl.NumberFormat(locale.value));

// M7-P2-5 (DESIGN s8.3 header aggregates). Locale-aware byte + rate formatting
// via Intl.NumberFormat (DESIGN s8.7: never a hand-rolled English formatter).
const BYTE_UNITS = ["B", "KB", "MB", "GB", "TB", "PB"] as const;

function formatBytes(bytes: number): string {
  if (bytes <= 0) return `${numberFormatter.value.format(0)} ${BYTE_UNITS[0]}`;
  const exponent = Math.min(
    Math.floor(Math.log(bytes) / Math.log(1024)),
    BYTE_UNITS.length - 1,
  );
  const value = bytes / Math.pow(1024, exponent);
  const fmt = new Intl.NumberFormat(locale.value, {
    maximumFractionDigits: exponent === 0 ? 0 : 1,
  });
  return `${fmt.format(value)} ${BYTE_UNITS[exponent]}`;
}

// Header aggregate view model (M7-P2-5). Null until the summary loads.
const bytesToday = computed(() =>
  activity.summary ? formatBytes(activity.summary.bytesToday) : null,
);
const bytesWeek = computed(() =>
  activity.summary ? formatBytes(activity.summary.bytesWeek) : null,
);
const throughput = computed(() => {
  const s = activity.summary;
  if (!s || s.throughputWindowMs <= 0) return null;
  const perSec = s.throughputWindowBytes / (s.throughputWindowMs / 1000);
  return formatBytes(perSec);
});
const statusCounts = computed(() => activity.summary?.fileStatusCounts ?? []);

function statusLabel(status: FileStateStatus): string {
  return t(`activity.status.${status}`);
}

// M7-P2-4: the event-type filter options come from the backend's DISTINCT query
// (the store's `eventTypeOptions`), so the user can filter for a type present in
// history but not in the currently-loaded rows - the loaded-rows-only derivation
// made the backend event-type filter unreachable for older types.
const eventTypeOptions = computed<string[]>(() => activity.eventTypeOptions);

const sourceNameById = computed<Record<string, string>>(() => {
  const map: Record<string, string> = {};
  for (const s of sources.sources) map[s.id] = s.displayName;
  return map;
});

const shownCount = computed(() => activity.entries.length);

function formatTime(entry: ActivityEntry): string {
  return dateTimeFormatter.value.format(new Date(entry.ts));
}

function levelLabel(level: ActivityLevel): string {
  return t(`activity.level.${level}`);
}

/** R1-P2-3: render a localized label for an activity event type instead of the
 * raw backend code. Delegates to the shared `activityEventLabel` helper (the
 * `te()` existence check uses the active locale with the i18n fallbackLocale).
 */
function eventLabel(eventType: string): string {
  return activityEventLabel(eventType, t, te);
}

function levelClass(level: ActivityLevel): string {
  switch (level) {
    case "error":
      return "text-red-600 dark:text-red-400";
    case "warn":
      return "text-amber-600 dark:text-amber-400";
    default:
      return "text-zinc-500 dark:text-zinc-400";
  }
}

function sourceLabel(entry: ActivityEntry): string {
  if (entry.sourceId == null) return t("activity.noSource");
  return sourceNameById.value[entry.sourceId] ?? entry.sourceId;
}

// Build the filter DTO from the form and apply it (re-query from page 0).
async function applyFilters(): Promise<void> {
  const eventTypes =
    filterEventType.value.length > 0 ? [filterEventType.value] : [];
  await activity.applyFilter({
    sourceId: filterSourceId.value.length > 0 ? filterSourceId.value : null,
    minLevel:
      filterLevel.value.length > 0
        ? (filterLevel.value as ActivityLevel)
        : null,
    eventTypes,
  });
}

async function clearFilters(): Promise<void> {
  filterSourceId.value = "";
  filterLevel.value = "";
  filterEventType.value = "";
  await activity.applyFilter({});
}

// Export the diagnostic bundle (DESIGN s8.3). C1/C2: the BACKEND owns the
// save-file dialog and returns a concrete `.zip` path + one-shot token; the
// webview never supplies a write target (SPEC s11.6.1).
async function exportBundle(): Promise<void> {
  exportErrorCode.value = null;
  exportedPath.value = null;
  let token: string;
  try {
    const picked = await ipc.pickSaveZipDialog();
    token = picked.token;
  } catch {
    // Cancel (or dialog error): nothing to export.
    return;
  }
  exporting.value = true;
  try {
    exportedPath.value = await ipc.exportDiagnosticBundle(token);
  } catch (e) {
    // M7-P2-6: normalize to the stable SPEC s24 code and localize via t().
    exportErrorCode.value = toErrorCode(e);
  } finally {
    exporting.value = false;
  }
}

onMounted(async () => {
  // Subscribe to the live tail BEFORE the first history load so no event that
  // fires during the load is missed (it dedups against the paged rows by id).
  // The subscription also reconciles from the durable log on `activity:lagged`
  // (M7-P1-1), so a broadcast-lag burst loses no rows.
  await activity.subscribeLive();
  await Promise.all([
    sources.refresh(),
    activity.loadInitial(),
    activity.loadEventTypeOptions(),
    activity.loadSummary(),
  ]);
});

onUnmounted(() => {
  activity.unsubscribeLive();
});
</script>

<template>
  <section class="space-y-4">
    <header class="space-y-1">
      <div class="flex items-start justify-between gap-3">
        <div class="space-y-1">
          <h1 class="text-2xl font-semibold">
            {{ t("activity.title") }}
          </h1>
          <p class="text-zinc-600 dark:text-zinc-400">
            {{ t("activity.subtitle") }}
          </p>
        </div>
        <button
          type="button"
          class="shrink-0 rounded border px-3 py-1.5 text-sm"
          :disabled="exporting"
          data-testid="activity-export-bundle"
          @click="exportBundle"
        >
          {{
            exporting
              ? t("activity.exporting")
              : t("activity.exportBundleButton")
          }}
        </button>
      </div>
      <p
        v-if="exportErrorCode"
        class="text-sm text-red-600"
        data-testid="activity-export-error"
      >
        {{ t(`errors.${exportErrorCode}.long`) }}
      </p>
      <p
        v-else-if="exportedPath"
        class="text-sm text-green-700 dark:text-green-400"
      >
        {{ t("activity.exportedTo", { path: exportedPath }) }}
      </p>

      <!-- M7-P2-5 (DESIGN s8.3): header aggregate stats. -->
      <dl
        v-if="activity.summary"
        class="grid grid-cols-2 gap-3 pt-2 sm:grid-cols-4"
        data-testid="activity-summary"
      >
        <div class="rounded border p-3">
          <dt class="text-xs text-zinc-500">
            {{ t("activity.summary.bytesToday") }}
          </dt>
          <dd class="text-lg font-semibold">
            {{ bytesToday }}
          </dd>
        </div>
        <div class="rounded border p-3">
          <dt class="text-xs text-zinc-500">
            {{ t("activity.summary.bytesWeek") }}
          </dt>
          <dd class="text-lg font-semibold">
            {{ bytesWeek }}
          </dd>
        </div>
        <div class="rounded border p-3">
          <dt class="text-xs text-zinc-500">
            {{ t("activity.summary.throughput") }}
          </dt>
          <dd class="text-lg font-semibold">
            {{ t("activity.summary.perSecond", { rate: throughput }) }}
          </dd>
        </div>
        <div class="rounded border p-3">
          <dt class="text-xs text-zinc-500">
            {{ t("activity.summary.byStatus") }}
          </dt>
          <dd class="text-sm">
            <span
              v-if="statusCounts.length === 0"
              class="text-zinc-400"
            >
              {{ t("activity.summary.noFiles") }}
            </span>
            <span
              v-for="sc in statusCounts"
              :key="sc.status"
              class="mr-2 inline-block whitespace-nowrap"
            >
              {{ statusLabel(sc.status) }}:
              {{ numberFormatter.format(sc.count) }}
            </span>
          </dd>
        </div>
      </dl>
    </header>

    <div
      class="flex flex-wrap items-end gap-3 rounded border p-3"
      data-testid="activity-filters"
    >
      <label class="block space-y-1 text-sm">
        <span class="text-zinc-600 dark:text-zinc-400">{{
          t("activity.filters.source")
        }}</span>
        <select
          v-model="filterSourceId"
          class="block rounded border px-2 py-1 text-sm"
          @change="applyFilters"
        >
          <option value="">
            {{ t("activity.filters.allSources") }}
          </option>
          <option
            v-for="s in sources.sources"
            :key="s.id"
            :value="s.id"
          >
            {{ s.displayName }}
          </option>
        </select>
      </label>

      <label class="block space-y-1 text-sm">
        <span class="text-zinc-600 dark:text-zinc-400">{{
          t("activity.filters.level")
        }}</span>
        <select
          v-model="filterLevel"
          class="block rounded border px-2 py-1 text-sm"
          @change="applyFilters"
        >
          <option value="">
            {{ t("activity.filters.allLevels") }}
          </option>
          <option value="info">
            {{ t("activity.level.info") }}
          </option>
          <option value="warn">
            {{ t("activity.level.warn") }}
          </option>
          <option value="error">
            {{ t("activity.level.error") }}
          </option>
        </select>
      </label>

      <label class="block space-y-1 text-sm">
        <span class="text-zinc-600 dark:text-zinc-400">{{
          t("activity.filters.eventType")
        }}</span>
        <select
          v-model="filterEventType"
          class="block rounded border px-2 py-1 text-sm"
          @change="applyFilters"
        >
          <option value="">
            {{ t("activity.filters.allEventTypes") }}
          </option>
          <option
            v-for="et in eventTypeOptions"
            :key="et"
            :value="et"
            :title="et"
          >
            {{ eventLabel(et) }}
          </option>
        </select>
      </label>

      <button
        type="button"
        class="rounded border px-3 py-1.5 text-sm"
        @click="clearFilters"
      >
        {{ t("activity.filters.clear") }}
      </button>
    </div>

    <p
      v-if="activity.errorCode"
      class="text-sm text-red-600"
      data-testid="activity-error"
    >
      {{ t(`errors.${activity.errorCode}.long`) }}
    </p>

    <p
      v-if="!activity.errorCode"
      class="text-sm text-zinc-500"
      data-testid="activity-count"
    >
      {{
        t("activity.countSummary", {
          shown: numberFormatter.format(shownCount),
          total: numberFormatter.format(activity.total),
        })
      }}
    </p>

    <p
      v-if="activity.isEmpty"
      class="rounded border border-dashed p-6 text-center text-sm text-zinc-500"
      data-testid="activity-empty"
    >
      {{ t("activity.empty") }}
    </p>

    <table
      v-else-if="activity.entries.length > 0"
      class="w-full text-left text-sm"
      data-testid="activity-table"
    >
      <thead class="text-xs text-zinc-500">
        <tr>
          <th class="py-1">
            {{ t("activity.column.time") }}
          </th>
          <th class="py-1">
            {{ t("activity.column.level") }}
          </th>
          <th class="py-1">
            {{ t("activity.column.event") }}
          </th>
          <th class="py-1">
            {{ t("activity.column.source") }}
          </th>
          <th class="py-1">
            {{ t("activity.column.details") }}
          </th>
        </tr>
      </thead>
      <tbody class="divide-y">
        <tr
          v-for="entry in activity.entries"
          :key="entry.id"
          data-testid="activity-row"
        >
          <td class="whitespace-nowrap py-2">
            {{ formatTime(entry) }}
          </td>
          <td
            class="py-2 font-medium"
            :class="levelClass(entry.level)"
          >
            {{ levelLabel(entry.level) }}
          </td>
          <td
            class="break-all py-2"
            :title="entry.eventType"
          >
            {{ eventLabel(entry.eventType) }}
          </td>
          <td class="break-all py-2">
            {{ sourceLabel(entry) }}
          </td>
          <td class="break-all py-2">
            <span v-if="entry.message">{{ entry.message }}</span>
            <span
              v-else-if="entry.fileCount != null"
              class="text-zinc-500"
            >
              {{
                t("activity.files", {
                  count: numberFormatter.format(entry.fileCount),
                })
              }}
            </span>
          </td>
        </tr>
      </tbody>
    </table>

    <div
      v-if="activity.hasMore"
      class="flex justify-center"
    >
      <button
        type="button"
        class="rounded border px-3 py-1.5 text-sm"
        :disabled="activity.loading"
        data-testid="activity-load-more"
        @click="activity.loadMore()"
      >
        {{
          activity.loading
            ? t("activity.loadingMore")
            : t("activity.loadMore")
        }}
      </button>
    </div>
    <p
      v-else-if="activity.entries.length > 0"
      class="text-center text-xs text-zinc-400"
    >
      {{ t("activity.allLoaded") }}
    </p>
  </section>
</template>
