import { ref, shallowRef } from "vue";

import * as ipc from "../ipc/commands";
import {
  onExclusionPreviewBatch,
  onExclusionPreviewDone,
  onExclusionPreviewError,
} from "../ipc/events";
import { toErrorCode } from "../ipc/errors";
import type {
  ExclusionPreviewBatch,
  ExclusionPreviewDone,
  ExclusionPreviewError,
  ExclusionPreviewRequest,
} from "../ipc/types";

// The exclusion editor's LIVE folder tree (SPEC s11.2; DESIGN s8.5 step 3).
//
// The one-shot `preview_exclusions` walks the whole source before it returns, so
// a large folder left the editor showing "Loading..." for minutes. This module
// consumes the STREAMING preview instead: `preview_exclusions_start` returns a
// generation id, the walk pushes `exclusion_preview:batch` events as it goes,
// and one `exclusion_preview:done` closes it out with the exact totals.
//
// Two things make this cheap enough to run on every rule edit:
//
// 1. The tree index is a PLAIN, NON-REACTIVE `Map` + plain node objects. Making
//    50k streamed nodes deeply reactive would cost more than rendering them.
//    Consumers re-read through the `treeVersion` ref, which is bumped exactly
//    ONCE per flush - so a component's `computed` over the visible rows
//    invalidates once per frame, not once per node.
// 2. Batches land in a non-reactive buffer and are applied in a single coalesced
//    flush per animation frame (the same pattern as the activity store's
//    `pendingLive`), so a fast SSD walk streaming thousands of nodes a second
//    still costs one reactive update per frame.
//
// A third thing makes it usable while EDITING rules: a new generation never
// blanks the old one. `start` builds the incoming tree off to the side and swaps
// it in whole on its first batch, so the panel goes from one result straight to
// the next with no empty frame and no counts bouncing through zero. The backend
// keeps the folder tree in memory across a rule edit, so that first batch
// normally lands within a frame or two; `recomputing` covers the gap with a
// dimmed "updating" state rather than a reset.
//
// It is a FACTORY, not a Pinia singleton: the add-source wizard and the inline
// per-source editor can both be mounted, and each needs its own generation,
// its own tree, and its own event subscription.

/** One node of the live preview tree. Plain and mutable by design (see the
 *  module docs) - never wrap these in `reactive`. */
export interface PreviewTreeNode {
  /** Source-root-relative path, forward-slashed (`docs/notes.txt`). */
  path: string;
  /** The last path segment, i.e. what the row displays. */
  name: string;
  isDir: boolean;
  /** The matcher's verdict for this exact entry. */
  included: boolean;
  /** File size in bytes; 0 for a directory. */
  size: number;
  /** Nesting depth below the root (a root child is 0). */
  depth: number;
  /** Children discovered so far, directories first then name-sorted. */
  children: PreviewTreeNode[];
}

/** The glob a "+" / "-" click appends to the include / exclude patterns to
 * target EXACTLY this path.
 *
 * MUST stay character-for-character identical to the Rust
 * `driven_core::exclude::anchored_pattern_for_path`, which is the function the
 * matcher's own tests pin: a leading `/` anchors the glob to the source root (so
 * `/docs/notes.txt` can never also hit `sub/docs/notes.txt`), a trailing `/`
 * marks a directory (which the matcher then applies to everything beneath it),
 * and every glob metacharacter in the path is backslash-escaped so a literal
 * `[`, `{` or `*` in a filename cannot widen the match. The Rust test
 * `anchored_pattern_vectors_are_stable` and the vitest suite assert the SAME
 * table of vectors; change both sides together.
 *
 * Returns `null` when the path cannot be expressed as one glob line - an empty
 * path, one containing a newline (patterns are stored one per line, so it would
 * split into two broken rules), or one ending in non-space whitespace (the
 * gitignore parser trims it and only a trailing SPACE can be protected). The
 * tree then withholds the button rather than appending a rule that would
 * silently match something else.
 */
