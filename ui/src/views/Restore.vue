<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { useI18n } from "vue-i18n";

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
  <section class="space-y-4">
    <h1 class="text-2xl font-semibold">
      {{ t("restore.title") }}
    </h1>

    <!-- Source selector + search -->
    <div class="flex flex-wrap items-end gap-3">
      <label class="flex flex-col gap-1 text-sm">
        <span class="text-zinc-600 dark:text-zinc-400">{{ t("restore.source") }}</span>
        <select
          class="rounded border border-zinc-300 bg-white px-2 py-1 dark:border-zinc-700 dark:bg-zinc-800"
          :value="restore.sourceId ?? ''"
          @change="onSourceChange"
        >
          <option v-for="s in restore.sources" :key="s.id" :value="s.id">
            {{ s.displayName }}
          </option>
        </select>
      </label>

      <form class="flex items-end gap-2" @submit.prevent="onSearchSubmit">
        <label class="flex flex-col gap-1 text-sm">
          <span class="text-zinc-600 dark:text-zinc-400">{{ t("restore.search.label") }}</span>
          <input
            v-model="searchInput"
            type="search"
            :placeholder="t('restore.search.placeholder')"
            class="w-64 rounded border border-zinc-300 bg-white px-2 py-1 dark:border-zinc-700 dark:bg-zinc-800"
          />
        </label>
        <button
          type="submit"
          class="rounded bg-zinc-200 px-3 py-1 text-sm hover:bg-zinc-300 dark:bg-zinc-700 dark:hover:bg-zinc-600"
        >
          {{ t("restore.search.submit") }}
        </button>
        <button
          v-if="restore.isSearching"
          type="button"
          class="rounded px-3 py-1 text-sm text-zinc-600 hover:underline dark:text-zinc-400"
          @click="onClearSearch"
        >
          {{ t("restore.search.clear") }}
        </button>
      </form>
    </div>

    <!-- Breadcrumb (browsing only) -->
    <nav
      v-if="!restore.isSearching"
      class="flex flex-wrap items-center gap-1 text-sm text-zinc-600 dark:text-zinc-400"
      :aria-label="t('restore.breadcrumb.label')"
    >
      <button type="button" class="hover:underline" @click="restore.goToBreadcrumb(-1)">
        {{ t("restore.breadcrumb.root") }}
      </button>
      <template v-for="(seg, i) in restore.breadcrumbs" :key="i">
        <span aria-hidden="true">/</span>
        <button type="button" class="hover:underline" @click="restore.goToBreadcrumb(i)">
          {{ seg }}
        </button>
      </template>
    </nav>

    <!-- Error -->
    <p
      v-if="restore.errorCode"
      class="rounded border border-red-300 bg-red-50 px-3 py-2 text-sm text-red-700 dark:border-red-800 dark:bg-red-950 dark:text-red-300"
    >
      {{ t(`errors.${restore.errorCode}.long`) }}
    </p>

    <!-- Loading -->
    <p v-if="restore.loading" class="text-sm text-zinc-500">
      {{ t("restore.loading") }}
    </p>

    <!-- Empty state -->
    <p
      v-else-if="restore.isEmpty"
      class="rounded border border-dashed border-zinc-300 px-4 py-8 text-center text-zinc-500 dark:border-zinc-700"
    >
      {{ restore.isSearching ? t("restore.empty.search") : t("restore.empty.tree") }}
    </p>

    <!-- Truncation notice (M8-P2-1): the folder listing was capped. Shown above
         the list (not part of the loading/empty v-if chain). -->
    <p
      v-if="!restore.loading && !restore.isSearching && restore.treeTruncated"
      class="rounded border border-amber-300 bg-amber-50 px-3 py-2 text-sm text-amber-800 dark:border-amber-800 dark:bg-amber-950 dark:text-amber-300"
    >
      {{ t("restore.truncated", { count: restore.nodes.length }) }}
    </p>

    <!-- File / folder list -->
    <ul
      v-if="!restore.loading && !restore.isEmpty"
      class="divide-y divide-zinc-200 rounded border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800"
    >
      <li
        v-for="row in restore.rows"
        :key="restore.keyOf(restore.sourceId ?? '', row.relativePath)"
        class="flex items-center gap-3 px-3 py-2 text-sm"
      >
        <!-- A folder node (tree only): click to descend. -->
        <template v-if="!restore.isSearching && 'isDir' in row && row.isDir">
          <span class="w-5 text-center" aria-hidden="true">[+]</span>
          <button
            type="button"
            class="flex-1 text-left font-medium hover:underline"
            @click="restore.openFolder(row.relativePath)"
          >
            {{ row.name }}
          </button>
          <span class="text-zinc-400">{{ t("restore.node.folder") }}</span>
        </template>

        <!-- A file node: a checkbox to select it for restore. -->
        <template v-else>
          <input
            type="checkbox"
            class="h-4 w-4"
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
          <span v-if="'size' in row" class="text-zinc-400">{{ formatBytes(row.size) }}</span>
          <span v-if="!row.restorable" class="text-amber-600 dark:text-amber-400">{{
            t("restore.node.notUploaded")
          }}</span>
        </template>
      </li>
    </ul>

    <!-- Action bar: selection + destination + restore -->
    <div
      class="flex flex-wrap items-center gap-3 border-t border-zinc-200 pt-3 dark:border-zinc-800"
    >
      <span class="text-sm text-zinc-600 dark:text-zinc-400">
        {{ t("restore.selectedCount", { count: restore.selectedCount }) }}
      </span>
      <button
        v-if="restore.selectedCount > 0"
        type="button"
        class="text-sm text-zinc-500 hover:underline"
        @click="restore.clearSelection"
      >
        {{ t("restore.clearSelection") }}
      </button>

      <span class="flex-1" />

      <button
        type="button"
        class="rounded bg-zinc-200 px-3 py-1 text-sm hover:bg-zinc-300 dark:bg-zinc-700 dark:hover:bg-zinc-600"
        @click="restore.pickDestination"
      >
        {{ t("restore.pickDestination") }}
      </button>
      <span
        v-if="restore.destPath"
        class="max-w-xs truncate text-sm text-zinc-500"
        :title="restore.destPath"
      >
        {{ restore.destPath }}
      </span>

      <button
        type="button"
        class="rounded bg-emerald-600 px-4 py-1 text-sm font-medium text-white hover:bg-emerald-700 disabled:cursor-not-allowed disabled:opacity-50"
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
        class="rounded bg-red-600 px-4 py-1 text-sm font-medium text-white hover:bg-red-700 disabled:cursor-not-allowed disabled:opacity-50"
        :disabled="restore.cancelling"
        @click="restore.cancelRestore"
      >
        {{ restore.cancelling ? t("restore.cancelling") : t("restore.cancel") }}
      </button>
    </div>

    <!-- Live restore progress -->
    <div
      v-if="restore.job"
      class="space-y-2 rounded border border-zinc-200 p-3 dark:border-zinc-800"
    >
      <div class="flex items-center justify-between text-sm">
        <span class="font-medium">{{
          restore.job.cancelled
            ? t("restore.progress.cancelled")
            : restore.job.done
              ? t("restore.progress.done")
              : t("restore.progress.running")
        }}</span>
        <span class="text-zinc-500">{{
          t("restore.progress.summary", {
            completed: restore.job.completedFiles,
            total: restore.job.totalFiles,
            failed: restore.job.failedFiles,
          })
        }}</span>
      </div>
      <!-- Overall bar -->
      <div
        class="h-2 w-full overflow-hidden rounded bg-zinc-200 dark:bg-zinc-700"
        role="progressbar"
        :aria-valuenow="overallPercent"
        aria-valuemin="0"
        aria-valuemax="100"
      >
        <div
          class="h-full bg-emerald-600 transition-all"
          :style="{ width: overallPercent + '%' }"
        />
      </div>
      <p v-if="restore.job.currentFile" class="truncate text-xs text-zinc-500">
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
          <span v-else-if="f.state === 'restoring'" class="text-zinc-500">{{
            t("restore.file.restoring")
          }}</span>
          <span v-else-if="f.state === 'cancelled'" class="text-zinc-500">{{
            t("restore.file.cancelled")
          }}</span>
          <span v-else class="text-zinc-400">{{ t("restore.file.pending") }}</span>
        </li>
      </ul>
    </div>
  </section>
</template>
