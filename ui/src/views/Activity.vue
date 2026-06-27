<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
import { activityEventLabel } from "../stores/activityEventLabel";
import { ACTIVITY_PAGE_SIZE, ACTIVITY_RENDER_WINDOW, useActivityStore } from "../stores/activity";
import { useSourcesStore } from "../stores/sources";
import type { ActivityEntry, ActivityLevel, FileStateStatus } from "../ipc/types";

// Activity dashboard (SPEC s11.4; DESIGN s8.3). A live tail (subscribes to
// `activity:new` and prepends new rows, deduped by id) over a paginated history
// (query_activity, pages accumulated client-side so scrolling back through
// 1000+ events never re-queries earlier pages), with filter controls (source,
// minimum level, event type) that re-query, and an empty state.
const { t, te, locale } = useI18n();
const activity = useActivityStore();
const sources = useSourcesStore();

// Design-system class strings (DRIVEN UI). Defined once so every control in this
// view stays visually consistent with the rest of the app: teal accent, dark-mode
// readable surfaces, teal focus rings. The exact strings are shared verbatim
// across slices so all views match.
const CARD =
  "rounded-lg border border-zinc-200 bg-white p-4 shadow-sm dark:border-zinc-800 dark:bg-zinc-900";
const STAT_TILE =
  "rounded-lg border border-zinc-200 bg-white p-3 shadow-sm dark:border-zinc-800 dark:bg-zinc-900";
const SELECT_INPUT =
  "rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 transition-colors focus:border-teal-500 focus:outline-none focus:ring-2 focus:ring-teal-500/40 disabled:opacity-60 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100";
const SECONDARY_BTN =
  "inline-flex items-center justify-center gap-2 rounded-md border border-zinc-300 bg-white px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800";

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
    })
);
const numberFormatter = computed(() => new Intl.NumberFormat(locale.value));

// M7-P2-5 (DESIGN s8.3 header aggregates). Locale-aware byte + rate formatting
// via Intl.NumberFormat (DESIGN s8.7: never a hand-rolled English formatter).
const BYTE_UNITS = ["B", "KB", "MB", "GB", "TB", "PB"] as const;

function formatBytes(bytes: number): string {
  if (bytes <= 0) return `${numberFormatter.value.format(0)} ${BYTE_UNITS[0]}`;
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), BYTE_UNITS.length - 1);
  const value = bytes / Math.pow(1024, exponent);
  const fmt = new Intl.NumberFormat(locale.value, {
    maximumFractionDigits: exponent === 0 ? 0 : 1,
  });
  return `${fmt.format(value)} ${BYTE_UNITS[exponent]}`;
}

// Header aggregate view model (M7-P2-5). Null until the summary loads.
const bytesToday = computed(() =>
  activity.summary ? formatBytes(activity.summary.bytesToday) : null
);
const bytesWeek = computed(() =>
  activity.summary ? formatBytes(activity.summary.bytesWeek) : null
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

// Empty-dropdown guards: a filter <select> with zero real options renders a
// disabled, self-explaining placeholder instead of a blank/confusing control,
// and the <select> itself is disabled. The source list comes from Settings; the
// event-type facets come from the backend's DISTINCT query over the log.
const hasSources = computed(() => sources.sources.length > 0);
const hasEventTypes = computed(() => eventTypeOptions.value.length > 0);

const sourceNameById = computed<Record<string, string>>(() => {
  const map: Record<string, string> = {};
  for (const s of sources.sources) map[s.id] = s.displayName;
  return map;
});

const shownCount = computed(() => activity.entries.length);

// Issue #45: bound the rendered DOM. The store can accumulate up to ~1000 live
// entries plus paged history; mounting every row makes the page janky while an
// upload streams new rows in. Render only the newest `renderLimit` rows and grow
// the window on demand, so the mounted row count never grows with the live tail.
const renderLimit = ref(ACTIVITY_RENDER_WINDOW);
const visibleEntries = computed(() => activity.entries.slice(0, renderLimit.value));

// More accumulated (in-memory) entries exist than are currently rendered.
const canShowMore = computed(() => renderLimit.value < activity.entries.length);
// The "load more" control is available when there are more buffered rows to
// reveal OR more history pages to fetch from the backend.
const canLoadMore = computed(() => canShowMore.value || activity.hasMore);

/** One progressive-disclosure "load more": first reveal any already-loaded rows
 * beyond the render window (instant, no fetch), then page older history from the
 * backend (growing the window to keep the freshly fetched page visible). */
async function loadMoreRows(): Promise<void> {
  if (canShowMore.value) {
    renderLimit.value += ACTIVITY_RENDER_WINDOW;
    return;
  }
  if (activity.hasMore) {
    await activity.loadMore();
    renderLimit.value += ACTIVITY_PAGE_SIZE;
  }
}

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
  const eventTypes = filterEventType.value.length > 0 ? [filterEventType.value] : [];
  // A re-query resets the accumulated rows, so collapse the render window too.
  renderLimit.value = ACTIVITY_RENDER_WINDOW;
  await activity.applyFilter({
    sourceId: filterSourceId.value.length > 0 ? filterSourceId.value : null,
    minLevel: filterLevel.value.length > 0 ? (filterLevel.value as ActivityLevel) : null,
    eventTypes,
  });
}