export function anchoredPatternForPath(rel: string, isDir: boolean): string | null {
  if (rel === "" || rel.includes("\n") || rel.includes("\r")) return null;
  const last = rel[rel.length - 1];
  if (/\s/.test(last) && last !== " ") return null;

  let out = "/";
  for (const ch of rel) {
    if (ch === "\\" || ch === "*" || ch === "?" || ch === "[" || ch === "]") {
      out += "\\";
    } else if (ch === "{" || ch === "}") {
      out += "\\";
    }
    out += ch;
  }
  if (isDir) {
    out += "/";
  } else if (out.endsWith(" ")) {
    // `GitignoreBuilder::add_line` trims trailing whitespace unless the line
    // ends with an escaped space.
    out = `${out.slice(0, -1)}\\ `;
  }
  return out;
}

/** Split a patterns textarea into the glob list the IPC layer takes. Mirrors the
 *  `splitPatterns` both editors already use (newline OR comma separated). */
export function splitPatterns(text: string): string[] {
  return text
    .split(/[\n,]/)
    .map((p) => p.trim())
    .filter((p) => p.length > 0);
}

/** Would this include pattern stop the scanner from PRUNING excluded folders?
 *
 * The walker skips descending into an excluded directory only when it can prove
 * no include pattern could ever re-include something below it. That proof needs
 * a pattern that is anchored to the source root AND bounded in depth, like
 * `/repo/.env` or a leading-slash pattern whose wildcards each cover a single
 * segment - those can only match at a known depth, so `node_modules` can be
 * skipped outright. A relative pattern (`.env`, `blah/.env`) matches at ANY
 * depth, and one containing a double-star spans any number of levels, so either
 * forces the walker to descend into every excluded directory just in case -
 * which is what makes a scan crawl.
 */
export function isUnconstrainedIncludePattern(pattern: string): boolean {
  const trimmed = pattern.trim();
  if (trimmed === "") return false;
  return !trimmed.startsWith("/") || trimmed.includes("**");
}

/** The include patterns in a textarea's raw text that defeat directory pruning,
 *  in the order the user typed them. Empty when the rules are all prune-safe. */
export function unconstrainedIncludePatterns(text: string): string[] {
  return splitPatterns(text).filter(isUnconstrainedIncludePattern);
}

/** Append `pattern` as a NEW LINE to a patterns textarea's text, skipping the
 * append when the exact pattern is already present (clicking "-" twice on the
 * same row must not stack duplicate rules against the source's 256-pattern cap).
 * Returns the new text. */
export function appendPatternLine(text: string, pattern: string): string {
  if (splitPatterns(text).includes(pattern)) return text;
  if (text.trim() === "") return pattern;
  return text.endsWith("\n") ? `${text}${pattern}` : `${text}\n${pattern}`;
}

/** Directories first, then case-insensitive name order - the ordering a file
 *  browser uses, so the live tree reads like the folder it mirrors. */
function compareNodes(a: PreviewTreeNode, b: PreviewTreeNode): number {
  if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
  return a.name.localeCompare(b.name);
}

/** Schedule a callback for the next paint. `requestAnimationFrame` aligns the
 * flush with rendering in the app; under vitest's node environment rAF is
 * undefined, so fall back to a ~1-frame timer that fake timers drive. (Same
 * seam as the activity store.) */
const scheduleFrame: (cb: () => void) => void =
  typeof requestAnimationFrame === "function"
    ? (cb) => {
        requestAnimationFrame(cb);
      }
    : (cb) => {
        setTimeout(cb, 16);
      };

export type ExclusionPreviewController = ReturnType<typeof createExclusionPreview>;

/**
 * Create one live-preview controller: an incremental tree index fed by the
 * streaming preview events, plus the start / cancel lifecycle.
 *
 * Call `subscribe()` once on mount and the returned teardown on unmount.
 */
