<script setup lang="ts">
import { computed, onBeforeUnmount, ref } from "vue";
import { useI18n } from "vue-i18n";

import DropdownPanel from "./DropdownPanel.vue";
import { formatBytes } from "../stores/formatBytes";
import { useProgressStore } from "../stores/progress";
import { useWorkQueueStore, type QueueRow } from "../stores/workQueue";
import type { WorkKind } from "../ipc/types";

// The top-bar work-queue menu (issue #304): what is running, what is waiting,
// and an X on each. Backups queue up from crash recovery, watcher ticks,
// "Back up now", and the scheduled timer; until this existed they coalesced
// invisibly, so a user closing the app had no way to know work was outstanding.
//
// The panel is default-COLLAPSED - it is a check-on-demand affordance, not a
// permanent list - and its badge carries the only number you need at a glance:
// pending plus running.

const { t, locale } = useI18n();
const queue = useWorkQueueStore();
const progress = useProgressStore();

const open = ref(false);

/** Ticked only while the panel is open, so the relative "queued 2 min ago"
 * labels stay honest without the app re-rendering once a second when nothing
 * is on screen. Read at render time, never accumulated, so a suspend/resume
 * cannot leave it drifting. */
const now = ref(Date.now());
let ticker: ReturnType<typeof setInterval> | null = null;

function onOpenChange(isOpen: boolean): void {
  if (isOpen) {
    now.value = Date.now();
    ticker ??= setInterval(() => (now.value = Date.now()), 30_000);
  } else if (ticker) {
    clearInterval(ticker);
    ticker = null;
  }
}

// Unmounting with the panel open (a route swap that re-creates the shell) must
// not leave the interval running against a dead component.
onBeforeUnmount(() => {
  if (ticker) clearInterval(ticker);
  ticker = null;
});

/** Panel rows, each paired with the running item's live counters (resolved once
 * per render rather than re-derived per template expression). */
const rows = computed(() =>
  queue.rows.map((row) => ({
    row,
    key: `${row.accountId}:${row.item.id}`,
    progress: row.running ? runningProgress(row) : null,
  }))
);
const count = computed(() => queue.count);

/** The title line: what this item is, plus the source it came from when the
 * request named one ("Backing up - Documents"). A row whose source we cannot
 * name simply shows the kind - never a raw uuid. */
function title(row: QueueRow): string {
  const kind = row.running
    ? row.draining
      ? t("queue.title.stopping")
      : t("queue.title.running")
    : pendingTitle(row.item.kind);
  return row.sourceName ? `${kind} · ${row.sourceName}` : kind;
}

/** The title of a PENDING row. Spelled out per kind rather than built from a
 * template key so the i18n lint can see every key that is actually in use. */
function pendingTitle(kind: WorkKind): string {
  switch (kind) {
    case "recovery":
      return t("queue.title.recovery");
    case "watcher":
      return t("queue.title.watcher");
    case "manual":
      return t("queue.title.manual");
    default:
      return t("queue.title.scheduled");
  }
}

/** The running item's own counters (files done / total, bytes left), taken from
 * the progress store PER ACCOUNT - with two accounts running, the aggregate
 * would be the wrong number for either row. Null until the account is actually
 * uploading (the scan / plan phases carry no totals). */
function runningProgress(row: QueueRow): { label: string; percent: number | null } | null {
  const p = progress.accountProgress(row.accountId);
  if (!p) return null;
  const bytesLeft = Math.max(0, p.bytes_total - p.bytes_done);
  const label = t("queue.sub.running", {
    done: p.files_done.toLocaleString(locale.value),
    total: p.files_total.toLocaleString(locale.value),
    left: formatBytes(bytesLeft, locale.value),
  });
  const opsTotal = p.files_total + p.trashes_total;
  const percent =
    p.bytes_total > 0 && p.trashes_total === 0
      ? p.bytes_done / p.bytes_total
      : opsTotal > 0
        ? (p.files_done + p.trashes_done) / opsTotal
        : null;
  return { label, percent: percent === null ? null : Math.min(1, Math.max(0, percent)) };
}

