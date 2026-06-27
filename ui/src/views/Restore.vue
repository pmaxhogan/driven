<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";

import { useVirtualList } from "../composables/useVirtualList";
import { useRestoreStore } from "../stores/restore";

// Restore browser (SPEC s11.5; DESIGN s8.4). Browse the backed-up tree (lazy
// per-folder, read from file_state - never Drive), search by filename / glob,
// multi-select files, pick a destination via the backend folder dialog, kick off
// a background restore, and show live progress from `restore:progress` (per-file
// + overall, with errors + a terminal done state). For an encrypted source the
// names shown here are already the decrypted plaintext (file_state stores the
// plaintext path). Every user-facing string flows through t() (DESIGN s8.7).
const props = defineProps<{ sourceId?: string }>();

const { t, locale } = useI18n();
const restore = useRestoreStore();

// Design-system class strings (DRIVEN UI). Defined once so every control in this
// view stays visually consistent with the rest of the app: teal accent for
// primary/interactive affordances, red for destructive, dark-mode readable
// surfaces, teal focus rings. The exact strings are shared verbatim across slices.
const CARD =
  "rounded-lg border border-zinc-200 bg-white p-4 shadow-sm dark:border-zinc-800 dark:bg-zinc-900";
const SELECT_INPUT =
  "rounded-md border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-900 transition-colors focus:border-teal-500 focus:outline-none focus:ring-2 focus:ring-teal-500/40 disabled:opacity-60 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-100";
const PRIMARY_BTN =
  "inline-flex items-center justify-center gap-2 rounded-md bg-teal-700 px-4 py-2 text-sm font-medium text-white shadow-sm transition-colors hover:bg-teal-600 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50";
const SECONDARY_BTN =
  "inline-flex items-center justify-center gap-2 rounded-md border border-zinc-300 bg-white px-4 py-2 text-sm font-medium text-zinc-700 transition-colors hover:bg-zinc-100 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 disabled:cursor-not-allowed disabled:opacity-50 dark:border-zinc-700 dark:bg-zinc-900 dark:text-zinc-200 dark:hover:bg-zinc-800";
const DESTRUCTIVE_BTN =
  "inline-flex items-center justify-center gap-2 rounded-md bg-red-600 px-4 py-2 text-sm font-medium text-white shadow-sm transition-colors hover:bg-red-700 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-red-500 disabled:cursor-not-allowed disabled:opacity-50";
const LINK_BTN =
  "rounded text-sm font-medium text-teal-700 transition-colors hover:text-teal-600 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:text-teal-300 dark:hover:text-teal-200";

// Empty-dropdown guard: the source <select> renders a disabled, self-explaining
// placeholder (and is itself disabled) when there are no sources to browse,
// instead of a blank/confusing control.
const hasSources = computed(() => restore.sources.length > 0);

// The search box model (applied to the store on input).
const searchInput = ref("");

// Locale-aware byte formatting (DESIGN s8.7: never a hand-rolled formatter).
const BYTE_UNITS = ["B", "KB", "MB", "GB", "TB", "PB"] as const;
function formatBytes(bytes: number): string {
  const fmt0 = new Intl.NumberFormat(locale.value);
  if (bytes <= 0) return `${fmt0.format(0)} ${BYTE_UNITS[0]}`;
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), BYTE_UNITS.length - 1);
  const value = bytes / Math.pow(1024, exponent);
  const fmt = new Intl.NumberFormat(locale.value, {
    maximumFractionDigits: exponent === 0 ? 0 : 1,
  });
  return `${fmt.format(value)} ${BYTE_UNITS[exponent]}`;
}

// Overall restore percent (0-100) for the progress bar.
const overallPercent = computed(() => {
  const j = restore.job;
  if (!j || j.totalBytes === 0) return j && j.done ? 100 : 0;
  return Math.min(100, Math.round((j.bytesDone / j.totalBytes) * 100));
});

// List virtualization (windowing). A backed-up folder can hold thousands of
// rows; rendering one <li> each makes the page crawl. Each row is laid out at a
// FIXED height so the windowing math (top/bottom spacer paddings on the <ul>)
// lines up exactly, and only the visible window (+ overscan) is mounted. The
// same ROW_HEIGHT drives both the composable and the per-row style binding.
const ROW_HEIGHT = 40;
const { containerRef: listRef, range: virtualRange } = useVirtualList(
  () => restore.rows.length,
  ROW_HEIGHT
);
// The mounted slice of `restore.rows`. Slicing preserves each row's identity and
// keyOf keying (the key is derived from the row, not its position).
const visibleRows = computed(() =>
  restore.rows.slice(virtualRange.value.startIndex, virtualRange.value.endIndex)
);