export function createExclusionPreview() {
  // --- reactive surface (kept deliberately small) --------------------------
  /** Bumped ONCE per flush; the tree is read through this (see module docs). */
  const treeVersion = ref(0);
  /** A walk is in flight (the "still scanning" indicator). */
  const scanning = ref(false);
  /** A NEW generation is being computed while the PREVIOUS one is still on
   * screen (see `start`). Drives a subtle "updating" affordance - never a
   * reset. Clears the instant the new generation's first batch swaps in. */
  const recomputing = ref(false);
  /** The last walk ran to completion (not cancelled): show "scan complete". */
  const complete = ref(false);
  const includedCount = ref(0);
  const excludedCount = ref(0);
  const includedBytes = ref(0);
  /** The streamed TREE hit the node cap; the counts above are still exact. */
  const truncated = ref(false);
  /** A stable SPEC s24 error code from `preview_exclusions_start` (invalid
   *  glob, unresolvable root), or null. The view localizes it. */
  const errorCode = ref<string | null>(null);
  /** The root's children, newest sort applied. A `shallowRef` so replacing the
   *  array on flush notifies without deep-tracking every node. */
  const roots = shallowRef<PreviewTreeNode[]>([]);

  // --- non-reactive index --------------------------------------------------
  /** The tree being BUILT for the current generation. While a recompute is in
   *  flight this is NOT what is on screen (see `displayedIndex`). */
  let index = new Map<string, PreviewTreeNode>();
  let rootChildren: PreviewTreeNode[] = [];
  /** The index matching what `roots` currently renders. Identical to `index`
   *  except during a recompute, when the previous generation is still shown. */
  let displayedIndex = index;
  /** Parents whose `children` gained an entry this flush and so need re-sorting
   *  (sorting once per flush instead of once per insert). */
  let dirtyParents = new Set<PreviewTreeNode | null>();
  /** The previous generation's tree is on screen and the new one has not
   * produced anything yet, so nothing may be published.
   *
   * This is the whole no-flash guarantee: a rule edit used to blank the tree and
   * zero the counts the moment it started, then repopulate - so every keystroke
   * flashed the panel empty and every number bounced through 0. Now the old
   * result stays put (dimmed via `recomputing`) and is replaced ATOMICALLY by
   * the new generation's first batch, which with the backend's folder-tree cache
   * lands in milliseconds. */
  let swapPending = false;

  // --- generation bookkeeping ----------------------------------------------
  /** The generation whose events we accept; null before the first id resolves. */
  let currentId: string | null = null;
  /** Incremented on every `start`, so a slow `preview_exclusions_start` reply
   *  from a superseded call can be recognised and thrown away. */
  let startSeq = 0;
  /** The walk begins before `preview_exclusions_start` resolves, so its first
   *  batches can legitimately arrive BEFORE we know the generation id. They are
   *  parked here and replayed once the id lands (bounded by the backend's own
   *  node cap, so this cannot grow without limit). */
  let preIdBatches: ExclusionPreviewBatch[] = [];
  let preIdDone: ExclusionPreviewDone[] = [];
  let preIdErrors: ExclusionPreviewError[] = [];

  // --- coalesced flush -----------------------------------------------------
  const pending: ExclusionPreviewBatch[] = [];
  let pendingDone: ExclusionPreviewDone | null = null;
  let flushScheduled = false;

  /** Start building a new generation's tree.
   *
   * Deliberately does NOT touch what is rendered when a previous result is
   * showing: the new tree is built off to the side and swapped in whole (see
   * `swapPending`). Only a FIRST preview - nothing on screen to preserve -
   * publishes its empty state, and there the zeroes are the truth rather than a
   * flash.
   */
  function beginGeneration(): void {
    index = new Map();
    rootChildren = [];
    dirtyParents = new Set();
    pending.length = 0;
    pendingDone = null;
    preIdBatches = [];
    preIdDone = [];
    preIdErrors = [];
    complete.value = false;

    swapPending = roots.value.length > 0;
    recomputing.value = swapPending;
    if (!swapPending) {
      displayedIndex = index;
      roots.value = [];
      includedCount.value = 0;
      excludedCount.value = 0;
      includedBytes.value = 0;
      truncated.value = false;
      treeVersion.value += 1;
    }
  }

  /** The container `path`'s children live in, creating any missing ancestor
   * directory rows on the way.
   *
   * The backend streams breadth-first, so a parent always precedes its children
   * and the placeholder branch should never fire. It exists so a node can never
   * be silently dropped for want of a parent: a placeholder is a real,
   * expandable directory row that the genuine node later fills in via
   * `upsert`. */
  function containerFor(path: string): { list: PreviewTreeNode[]; parent: PreviewTreeNode | null } {
    const cut = path.lastIndexOf("/");
    if (cut < 0) return { list: rootChildren, parent: null };
    const parentPath = path.slice(0, cut);
    const existing = index.get(parentPath);
    if (existing) return { list: existing.children, parent: existing };
    const created = upsert(parentPath, true, true, 0);
    return { list: created.children, parent: created };
  }

  /** Insert `path`, or update it in place if it was already seen (a placeholder
   *  ancestor, or a duplicate the backend re-sent). Returns the node. */
  function upsert(path: string, isDir: boolean, included: boolean, size: number): PreviewTreeNode {
    const found = index.get(path);
    if (found) {
      found.isDir = isDir || found.isDir;
      found.included = included;
      found.size = size;
      return found;
    }
    const cut = path.lastIndexOf("/");
    const node: PreviewTreeNode = {
      path,
      name: cut < 0 ? path : path.slice(cut + 1),
      isDir,
      included,
      size,
      depth: 0,
      children: [],
    };
    // `containerFor` may create ancestors, which re-enters `upsert`; register
    // this node FIRST so a cycle of missing parents cannot recurse forever.
    index.set(path, node);
    const { list, parent } = containerFor(path);
    node.depth = parent ? parent.depth + 1 : 0;
    list.push(node);
    dirtyParents.add(parent);
    return node;
  }

  /** Apply every buffered batch in ONE reactive update. */
  function flush(): void {
    flushScheduled = false;
    if (pending.length === 0 && pendingDone === null) return;

    let latest: ExclusionPreviewBatch | null = null;
    for (const batch of pending.splice(0)) {
      for (const node of batch.nodes) {
        upsert(node.path, node.isDir, node.included, node.size);
      }
      latest = batch;
    }

    for (const parent of dirtyParents) {
      if (parent === null) rootChildren.sort(compareNodes);
      else parent.children.sort(compareNodes);
    }
    dirtyParents = new Set();

    const done = pendingDone;
    pendingDone = null;

    if (swapPending) {
      // The new generation has something real to show: swap the whole tree and
      // its totals over in this one update, so the panel goes straight from the
      // old result to the new one with no empty frame in between. A `done` also
      // swaps - it is the generation's final word, even if it found nothing -
      // but a CANCELLED one does not: it means this generation was abandoned,
      // and the tree on screen is still the best answer available.
      if (latest === null && (done === null || done.cancelled)) {
        if (done !== null) {
          scanning.value = false;
          recomputing.value = false;
        }
        return;
      }
      swapPending = false;
      recomputing.value = false;
      displayedIndex = index;
    }

    if (latest !== null) {
      includedCount.value = latest.includedCount;
      excludedCount.value = latest.excludedCount;
      includedBytes.value = latest.includedBytes;
      truncated.value = latest.truncated;
    }

    if (done !== null) {
      includedCount.value = done.includedCount;
      excludedCount.value = done.excludedCount;
      includedBytes.value = done.includedBytes;
      truncated.value = done.truncated;
      scanning.value = false;
      complete.value = !done.cancelled;
    }

    // One assignment for the root list + one version bump: a component's
    // visible-rows computed invalidates exactly once per frame.
    roots.value = rootChildren.slice();
    treeVersion.value += 1;
  }

  function scheduleFlush(): void {
    if (flushScheduled) return;
    flushScheduled = true;
    scheduleFrame(() => {
      if (flushScheduled) flush();
    });
  }

  /** Take a batch from the event stream (or from the pre-id park). */
  function ingestBatch(batch: ExclusionPreviewBatch): void {
    if (currentId === null) {
      preIdBatches.push(batch);
      return;
    }
    // A superseded walk's in-flight events must never touch the live tree.
    if (batch.previewId !== currentId) return;
    pending.push(batch);
    scheduleFlush();
  }

  function ingestDone(done: ExclusionPreviewDone): void {
    if (currentId === null) {
      preIdDone.push(done);
      return;
    }
    if (done.previewId !== currentId) return;
    pendingDone = done;
    scheduleFlush();
  }

  /** Take an `exclusion_preview:error` from the event stream.
   *
   * The backend hands out the generation id before it can know the preview is
   * viable (building the matcher reads the source's ignore-file cascade off
   * disk), so a setup failure arrives here rather than as a rejected `start`.
   * It is terminal for the generation: stop the spinner and show the code. The
   * tree is left alone - the view renders the error in its place, and if the
   * user fixes the rule the next generation swaps a real tree back in. */
  function ingestError(error: ExclusionPreviewError): void {
    if (currentId === null) {
      preIdErrors.push(error);
      return;
    }
    if (error.previewId !== currentId) return;
    errorCode.value = error.code;
    scanning.value = false;
    recomputing.value = false;
  }

  /** Replay the events that arrived before the generation id was known, keeping
   *  only the ones that belong to it. */
  function drainPreId(): void {
    const batches = preIdBatches;
    const dones = preIdDone;
    const errors = preIdErrors;
    preIdBatches = [];
    preIdDone = [];
    preIdErrors = [];
    for (const batch of batches) ingestBatch(batch);
    for (const done of dones) ingestDone(done);
    for (const error of errors) ingestError(error);
  }

  /**
   * Start a fresh preview for `req`, replacing whatever is on screen ONLY once
   * the replacement exists (see `beginGeneration` / `swapPending`).
   *
   * The backend supersedes the pass this replaces, so re-previewing on every
   * rule edit cannot stack concurrent full-tree walks.
   */
  async function start(req: ExclusionPreviewRequest): Promise<void> {
    const seq = ++startSeq;
    currentId = null;
    beginGeneration();
    scanning.value = true;
    errorCode.value = null;
    let id: string;
    try {
      id = await ipc.previewExclusionsStart(req);
    } catch (e) {
      // Only the LATEST start may report - an older call's rejection must not
      // wipe the error state (or the scanning flag) of the one that replaced it.
      if (seq === startSeq) {
        errorCode.value = toErrorCode(e);
        scanning.value = false;
        recomputing.value = false;
      }
      return;
    }
    if (seq !== startSeq) {
      // A newer start overtook this one: abandon this generation rather than
      // adopting it, and stop its walk so it is not left burning CPU.
      void ipc.previewExclusionsCancel(id).catch(() => undefined);
      return;
    }
    currentId = id;
    drainPreId();
  }

  /** Stop the live walk (the editor closed / navigated away) and drop its tree
   *  state. Safe to call when nothing is running. */
  async function cancel(): Promise<void> {
    startSeq += 1;
    const id = currentId;
    currentId = null;
    scanning.value = false;
    recomputing.value = false;
    if (id === null) return;
    await ipc.previewExclusionsCancel(id).catch(() => undefined);
  }

  /** Subscribe to the streaming events. Returns the teardown to call on unmount
   *  (which also cancels any walk still running). */
  async function subscribe(): Promise<() => void> {
    const unlisteners = await Promise.all([
      onExclusionPreviewBatch(ingestBatch),
      onExclusionPreviewDone(ingestDone),
      onExclusionPreviewError(ingestError),
    ]);
    return () => {
      for (const un of unlisteners) un();
      flushScheduled = false;
      void cancel();
    };
  }

  return {
    // reactive state
    treeVersion,
    scanning,
    recomputing,
    complete,
    includedCount,
    excludedCount,
    includedBytes,
    truncated,
    errorCode,
    roots,
    // lifecycle
    start,
    cancel,
    subscribe,
    // exposed for deterministic tests (and a teardown drain)
    flush,
    /** The live generation id, or null. Test/diagnostic seam. */
    currentPreviewId: () => currentId,
    /** Look up a node of the tree ON SCREEN by path - which during a recompute
     *  is still the previous generation's. Test/diagnostic seam. */
    nodeAt: (path: string) => displayedIndex.get(path),
  };
}
