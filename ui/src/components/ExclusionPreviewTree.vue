<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref } from "vue";
import { useI18n } from "vue-i18n";

import {
  anchoredPatternForPath,
  createExclusionPreview,
  type PreviewTreeNode,
} from "../stores/exclusionPreview";

// The exclusion editor's LIVE folder-tree preview (SPEC s11.2; DESIGN s8.5
// step 3), used by BOTH the add-source wizard's exclusions step and the inline
// per-source editor in SourceTable.
//
// It replaces a minutes-long "Loading..." with the tree as it is walked: the
// backend streams `exclusion_preview:batch` events and the controller in
// `stores/exclusionPreview` folds them into an incremental index, so rows appear
// while the scan is still running and the running counts stay live. Every folder
// starts COLLAPSED and a folder's children are only rendered once it is
// expanded, so the DOM stays small no matter how large the streamed tree is; a
// folder with a huge number of children is additionally paged
// (`CHILD_PAGE` at a time behind a "show more" row).
//
// Each row carries the one action that can change its verdict: an EXCLUDED row
// offers "+" (append a re-include glob), an INCLUDED row offers "-" (append an
// exclude glob). The glob comes from `anchoredPatternForPath`, whose exact form
// is pinned against the real matcher in the Rust exclude tests; the parent
// appends it to the matching patterns textarea and calls `restart()`, so the
// tree immediately re-classifies under the new rule.
const { t, locale } = useI18n();

const props = defineProps<{
  /** Preview an EXISTING source by id (the backend resolves its local path). */
  sourceId?: string | null;
  /** Preview a NEW candidate folder by its one-shot dialog token. */
  localPathToken?: string | null;
  respectGitignore: boolean;
  includePatterns: string[];
  excludePatterns: string[];
  /** Issue #305: near-fullscreen sizing for a context that gives the tree the
   * whole viewport (the add-source wizard's exclusions step, sized up per the
   * v2.12 mockup) - the tree FLEXES to fill its container instead of capping
   * at a fixed height. Defaults to false, which keeps today's capped-height
   * behaviour for SourceTable's inline per-source editor unchanged. */
  fill?: boolean;
}>();

const emit = defineEmits<{
  /** Append this glob to the INCLUDE patterns (a "+" on an excluded row). */
  "append-include": [pattern: string];
  /** Append this glob to the EXCLUDE patterns (a "-" on an included row). */
  "append-exclude": [pattern: string];
}>();

/** Children rendered per folder before the "show more" row. Bounds the DOM even
 *  for a directory holding tens of thousands of siblings. */
const CHILD_PAGE = 200;

const preview = createExclusionPreview();

/** Paths of the folders the user has expanded. Everything starts collapsed. */
const expanded = ref(new Set<string>());
/** Per-container render limit, keyed by the parent's path ("" = the root). */
const shownLimit = ref(new Map<string, number>());

let teardown: (() => void) | null = null;
/** Subscription INTENT, re-checked after `subscribe()` resolves.
 *
 * `subscribe()` is async (three `listen()` round-trips), so a component that
 * unmounts while it is in flight would run `onUnmounted` with `teardown` still
 * null and tear down nothing - stranding all three listeners for the life of
 * the process. Those listeners are registered globally BY EVENT NAME, so the
 * orphan keeps receiving every later preview's `exclusion_preview:batch`, and
 * its controller (whose generation id never resolved) parks each one forever.
 * The editor opens and closes on `v-if`, so losing that race is a normal
 * interaction, not a pathological one.
 *
 * Same shape as `activity.ts`'s `desiredSubscribed`: flip the intent first,
 * then have the resolving side honour it. */
let subscribeWanted = false;

onMounted(async () => {
  subscribeWanted = true;
  const stop = await preview.subscribe();
  if (!subscribeWanted) {
    // Unmounted while subscribing: tear the listeners down now, and do NOT
    // start a walk nobody is rendering.
    stop();
    return;
  }
  teardown = stop;
  await restart();
});

onUnmounted(() => {
  subscribeWanted = false;
  teardown?.();
  teardown = null;
});

/** (Re)start the classification with the CURRENT rules. Called on mount, and by
 * the parent editor whenever a rule changes (textarea blur, gitignore toggle, or
 * a "+"/"-" click). The backend supersedes the pass this replaces.
 *
 * Expansion and paging state deliberately SURVIVE a restart. A rule edit
 * re-classifies the same folder, so the folders the user opened to inspect are
 * exactly the ones they still want open - collapsing the tree on every keystroke
 * (which is what resetting these did) threw away their place in it. Paths are
 * stable keys, so a path that stops being streamed simply never matches. */