onMounted(async () => {
  await restore.subscribeProgress();
  await restore.loadSources();
  // A scoped route (/restore/:sourceId) browses that source directly.
  if (props.sourceId) {
    await restore.selectSource(props.sourceId);
  }
});

onUnmounted(() => {
  restore.unsubscribeProgress();
});

// Keep the scoped route in sync if it changes while mounted.
watch(
  () => props.sourceId,
  (id) => {
    if (id) void restore.selectSource(id);
  }
);

async function onSourceChange(event: Event): Promise<void> {
  const id = (event.target as HTMLSelectElement).value;
  if (id) await restore.selectSource(id);
}

async function onSearchSubmit(): Promise<void> {
  await restore.runSearch(searchInput.value);
}

async function onClearSearch(): Promise<void> {
  searchInput.value = "";
  await restore.runSearch("");
}
</script>

<template>
  <!-- pb-4 keeps a small buffer below the sticky action bar so the very last
       rows clear it when scrolled to the bottom. -->
  <section class="space-y-4 pb-4">
    <header class="space-y-1">
      <h1 class="text-2xl font-semibold">
        {{ t("restore.title") }}
      </h1>
      <p class="text-zinc-600 dark:text-zinc-400">
        {{ t("restore.subtitle") }}
      </p>
    </header>

    <!-- Source selector + search (a single intentional toolbar panel) -->
    <div :class="CARD">
      <div class="flex flex-wrap items-end gap-3">
        <label class="flex flex-col gap-1 text-sm">
          <span class="text-zinc-600 dark:text-zinc-400">{{ t("restore.source") }}</span>
          <select
            class="block w-full sm:w-56"
            :class="SELECT_INPUT"
            :value="restore.sourceId ?? ''"
            :disabled="!hasSources"
            :aria-label="t('restore.source')"
            data-testid="restore-source"
            @change="onSourceChange"
          >
            <template v-if="hasSources">
              <option v-for="s in restore.sources" :key="s.id" :value="s.id">
                {{ s.displayName }}
              </option>
            </template>
            <option v-else value="" disabled>
              {{ t("restore.noSourcesYet") }}
            </option>
          </select>
        </label>

        <form class="flex flex-1 items-end gap-2" @submit.prevent="onSearchSubmit">
          <label class="flex flex-1 flex-col gap-1 text-sm">
            <span class="text-zinc-600 dark:text-zinc-400">{{ t("restore.search.label") }}</span>
            <input
              v-model="searchInput"
              type="search"
              :placeholder="t('restore.search.placeholder')"
              class="w-full"
              :class="SELECT_INPUT"
              :disabled="!hasSources"
              data-testid="restore-search-input"
            />
          </label>
          <button type="submit" :class="PRIMARY_BTN" :disabled="!hasSources">
            {{ t("restore.search.submit") }}
          </button>
          <button
            v-if="restore.isSearching"
            type="button"
            :class="SECONDARY_BTN"
            @click="onClearSearch"
          >
            {{ t("restore.search.clear") }}
          </button>
        </form>
      </div>
    </div>

    <!-- Breadcrumb (browsing only): the current folder is shown as plain text;
         ancestor segments are teal links back up the tree. -->
    <nav
      v-if="!restore.isSearching"
      class="flex flex-wrap items-center gap-1.5 text-sm"
      :aria-label="t('restore.breadcrumb.label')"
    >
      <button
        v-if="restore.breadcrumbs.length > 0"
        type="button"
        :class="LINK_BTN"
        @click="restore.goToBreadcrumb(-1)"
      >
        {{ t("restore.breadcrumb.root") }}
      </button>
      <span v-else class="font-medium text-zinc-900 dark:text-zinc-100" aria-current="page">{{
        t("restore.breadcrumb.root")
      }}</span>
      <template v-for="(seg, i) in restore.breadcrumbs" :key="i">
        <span aria-hidden="true" class="text-zinc-400 dark:text-zinc-600">/</span>
        <button
          v-if="i < restore.breadcrumbs.length - 1"
          type="button"
          :class="LINK_BTN"
          @click="restore.goToBreadcrumb(i)"
        >
          {{ seg }}
        </button>
        <span v-else class="font-medium text-zinc-900 dark:text-zinc-100" aria-current="page">{{
          seg
        }}</span>
      </template>
    </nav>

    <!-- Error -->
    <p
      v-if="restore.errorCode"
      class="rounded-lg border border-red-300 bg-red-50 px-3 py-2 text-sm text-red-700 dark:border-red-800 dark:bg-red-950 dark:text-red-300"
    >
      {{ t(`errors.${restore.errorCode}.long`) }}
    </p>

    <!-- Loading -->
    <p v-if="restore.loading" class="text-sm text-zinc-500 dark:text-zinc-400">
      {{ t("restore.loading") }}
    </p>

    <!-- Empty state -->
    <p
      v-else-if="restore.isEmpty"
      class="rounded-lg border border-dashed border-zinc-300 p-8 text-center text-sm text-zinc-500 dark:border-zinc-700"
      data-testid="restore-empty"
    >
      {{
        !hasSources
          ? t("restore.empty.noSources")
          : restore.isSearching
            ? t("restore.empty.search")
            : t("restore.empty.tree")
      }}
    </p>

    <!-- Truncation notice (M8-P2-1): the folder listing was capped. Shown above
         the list (not part of the loading/empty v-if chain). -->
    <p
      v-if="!restore.loading && !restore.isSearching && restore.treeTruncated"
      class="rounded-lg border border-amber-300 bg-amber-50 px-3 py-2 text-sm text-amber-800 dark:border-amber-800 dark:bg-amber-950 dark:text-amber-300"
    >
      {{ t("restore.truncated", { count: restore.nodes.length }) }}
    </p>

    <!-- File / folder list (virtualized). Only the rows in the visible window
         (+ overscan) are mounted; the top/bottom paddings stand in for the rest
         so the scrollbar still reflects the full list. Each <li> renders at a
         FIXED ROW_HEIGHT so the spacer math lines up exactly. -->
    <ul
      v-if="!restore.loading && !restore.isEmpty"
      ref="listRef"
      data-testid="restore-list"
      class="divide-y divide-zinc-200 overflow-hidden rounded-lg border border-zinc-200 bg-white dark:divide-zinc-800 dark:border-zinc-800 dark:bg-zinc-900"
      :style="{
        paddingTop: virtualRange.paddingTop + 'px',
        paddingBottom: virtualRange.paddingBottom + 'px',
      }"
    >
      <li
        v-for="row in visibleRows"
        :key="restore.keyOf(restore.sourceId ?? '', row.relativePath)"
        class="flex items-center gap-3 px-3 text-sm"
        :style="{ height: ROW_HEIGHT + 'px' }"
      >
        <!-- A folder node (tree only): click to descend. -->
        <template v-if="!restore.isSearching && 'isDir' in row && row.isDir">
          <span class="w-5 text-center text-zinc-400 dark:text-zinc-500" aria-hidden="true"
            >[+]</span
          >
          <button
            type="button"
            class="flex-1 rounded text-left font-medium text-zinc-900 transition-colors hover:text-teal-700 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:text-zinc-100 dark:hover:text-teal-300"
            @click="restore.openFolder(row.relativePath)"
          >
            {{ row.name }}
          </button>
          <span class="text-zinc-400 dark:text-zinc-500">{{ t("restore.node.folder") }}</span>
        </template>

        <!-- A file node: a checkbox to select it for restore. -->
        <template v-else>
          <input
            type="checkbox"
            class="h-4 w-4 accent-teal-600"
            :checked="
              restore.isSelected(
                'sourceId' in row ? row.sourceId : (restore.sourceId ?? ''),
                row.relativePath
              )
            "
            :disabled="!row.restorable"
            :aria-label="t('restore.node.select')"
            @change="
              restore.toggleSelect(
                'sourceId' in row ? row.sourceId : (restore.sourceId ?? ''),
                row.relativePath
              )
            "
          />
          <span class="flex-1 truncate" :title="row.relativePath">
            {{ "name" in row ? row.name : row.relativePath }}
          </span>
          <span v-if="'size' in row" class="text-zinc-400 dark:text-zinc-500">{{
            formatBytes(row.size)
          }}</span>
          <span v-if="!row.restorable" class="text-amber-600 dark:text-amber-400">{{
            t("restore.node.notUploaded")
          }}</span>
        </template>
      </li>
    </ul>

    <!-- Action bar: selection + destination + restore. Sticky to the bottom of
         the scroll viewport so the Restore / Cancel actions stay reachable
         without scrolling a huge folder all the way down. The page-matching
         background + top border + z-index keep it readable over the list (in
         both light and dark mode) as rows scroll beneath it. -->
    <div
      data-testid="restore-action-bar"
      class="sticky bottom-0 z-10 flex flex-wrap items-center gap-3 border-t border-zinc-200 bg-zinc-50 py-3 dark:border-zinc-800 dark:bg-zinc-950"
    >
      <span class="text-sm text-zinc-600 dark:text-zinc-400">
        {{ t("restore.selectedCount", { count: restore.selectedCount }) }}
      </span>
      <button
        v-if="restore.selectedCount > 0"
        type="button"
        :class="LINK_BTN"
        @click="restore.clearSelection"
      >
        {{ t("restore.clearSelection") }}
      </button>

      <span class="flex-1" />

      <button type="button" :class="SECONDARY_BTN" @click="restore.pickDestination">
        {{ t("restore.pickDestination") }}
      </button>
      <span
        v-if="restore.destPath"
        class="max-w-xs truncate text-sm text-zinc-500 dark:text-zinc-400"
        :title="restore.destPath"
      >
        {{ restore.destPath }}
      </span>

      <button
        type="button"
        :class="PRIMARY_BTN"
        :disabled="!restore.canRestore"
        @click="restore.startRestore"
      >
        {{ t("restore.start") }}
      </button>
      <!-- Cancel (M8-P1-1): shown while a job is running; disabled once a cancel
           is requested until the terminal CANCELLED status arrives. -->
      <button
        v-if="restore.restoring && restore.job && !restore.job.done"
        type="button"
        :class="DESTRUCTIVE_BTN"
        :disabled="restore.cancelling"
        @click="restore.cancelRestore"
      >
        {{ restore.cancelling ? t("restore.cancelling") : t("restore.cancel") }}
      </button>
    </div>

    <!-- Live restore progress -->
    <div v-if="restore.job" :class="CARD" class="space-y-2">
      <div class="flex items-center justify-between text-sm">
        <span class="font-medium">{{
          restore.job.cancelled
            ? t("restore.progress.cancelled")
            : restore.job.done
              ? t("restore.progress.done")
              : t("restore.progress.running")
        }}</span>
        <span class="text-zinc-500 dark:text-zinc-400">{{
          t("restore.progress.summary", {
            completed: restore.job.completedFiles,
            total: restore.job.totalFiles,
            failed: restore.job.failedFiles,
          })
        }}</span>
      </div>
      <!-- Overall bar -->
      <div
        class="h-2 w-full overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700"
        role="progressbar"
        :aria-valuenow="overallPercent"
        aria-valuemin="0"
        aria-valuemax="100"
      >
        <div class="h-full bg-teal-600 transition-all" :style="{ width: overallPercent + '%' }" />
      </div>
      <p v-if="restore.job.currentFile" class="truncate text-xs text-zinc-500 dark:text-zinc-400">
        {{ t("restore.progress.current", { file: restore.job.currentFile }) }}
      </p>
      <!-- Per-file breakdown -->
      <ul class="max-h-48 space-y-1 overflow-auto text-xs">
        <li v-for="f in restore.job.files" :key="f.relativePath" class="flex items-center gap-2">
          <span class="flex-1 truncate" :title="f.relativePath">{{ f.relativePath }}</span>
          <span
            v-if="f.state === 'failed'"
            class="text-red-600 dark:text-red-400"
            :title="f.errorCode ? t(`errors.${f.errorCode}.long`) : undefined"
            >{{ t("restore.file.failed") }}</span
          >
          <span v-else-if="f.state === 'done'" class="text-emerald-600 dark:text-emerald-400">{{
            t("restore.file.done")
          }}</span>
          <span v-else-if="f.state === 'restoring'" class="text-zinc-500 dark:text-zinc-400">{{
            t("restore.file.restoring")
          }}</span>
          <span v-else-if="f.state === 'cancelled'" class="text-zinc-500 dark:text-zinc-400">{{
            t("restore.file.cancelled")
          }}</span>
          <span v-else class="text-zinc-400 dark:text-zinc-500">{{
            t("restore.file.pending")
          }}</span>
        </li>
      </ul>
    </div>
  </section>
</template>
