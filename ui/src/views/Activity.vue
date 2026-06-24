<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import * as ipc from "../ipc/commands";
import { useActivityStore } from "../stores/activity";
import { useSourcesStore } from "../stores/sources";
import type { ActivityEntry, ActivityLevel } from "../ipc/types";

// Activity dashboard (SPEC s11.4; DESIGN s8.3). A live tail (subscribes to
// `activity:new` and prepends new rows, deduped by id) over a paginated history
// (query_activity, pages accumulated client-side so scrolling back through
// 1000+ events never re-queries earlier pages), with filter controls (source,
// minimum level, event type) that re-query, and an empty state.
const { t, locale } = useI18n();
const activity = useActivityStore();
const sources = useSourcesStore();

// Filter form state (bound to the controls; applied via the store on change).
const filterSourceId = ref<string>("");
const filterLevel = ref<string>("");
const filterEventType = ref<string>("");

// Diagnostic-bundle export state (ROADMAP M7 / DESIGN s8.3 "Export diagnostic
// bundle" button; backed by the M6 export_diagnostic_bundle command).
const exporting = ref(false);
const exportError = ref<string | null>(null);
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

// The set of event types present in the loaded rows, for the event-type filter
// options (the backend has no enumerate endpoint; the loaded rows are the source
// of truth for what the user has actually seen).
const eventTypeOptions = computed<string[]>(() => {
  const seen = new Set<string>();
  for (const e of activity.entries) seen.add(e.eventType);
  return Array.from(seen).sort();
});

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
  exportError.value = null;
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
    exportError.value = String(e);
  } finally {
    exporting.value = false;
  }
}

onMounted(async () => {
  // Subscribe to the live tail BEFORE the first history load so no event that
  // fires during the load is missed (it dedups against the paged rows by id).
  await activity.subscribeLive();
  await Promise.all([sources.refresh(), activity.loadInitial()]);
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
        v-if="exportError"
        class="text-sm text-red-600"
      >
        {{ exportError }}
      </p>
      <p
        v-else-if="exportedPath"
        class="text-sm text-green-700 dark:text-green-400"
      >
        {{ t("activity.exportedTo", { path: exportedPath }) }}
      </p>
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
          >
            {{ et }}
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
      v-if="activity.error"
      class="text-sm text-red-600"
    >
      {{ activity.error }}
    </p>

    <p
      v-if="!activity.error"
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
          <td class="break-all py-2">
            {{ entry.eventType }}
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