async function restart(): Promise<void> {
  await preview.start({
    sourceId: props.sourceId ?? undefined,
    localPathToken: props.localPathToken ?? undefined,
    respectGitignore: props.respectGitignore,
    includePatterns: props.includePatterns,
    excludePatterns: props.excludePatterns,
  });
}

defineExpose({ restart });

/** One rendered line: either a tree node or the "show more" affordance that
 *  stands in for the rest of a paged container. */
type Row =
  | { kind: "node"; key: string; node: PreviewTreeNode; level: number }
  | { kind: "more"; key: string; parentPath: string; remaining: number; level: number };

/** Flatten the visible part of the tree: a folder contributes its children only
 * while it is expanded, so this list (and therefore the DOM) stays proportional
 * to what the user has actually opened, not to the streamed node count. */
const rows = computed<Row[]>(() => {
  // The index is deliberately non-reactive (see stores/exclusionPreview); this
  // read is what re-runs the flatten once per coalesced flush.
  void preview.treeVersion.value;
  const out: Row[] = [];
  const walk = (children: PreviewTreeNode[], parentPath: string, level: number): void => {
    const limit = shownLimit.value.get(parentPath) ?? CHILD_PAGE;
    for (const node of children.slice(0, limit)) {
      out.push({ kind: "node", key: node.path, node, level });
      if (node.isDir && expanded.value.has(node.path)) {
        walk(node.children, node.path, level + 1);
      }
    }
    if (children.length > limit) {
      out.push({
        kind: "more",
        key: `more:${parentPath}`,
        parentPath,
        remaining: children.length - limit,
        level,
      });
    }
  };
  walk(preview.roots.value, "", 0);
  return out;
});

const numberFormatter = computed(() => new Intl.NumberFormat(locale.value));

function formatBytes(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const rounded =
    unit === 0
      ? value.toString()
      : value.toLocaleString(locale.value, { maximumFractionDigits: 1 });
  return `${rounded} ${units[unit]}`;
}

function toggle(node: PreviewTreeNode): void {
  const next = new Set(expanded.value);
  if (next.has(node.path)) next.delete(node.path);
  else next.add(node.path);
  expanded.value = next;
}

function showMore(parentPath: string): void {
  const next = new Map(shownLimit.value);
  next.set(parentPath, (next.get(parentPath) ?? CHILD_PAGE) + CHILD_PAGE);
  shownLimit.value = next;
}

/** The glob a row's action button would append, or null when the path cannot be
 *  expressed as a single glob line (then the button is not offered at all). */
function patternFor(node: PreviewTreeNode): string | null {
  return anchoredPatternForPath(node.path, node.isDir);
}

/** Append the row's glob to the matching patterns list and re-classify. An
 * EXCLUDED row emits an INCLUDE glob (bring it back), an INCLUDED row emits an
 * EXCLUDE glob (take it out). The parent appends the line to its textarea and
 * the tree restarts under the new rules.
 *
 * The `nextTick` is load-bearing: the parent applies the append to its own
 * textarea state, and `restart()` reads the rules back out of `props`. Without
 * waiting for the parent's re-render to push the new props down, every click
 * would re-scan with the rules as they stood BEFORE it - the row the user just
 * clicked would come back unchanged. */
async function applyRule(node: PreviewTreeNode): Promise<void> {
  const pattern = patternFor(node);
  if (pattern === null) return;
  if (node.included) emit("append-exclude", pattern);
  else emit("append-include", pattern);
  await nextTick();
  await restart();
}
</script>