async function clearFilters(): Promise<void> {
  filterSourceId.value = "";
  filterLevel.value = "";
  filterEventType.value = "";
  renderLimit.value = ACTIVITY_RENDER_WINDOW;
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
    <header class="space-y-3">
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
          class="shrink-0"
          :class="SECONDARY_BTN"
          :disabled="exporting"
          data-testid="activity-export-bundle"
          @click="exportBundle"
        >
          {{ exporting ? t("activity.exporting") : t("activity.exportBundleButton") }}
        </button>
      </div>
      <p
        v-if="exportErrorCode"
        class="text-sm text-red-600 dark:text-red-400"
        data-testid="activity-export-error"
      >
        {{ t(`errors.${exportErrorCode}.long`) }}
      </p>
      <p v-else-if="exportedPath" class="text-sm text-emerald-700 dark:text-emerald-400">
        {{ t("activity.exportedTo", { path: exportedPath }) }}
      </p>

      <!-- M7-P2-5 (DESIGN s8.3): header aggregate stats. -->
      <dl
        v-if="activity.summary"
        class="grid grid-cols-2 gap-3 sm:grid-cols-4"
        data-testid="activity-summary"
      >
        <div :class="STAT_TILE">
          <dt class="text-xs text-zinc-500 dark:text-zinc-400">
            {{ t("activity.summary.bytesToday") }}
          </dt>
          <dd class="mt-1 text-lg font-semibold">
            {{ bytesToday }}
          </dd>
        </div>
        <div :class="STAT_TILE">
          <dt class="text-xs text-zinc-500 dark:text-zinc-400">
            {{ t("activity.summary.bytesWeek") }}
          </dt>
          <dd class="mt-1 text-lg font-semibold">
            {{ bytesWeek }}
          </dd>
        </div>
        <div :class="STAT_TILE">
          <dt class="text-xs text-zinc-500 dark:text-zinc-400">
            {{ t("activity.summary.throughput") }}
          </dt>
          <dd class="mt-1 text-lg font-semibold">
            {{ t("activity.summary.perSecond", { rate: throughput }) }}
          </dd>
        </div>
        <div :class="STAT_TILE">
          <dt class="text-xs text-zinc-500 dark:text-zinc-400">
            {{ t("activity.summary.byStatus") }}
          </dt>
          <dd class="mt-1 text-sm">
            <span v-if="statusCounts.length === 0" class="text-zinc-400 dark:text-zinc-500">
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

    <div :class="CARD" data-testid="activity-filters">
      <h2 class="mb-3 text-sm font-semibold text-zinc-700 dark:text-zinc-300">
        {{ t("activity.filters.title") }}
      </h2>
      <div class="flex flex-wrap items-end gap-3">
        <label class="block space-y-1 text-sm">
          <span class="text-zinc-600 dark:text-zinc-400">{{ t("activity.filters.source") }}</span>
          <select
            v-model="filterSourceId"
            class="block w-full"
            :class="SELECT_INPUT"
            :disabled="!hasSources"
            :aria-label="t('activity.filters.source')"
            data-testid="activity-filter-source"
            @change="applyFilters"
          >
            <template v-if="hasSources">
              <option value="">
                {{ t("activity.filters.allSources") }}
              </option>
              <option v-for="s in sources.sources" :key="s.id" :value="s.id">
                {{ s.displayName }}
              </option>
            </template>
            <option v-else value="" disabled>
              {{ t("activity.filters.noSourcesYet") }}
            </option>
          </select>
        </label>

        <label class="block space-y-1 text-sm">
          <span class="text-zinc-600 dark:text-zinc-400">{{ t("activity.filters.level") }}</span>
          <select
            v-model="filterLevel"
            class="block w-full"
            :class="SELECT_INPUT"
            :aria-label="t('activity.filters.level')"
            data-testid="activity-filter-level"
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
            class="block w-full"
            :class="SELECT_INPUT"
            :disabled="!hasEventTypes"
            :aria-label="t('activity.filters.eventType')"
            data-testid="activity-filter-event-type"
            @change="applyFilters"
          >
            <template v-if="hasEventTypes">
              <option value="">
                {{ t("activity.filters.allEventTypes") }}
              </option>
              <option v-for="et in eventTypeOptions" :key="et" :value="et" :title="et">
                {{ eventLabel(et) }}
              </option>
            </template>
            <option v-else value="" disabled>
              {{ t("activity.filters.noEventsYet") }}
            </option>
          </select>
        </label>

        <button type="button" :class="SECONDARY_BTN" @click="clearFilters">
          {{ t("activity.filters.clear") }}
        </button>
      </div>
    </div>

    <p
      v-if="activity.errorCode"
      class="text-sm text-red-600 dark:text-red-400"
      data-testid="activity-error"
    >
      {{ t(`errors.${activity.errorCode}.long`) }}
    </p>

    <p
      v-if="!activity.errorCode"
      class="text-sm text-zinc-500 dark:text-zinc-400"
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
      class="rounded-lg border border-dashed border-zinc-300 p-8 text-center text-sm text-zinc-500 dark:border-zinc-700"
      data-testid="activity-empty"
    >
      {{ t("activity.empty") }}
    </p>

    <div v-else-if="activity.entries.length > 0" class="overflow-x-auto" :class="CARD">
      <table class="w-full text-left text-sm" data-testid="activity-table">
        <thead class="text-xs text-zinc-500 dark:text-zinc-400">
          <tr>
            <th class="py-1 pr-3 font-medium">
              {{ t("activity.column.time") }}
            </th>
            <th class="py-1 pr-3 font-medium">
              {{ t("activity.column.level") }}
            </th>
            <th class="py-1 pr-3 font-medium">
              {{ t("activity.column.event") }}
            </th>
            <th class="py-1 pr-3 font-medium">
              {{ t("activity.column.source") }}
            </th>
            <th class="py-1 font-medium">
              {{ t("activity.column.details") }}
            </th>
          </tr>
        </thead>
        <tbody class="divide-y divide-zinc-200 dark:divide-zinc-800">
          <tr v-for="entry in visibleEntries" :key="entry.id" data-testid="activity-row">
            <td class="whitespace-nowrap py-2 pr-3 align-top">
              {{ formatTime(entry) }}
            </td>
            <td class="py-2 pr-3 align-top font-medium" :class="levelClass(entry.level)">
              {{ levelLabel(entry.level) }}
            </td>
            <td class="break-all py-2 pr-3 align-top" :title="entry.eventType">
              {{ eventLabel(entry.eventType) }}
            </td>
            <td class="break-all py-2 pr-3 align-top">
              {{ sourceLabel(entry) }}
            </td>
            <td class="break-all py-2 align-top text-zinc-600 dark:text-zinc-300">
              <span v-if="entry.message">{{ entry.message }}</span>
              <span v-else-if="entry.fileCount != null" class="text-zinc-500 dark:text-zinc-400">
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
    </div>

    <div v-if="canLoadMore" class="flex justify-center">
      <button
        type="button"
        :class="SECONDARY_BTN"
        :disabled="activity.loading"
        data-testid="activity-load-more"
        @click="loadMoreRows"
      >
        {{ activity.loading ? t("activity.loadingMore") : t("activity.loadMore") }}
      </button>
    </div>
    <p
      v-else-if="activity.entries.length > 0"
      class="text-center text-xs text-zinc-400 dark:text-zinc-500"
    >
      {{ t("activity.allLoaded") }}
    </p>
  </section>
</template>