/** The subtitle for a PENDING row: why it is queued, and how long it has been. */
function subtitle(row: QueueRow): string {
  const ago = relativeAge(row.item.enqueued_at);
  switch (row.item.kind) {
    case "recovery":
      return t("queue.sub.recovery");
    case "watcher":
      return t("queue.sub.watcher", { ago });
    case "manual":
      return t("queue.sub.manual", { ago });
    default:
      return t("queue.sub.scheduled", { ago });
  }
}

/** "just now" / "4 min ago" / "2 hr ago". Whole units only - a queue row does
 * not need seconds, and a ticking seconds counter would be noise. */
function relativeAge(atMs: number): string {
  const minutes = Math.floor(Math.max(0, now.value - atMs) / 60_000);
  if (minutes < 1) return t("queue.age.justNow");
  if (minutes < 60) return t("queue.age.minutes", { n: minutes });
  return t("queue.age.hours", { n: Math.floor(minutes / 60) });
}

/** The next scheduled scan as a local HH:MM, for the empty state. */
const nextScheduledLabel = computed<string | null>(() => {
  const at = queue.nextScheduledAt;
  if (at === null) return null;
  return new Date(at).toLocaleTimeString(locale.value, {
    hour: "2-digit",
    minute: "2-digit",
  });
});

const emptyLabel = computed<string>(() =>
  nextScheduledLabel.value
    ? t("queue.emptyWithNext", { time: nextScheduledLabel.value })
    : t("queue.empty")
);

async function cancelRow(row: QueueRow): Promise<void> {
  try {
    await queue.cancel(row.accountId, row.item.id);
  } catch {
    // The store already logged it and re-hydrated, so the panel is showing the
    // truth again; there is nothing further this menu can do about it.
  }
}

async function clearAll(): Promise<void> {
  try {
    await queue.clearAll();
  } catch {
    // Same as above: the store re-hydrated, the list is honest.
  }
}

const ICON_BUTTON =
  "rounded-sm p-0.5 text-zinc-400 transition-colors hover:text-zinc-700 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:hover:text-zinc-200";
</script>