<template>
  <div
    class="space-y-2"
    :class="fill ? 'flex h-full min-h-0 flex-col' : ''"
    data-testid="exclusion-preview"
  >
    <!-- Running summary: counts + bytes stay EXACT even when the tree below is
         truncated, and the scan state is spelled out in words.

         While a rule edit is being re-classified the PREVIOUS numbers stay put,
         dimmed - they are the last true answer, and blanking them to zero for a
         frame read as "your rule excluded everything". -->
    <div
      class="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm transition-opacity duration-150"
      :class="preview.recomputing.value ? 'opacity-60' : ''"
    >
      <span class="text-zinc-700 dark:text-zinc-300">
        {{
          t("settings.addSource.preview.included", {
            count: numberFormatter.format(preview.includedCount.value),
          })
        }}
      </span>
      <span class="text-zinc-500 dark:text-zinc-400">
        {{
          t("settings.addSource.preview.includedBytes", {
            size: formatBytes(preview.includedBytes.value),
          })
        }}
      </span>
      <span class="text-zinc-500 dark:text-zinc-400">
        {{
          t("settings.addSource.preview.excluded", {
            count: numberFormatter.format(preview.excludedCount.value),
          })
        }}
      </span>
      <span class="text-zinc-500 dark:text-zinc-400" data-testid="preview-excluded-bytes">
        {{
          t("settings.addSource.preview.excludedBytes", {
            size: formatBytes(preview.excludedBytes.value),
          })
        }}
      </span>
      <span
        v-if="preview.recomputing.value"
        class="inline-flex items-center gap-1.5 text-xs text-zinc-500 dark:text-zinc-400"
        data-testid="preview-recomputing"
        role="status"
      >
        <span
          class="size-1.5 animate-pulse rounded-full bg-zinc-400 dark:bg-zinc-500"
          aria-hidden="true"
        />
        {{ t("settings.exclusionPreview.recomputing") }}
      </span>
      <span
        v-else-if="preview.scanning.value"
        class="inline-flex items-center gap-1.5 text-xs text-teal-700 dark:text-teal-300"
        data-testid="preview-scanning"
        role="status"
      >
        <span
          class="size-1.5 animate-pulse rounded-full bg-teal-600 dark:bg-teal-400"
          aria-hidden="true"
        />
        {{ t("settings.exclusionPreview.scanning") }}
      </span>
      <span
        v-else-if="preview.complete.value"
        class="text-xs text-zinc-500 dark:text-zinc-400"
        data-testid="preview-complete"
        role="status"
      >
        {{ t("settings.exclusionPreview.complete") }}
      </span>
    </div>

    <p v-if="preview.errorCode.value" class="text-sm text-red-600" role="alert">
      {{ t(`errors.${preview.errorCode.value}.long`) }}
    </p>

    <div
      v-else
      class="overflow-auto rounded-md border border-zinc-200 bg-zinc-50/60 p-1 transition-opacity duration-150 dark:border-zinc-700 dark:bg-zinc-950/40"
      :class="[preview.recomputing.value ? 'opacity-60' : '', fill ? 'flex-1 min-h-0' : 'max-h-64']"
    >
      <p
        v-if="rows.length === 0"
        class="px-2 py-3 text-xs text-zinc-500 dark:text-zinc-400"
        data-testid="preview-empty"
      >
        {{
          preview.scanning.value
            ? t("settings.exclusionPreview.scanning")
            : t("settings.exclusionPreview.empty")
        }}
      </p>
      <ul v-else role="tree" :aria-label="t('settings.exclusionPreview.treeLabel')" class="text-xs">
        <template v-for="row in rows" :key="row.key">
          <li
            v-if="row.kind === 'node'"
            role="treeitem"
            :aria-level="row.level + 1"
            :aria-expanded="row.node.isDir ? expanded.has(row.node.path) : undefined"
            :data-testid="`preview-row-${row.node.path}`"
            class="group flex items-center gap-1 rounded-sm px-1 py-0.5 hover:bg-zinc-200/60 dark:hover:bg-zinc-800/60"
            :style="{ paddingLeft: `${row.level * 14 + 4}px` }"
          >
            <!-- Expand / collapse. A file (or a folder with nothing streamed
                 under it, e.g. one pruned as excluded) gets an inert spacer so
                 every row's label still lines up. -->
            <button
              v-if="row.node.isDir && row.node.children.length > 0"
              type="button"
              class="flex size-4 shrink-0 items-center justify-center rounded-xs text-zinc-500 transition-colors hover:text-teal-700 focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-teal-500 dark:text-zinc-400 dark:hover:text-teal-300"
              :aria-label="
                expanded.has(row.node.path)
                  ? t('settings.exclusionPreview.collapseAction', { name: row.node.name })
                  : t('settings.exclusionPreview.expandAction', { name: row.node.name })
              "
              @click="toggle(row.node)"
            >
              <svg
                viewBox="0 0 16 16"
                class="size-3 transition-transform"
                :class="expanded.has(row.node.path) ? 'rotate-90' : ''"
                fill="currentColor"
                aria-hidden="true"
              >
                <path d="M6 3.5 10.5 8 6 12.5z" />
              </svg>
            </button>
            <span v-else class="size-4 shrink-0" aria-hidden="true" />

            <!-- Verdict marker: colour AND a distinct glyph AND (below) a
                 strikethrough, so the include/exclude split never depends on
                 colour alone. -->
            <svg
              v-if="row.node.included"
              viewBox="0 0 16 16"
              class="size-3.5 shrink-0 text-teal-600 dark:text-teal-400"
              fill="currentColor"
              aria-hidden="true"
            >
              <path
                d="M8 1.5a6.5 6.5 0 1 0 0 13 6.5 6.5 0 0 0 0-13m3.1 4.6-3.9 4a.75.75 0 0 1-1.08 0L4.9 8.8a.75.75 0 1 1 1.08-1.04l1.14 1.18 3.36-3.46A.75.75 0 0 1 11.6 6.1z"
              />
            </svg>
            <svg
              v-else
              viewBox="0 0 16 16"
              class="size-3.5 shrink-0 text-zinc-400 dark:text-zinc-500"
              fill="currentColor"
              aria-hidden="true"
            >
              <path
                d="M8 1.5a6.5 6.5 0 1 0 0 13 6.5 6.5 0 0 0 0-13m0 1.5c1.15 0 2.2.38 3.05 1.02L4.02 11.05A5 5 0 0 1 8 3m0 10a4.98 4.98 0 0 1-3.05-1.02l7.03-7.03A5 5 0 0 1 8 13"
              />
            </svg>
            <span class="sr-only">{{
              row.node.included
                ? t("settings.exclusionPreview.includedLabel")
                : t("settings.exclusionPreview.excludedLabel")
            }}</span>

            <!-- Folder / file glyph. -->
            <svg
              viewBox="0 0 16 16"
              class="size-3.5 shrink-0 text-zinc-400 dark:text-zinc-500"
              fill="currentColor"
              aria-hidden="true"
            >
              <path
                v-if="row.node.isDir"
                d="M1.5 3.5A1.5 1.5 0 0 1 3 2h3.1l1.4 1.5H13A1.5 1.5 0 0 1 14.5 5v6A1.5 1.5 0 0 1 13 12.5H3A1.5 1.5 0 0 1 1.5 11z"
              />
              <path v-else d="M3.5 1.5h5L12.5 5.5v9h-9zm5 .8V6h3.7z" />
            </svg>

            <span
              class="truncate"
              :class="
                row.node.included
                  ? 'text-zinc-800 dark:text-zinc-100'
                  : 'text-zinc-400 line-through dark:text-zinc-500'
              "
              :title="row.node.path"
              >{{ row.node.name }}</span
            >

            <span v-if="!row.node.isDir" class="shrink-0 text-zinc-400 dark:text-zinc-600">
              {{ formatBytes(row.node.size) }}
            </span>
            <!-- Issue #305: the folder's rollup - how much is under it,
                 regardless of any one child's own verdict. Settles as the
                 walk streams more of the subtree in (a descended folder) or
                 is already final on arrival (a pruned excluded folder - see
                 the store's PreviewTreeNode doc). -->
            <span
              v-else
              class="shrink-0 text-zinc-400 dark:text-zinc-600"
              :data-testid="`preview-rollup-${row.node.path}`"
            >
              {{
                t("settings.exclusionPreview.rollup", {
                  count: numberFormatter.format(row.node.fileCount),
                  size: formatBytes(row.node.byteSize),
                })
              }}
            </span>

            <!-- The one action that flips this row: "+" re-includes an excluded
                 path, "-" excludes an included one. -->
            <button
              v-if="patternFor(row.node) !== null"
              type="button"
              class="ml-auto flex size-5 shrink-0 items-center justify-center rounded-sm border text-sm leading-none transition-colors focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-teal-500"
              :class="
                row.node.included
                  ? 'border-zinc-300 text-zinc-500 hover:border-red-400 hover:bg-red-50 hover:text-red-600 dark:border-zinc-700 dark:text-zinc-400 dark:hover:border-red-800 dark:hover:bg-red-950/40 dark:hover:text-red-300'
                  : 'border-zinc-300 text-zinc-500 hover:border-teal-500 hover:bg-teal-50 hover:text-teal-700 dark:border-zinc-700 dark:text-zinc-400 dark:hover:border-teal-700 dark:hover:bg-teal-950/40 dark:hover:text-teal-300'
              "
              :aria-label="
                row.node.included
                  ? t('settings.exclusionPreview.excludeAction', { path: row.node.path })
                  : t('settings.exclusionPreview.includeAction', { path: row.node.path })
              "
              :data-testid="`preview-action-${row.node.path}`"
              @click="applyRule(row.node)"
            >
              {{ row.node.included ? "-" : "+" }}
            </button>
          </li>

          <li v-else role="none" :style="{ paddingLeft: `${row.level * 14 + 24}px` }">
            <button
              type="button"
              class="px-1 py-0.5 text-teal-700 underline-offset-2 hover:underline focus-visible:outline-solid focus-visible:outline-2 focus-visible:outline-teal-500 dark:text-teal-300"
              @click="showMore(row.parentPath)"
            >
              {{
                t("settings.exclusionPreview.showMore", {
                  count: numberFormatter.format(row.remaining),
                })
              }}
            </button>
          </li>
        </template>
      </ul>
    </div>

    <p
      v-if="preview.truncated.value"
      class="text-xs text-zinc-500 dark:text-zinc-400"
      data-testid="preview-truncated"
    >
      {{ t("settings.exclusionPreview.truncated") }}
    </p>
  </div>
</template>