<template>
  <DropdownPanel
    id="work-queue"
    v-model:open="open"
    :panel-label="t('queue.title.panel')"
    :trigger-label="t('queue.trigger', { count })"
    trigger-class="relative rounded-sm px-1.5 py-1 text-zinc-600 transition-colors hover:text-teal-700 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:text-zinc-400 dark:hover:text-teal-300"
    data-testid="work-queue"
    @update:open="onOpenChange"
  >
    <template #trigger>
      <!-- List glyph + count badge (the mockup's top-bar affordance). -->
      <svg
        class="h-5 w-5"
        viewBox="0 0 20 20"
        fill="none"
        stroke="currentColor"
        stroke-width="1.6"
        stroke-linecap="round"
        aria-hidden="true"
      >
        <path d="M7 5h9M7 10h9M7 15h9M3.5 5h.01M3.5 10h.01M3.5 15h.01" />
      </svg>
      <span
        v-if="count > 0"
        class="absolute -top-1 -right-1 min-w-[1.05rem] rounded-full bg-teal-600 px-1 text-[0.65rem] leading-4 font-semibold text-white dark:bg-teal-500"
        data-testid="work-queue-badge"
        >{{ count }}</span
      >
    </template>

    <div
      class="flex items-center justify-between border-b border-zinc-200 px-4 py-2.5 dark:border-zinc-700"
    >
      <span class="text-sm font-semibold text-zinc-800 dark:text-zinc-100">{{
        t("queue.title.panel")
      }}</span>
      <button
        v-if="queue.clearable"
        type="button"
        class="rounded-sm text-xs font-medium text-teal-700 transition-colors hover:text-teal-600 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal-500 dark:text-teal-300 dark:hover:text-teal-200"
        data-testid="work-queue-clear-all"
        @click="clearAll"
      >
        {{ t("queue.clearAll") }}
      </button>
    </div>

    <p
      v-if="rows.length === 0"
      class="px-4 py-5 text-center text-xs text-zinc-500 dark:text-zinc-400"
      data-testid="work-queue-empty"
    >
      {{ emptyLabel }}
    </p>

    <ul v-else class="divide-y divide-zinc-200 dark:divide-zinc-700">
      <li
        v-for="{ row, key, progress: rowProgress } in rows"
        :key="key"
        class="flex items-start gap-2.5 px-4 py-3"
        data-testid="work-queue-item"
      >
        <span class="mt-0.5 shrink-0 text-zinc-400 dark:text-zinc-500" aria-hidden="true">
          <!-- Running: a spinner. Pending: one glyph per kind, so the list is
               scannable without reading every title. -->
          <svg
            v-if="row.running"
            class="h-4 w-4 animate-spin text-teal-600 dark:text-teal-400"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            stroke-width="1.8"
          >
            <circle cx="8" cy="8" r="6" class="opacity-25" />
            <path d="M14 8a6 6 0 0 0-6-6" stroke-linecap="round" />
          </svg>
          <svg
            v-else
            class="h-4 w-4"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
            stroke-linejoin="round"
          >
            <template v-if="row.item.kind === 'recovery'">
              <circle cx="8" cy="8" r="6" />
              <path d="M8 4.5V8l2.5 1.5" />
            </template>
            <template v-else-if="row.item.kind === 'watcher'">
              <path d="M1.5 8s2.4-4 6.5-4 6.5 4 6.5 4-2.4 4-6.5 4-6.5-4-6.5-4Z" />
              <circle cx="8" cy="8" r="1.6" />
            </template>
            <template v-else-if="row.item.kind === 'manual'">
              <path d="M8 14V5.5a1.2 1.2 0 0 1 2.4 0V8" />
              <path d="M5.6 8V3.6a1.2 1.2 0 0 1 2.4 0" />
              <path d="M10.4 8V6.2a1.2 1.2 0 0 1 2.4 0V10a4 4 0 0 1-4 4H7.2a4 4 0 0 1-3.4-1.9" />
            </template>
            <template v-else>
              <rect x="2" y="3.5" width="12" height="10.5" rx="1.5" />
              <path d="M2 6.5h12M5.5 1.8v2.4M10.5 1.8v2.4" />
            </template>
          </svg>
        </span>

        <div class="min-w-0 flex-1">
          <p class="truncate text-xs font-semibold text-zinc-800 dark:text-zinc-100">
            {{ title(row) }}
          </p>
          <template v-if="row.running">
            <p
              v-if="rowProgress"
              class="mt-0.5 font-mono text-[0.7rem] text-zinc-500 dark:text-zinc-400"
              data-testid="work-queue-running-sub"
            >
              {{ rowProgress.label }}
            </p>
            <p v-else class="mt-0.5 text-[0.7rem] text-zinc-500 dark:text-zinc-400">
              {{ row.draining ? t("queue.sub.stopping") : t("queue.sub.starting") }}
            </p>
            <div
              v-if="rowProgress && rowProgress.percent !== null"
              class="mt-1.5 h-1 w-full overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700"
              role="progressbar"
              :aria-valuenow="Math.round(rowProgress.percent * 100)"
              aria-valuemin="0"
              aria-valuemax="100"
              data-testid="work-queue-progress"
            >
              <div
                class="h-full rounded-full bg-teal-600 transition-[width] dark:bg-teal-500"
                :style="{ width: `${Math.round(rowProgress.percent * 100)}%` }"
              />
            </div>
          </template>
          <p v-else class="mt-0.5 text-[0.7rem] text-zinc-500 dark:text-zinc-400">
            {{ subtitle(row) }}
          </p>
        </div>

        <button
          type="button"
          :class="ICON_BUTTON"
          :aria-label="t('queue.cancelItem', { item: title(row) })"
          :disabled="row.draining"
          :title="row.running ? t('queue.cancelRunningHint') : t('queue.cancelPendingHint')"
          data-testid="work-queue-cancel"
          @click="cancelRow(row)"
        >
          <svg
            class="h-4 w-4"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            stroke-width="1.6"
            stroke-linecap="round"
            aria-hidden="true"
          >
            <path d="M4 4l8 8M12 4l-8 8" />
          </svg>
        </button>
      </li>
    </ul>

    <!-- The one thing a queue must explain: why item two is not moving yet. -->
    <p
      v-if="rows.length > 0"
      class="border-t border-zinc-200 px-4 py-2 text-[0.7rem] text-zinc-500 dark:border-zinc-700 dark:text-zinc-400"
      data-testid="work-queue-footer"
    >
      {{ t("queue.footer") }}
    </p>
  </DropdownPanel>
</template>
