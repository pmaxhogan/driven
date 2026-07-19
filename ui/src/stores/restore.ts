import { defineStore } from "pinia";
import { computed, ref } from "vue";
import type { UnlistenFn } from "@tauri-apps/api/event";

import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
import { onRestoreProgress } from "../ipc/events";
import type {
  FileSearchHitDto,
  RemoteEntryDto,
  RestoreItem,
  RestoreJobStatus,
  SourceDto,
} from "../ipc/types";

/** Default per-query search result cap (<= the backend's MAX_SEARCH_LIMIT). */
export const SEARCH_LIMIT = 500;

/**
 * Restore browser store (SPEC s11.5; DESIGN s8.4).
 *
 * Drives the whole restore flow:
 * - source selection + the browsed tree (lazy per-folder via `listRemoteTree`,
 *   reading file_state - never Drive),
 * - filename / glob search (`searchFiles`),
 * - a multi-select of files (by `${sourceId}::${relativePath}` key),
 * - the destination folder (a backend dialog token from `pickFolderDialog`),
 * - the active restore job + live progress from `restore:progress`.
 *
 * Encrypted sources are transparent here: file_state stores the PLAINTEXT path,
 * so the browser shows decrypted names without any extra step.
 *
 * Stale-response guard: tree + search carry a request token; a response is
 * committed only if its token is still current, so a fast folder switch / new
 * search discards an in-flight earlier response.
 *
 * Command errors are normalized to the stable SPEC s24 `{ code }` shape and
 * exposed as `errorCode` for the view to localize via `t(\`errors.${code}.long\`)`.
 */
export const useRestoreStore = defineStore("restore", () => {
  // The sources the user can browse (loaded from `listSources`).
  const sources = ref<SourceDto[]>([]);
  // The currently-browsed source id (null until one is selected).
  const sourceId = ref<string | null>(null);
  // The current folder prefix within the source ("" = source root).
  const prefix = ref("");
  // The immediate children of the current prefix.
  const nodes = ref<RemoteEntryDto[]>([]);
  // M8-P2-1: true when the current folder's listing was CAPPED (more immediate
  // children than the backend's cap), so the view can show a "showing first N"
  // notice instead of implying the folder is complete.
  const treeTruncated = ref(false);
  // The active search query ("" = browsing, not searching).
  const query = ref("");
  // The current search results (only meaningful while `query` is non-empty).
  const searchResults = ref<FileSearchHitDto[]>([]);
  // The multi-selection, keyed by `${sourceId}::${relativePath}` (so a selection
  // survives navigating between folders).
  const selectedKeys = ref<Set<string>>(new Set());
  // The destination folder path (display echo) + its one-shot dialog token.
  const destPath = ref<string | null>(null);
  const destToken = ref<string | null>(null);
  // Issue #36: optional point-in-time restore. `null` = restore the latest
  // backup (default). When set to a Unix-ms instant, each file is restored as it
  // was backed up as of that instant (its current bytes if already in place
  // then, else the retained version whose window covers it).
  const asOf = ref<number | null>(null);
  // The active restore job status (null until a restore starts).
  const job = ref<RestoreJobStatus | null>(null);
  // M8-P2-4: the active restore job id, persisted so a view remount / missed
  // terminal event can reconcile current state via getRestoreJob(jobId) instead
  // of relying solely on the live `restore:progress` stream.
  const activeJobId = ref<string | null>(null);
  const loading = ref(false);
  const restoring = ref(false);
  // M8-P1-1: true while a cancel has been requested but the terminal CANCELLED
  // status has not yet arrived (so the UI can disable the cancel button).
  const cancelling = ref(false);
  // Stable SPEC s24 code (null = no error); the view maps it via t().
  const errorCode = ref<string | null>(null);

  // Request generation for the tree + search stale-response guard.
  let requestToken = 0;

  /** The breadcrumb segments of the current prefix (for the path bar). */
  const breadcrumbs = computed<string[]>(() =>
    prefix.value === "" ? [] : prefix.value.split("/")
  );

  /** Whether the view is showing search results (vs the folder tree). */
  const isSearching = computed(() => query.value.trim().length > 0);

  /** The rendered rows: search hits while searching, else the folder tree. */
  const rows = computed(() => (isSearching.value ? searchResults.value : nodes.value));

  /** True when the current view has nothing to show and is not loading. */
  const isEmpty = computed(
    () => !loading.value && rows.value.length === 0 && errorCode.value === null
  );

  /** The number of files currently selected. */
  const selectedCount = computed(() => selectedKeys.value.size);

  /** Whether a restore can be started (selection + a destination + a source). */
  const canRestore = computed(
    () => selectedKeys.value.size > 0 && destToken.value !== null && !restoring.value
  );

  /** The stable selection key for a (sourceId, relativePath) pair. */
  function keyOf(srcId: string, relativePath: string): string {
    return `${srcId}::${relativePath}`;
  }

  /** Load the list of browsable sources. */
  async function loadSources(): Promise<void> {
    try {
      sources.value = await ipc.listSources();
      // Default to the first source if none is selected yet.
      if (sourceId.value === null && sources.value.length > 0) {
        await selectSource(sources.value[0].id);
      }
    } catch (e) {
      errorCode.value = toErrorCode(e);
    }
  }

  /** Select a source to browse, resetting the tree to its root.
   *
   * R3-P1-1 (defense in depth): clear the multi-selection when the ACTIVE source
   * changes. The selection is keyed by `${sourceId}::${relativePath}` and survives
   * folder navigation by design, but accumulating selections ACROSS sources is
   * what lets two sources' identically-named files (e.g. both `foo.txt`) target
   * the same restore destination. The backend now REJECTS such a job outright
   * (the real guard), but clearing here keeps the UX honest so the user does not
   * unknowingly carry a hidden cross-source selection into a rejected restore. */
  async function selectSource(id: string): Promise<void> {
    if (sourceId.value !== null && sourceId.value !== id) {
      clearSelection();
    }
    sourceId.value = id;
    prefix.value = "";
    query.value = "";
    searchResults.value = [];
    await openFolder("");
  }

  /** Open (browse into) a folder by its full relative-path prefix ("" = root).
   * Lazy: reads only this folder's immediate children from file_state. */
  async function openFolder(nextPrefix: string): Promise<void> {
    if (sourceId.value === null) return;
    query.value = "";
    prefix.value = nextPrefix;
    const token = ++requestToken;
    const src = sourceId.value;
    loading.value = true;
    errorCode.value = null;
    try {
      const result = await ipc.listRemoteTree(src, nextPrefix);
      // Discard a stale response (a newer navigation started).
      if (token !== requestToken) return;
      nodes.value = result.entries;
      treeTruncated.value = result.truncated;
    } catch (e) {
      if (token !== requestToken) return;
      errorCode.value = toErrorCode(e);
    } finally {
      if (token === requestToken) loading.value = false;
    }
  }

  /** Navigate to a breadcrumb index (-1 = root). */
  async function goToBreadcrumb(index: number): Promise<void> {
    const segs = breadcrumbs.value;
    const nextPrefix = index < 0 ? "" : segs.slice(0, index + 1).join("/");
    await openFolder(nextPrefix);
  }

  /** Run a filename / glob search across the current source (or all sources if
   * none is selected). An empty query clears search and returns to the tree. */
  async function runSearch(q: string): Promise<void> {
    query.value = q;
    if (q.trim().length === 0) {
      searchResults.value = [];
      return;
    }
    const token = ++requestToken;
    loading.value = true;
    errorCode.value = null;
    try {
      const result = await ipc.searchFiles(sourceId.value, q.trim(), SEARCH_LIMIT);
      if (token !== requestToken) return;
      searchResults.value = result;
    } catch (e) {
      if (token !== requestToken) return;
      errorCode.value = toErrorCode(e);
    } finally {
      if (token === requestToken) loading.value = false;
    }
  }

  /** Whether a (sourceId, relativePath) file is selected. */
  function isSelected(srcId: string, relativePath: string): boolean {
    return selectedKeys.value.has(keyOf(srcId, relativePath));
  }

  /** Toggle the selection of one restorable file. Folders cannot be selected. */
  function toggleSelect(srcId: string, relativePath: string): void {
    const key = keyOf(srcId, relativePath);
    const next = new Set(selectedKeys.value);
    if (next.has(key)) {
      next.delete(key);
    } else {
      next.add(key);
    }
    selectedKeys.value = next;
  }

  /** Clear the entire selection. */
  function clearSelection(): void {
    selectedKeys.value = new Set();
  }

  /** Record the destination folder chosen via the backend dialog (path + token).
   * Call after `pickFolderDialog`. */
  function setDestination(path: string, token: string): void {
    destPath.value = path;
    destToken.value = token;
  }

  /** Issue #36: set (or clear, with `null`) the point-in-time "as of" instant
   * applied to the next restore. Accepts a Unix-ms number, an ISO date/datetime
   * string (from a native date input), or `null`/empty to restore the latest. */
  function setAsOf(value: number | string | null): void {
    if (value === null || value === "") {
      asOf.value = null;
      return;
    }
    const ms = typeof value === "number" ? value : Date.parse(value);
    asOf.value = Number.isNaN(ms) ? null : ms;
  }

  /** Open the backend folder dialog and record the chosen destination. */
  async function pickDestination(): Promise<void> {
    try {
      const picked = await ipc.pickFolderDialog();
      setDestination(picked.path, picked.token);
    } catch (e) {
      // A cancelled dialog surfaces as a benign io_error; do not advance.
      const code = toErrorCode(e);
      if (code !== "local.io_error") errorCode.value = code;
    }
  }

  /** Build the RestoreItem list from the current selection (the key encodes the
   * (sourceId, relativePath) pair). */
  function selectedItems(): RestoreItem[] {
    const items: RestoreItem[] = [];
    for (const key of selectedKeys.value) {
      const sep = key.indexOf("::");
      if (sep < 0) continue;
      items.push({
        sourceId: key.slice(0, sep),
        relativePath: key.slice(sep + 2),
      });
    }
    return items;
  }

  /** Start a restore of the current selection to the chosen destination. The
   * destination token is one-shot (consumed by the backend), so it is cleared
   * after the call; the live progress arrives on `restore:progress`. */
  async function startRestore(): Promise<void> {
    if (destToken.value === null) {
      errorCode.value = "local.io_error";
      return;
    }
    const items = selectedItems();
    if (items.length === 0) return;
    restoring.value = true;
    cancelling.value = false;
    errorCode.value = null;
    job.value = null;
    // M9c D3 (M8 R4-P2-2): clear the PRIOR job id BEFORE the new IPC. If the new
    // restore is REJECTED (e.g. a collision / bad-input error from the R3 fixes),
    // the store must NOT keep tracking the OLD job id - a later reconcile / cancel
    // would otherwise target stale state. The returned id is assigned only on
    // success below.
    activeJobId.value = null;
    try {
      // M8-P2-4: keep the returned job id so a remount / missed terminal event
      // can reconcile current state via getRestoreJob(jobId).
      // Issue #36: pass the optional point-in-time instant (null = latest).
      const jobId = await ipc.restoreFiles(items, destToken.value, asOf.value);
      activeJobId.value = jobId;
      // The backend consumes the one-shot token; require a fresh pick for a
      // subsequent restore.
      destToken.value = null;
      destPath.value = null;
      // Reconcile immediately so the panel shows the seeded job even if the first
      // live tick was missed (e.g. subscription raced the spawn).
      await reconcileJob();
    } catch (e) {
      errorCode.value = toErrorCode(e);
      restoring.value = false;
    }
  }

  /** Request cancellation of the active restore job (M8-P1-1). The backend stops
   * the job, deletes any in-flight temp (no partial), and emits a terminal
   * CANCELLED status; `cancelling` gates the button until that arrives. */
  async function cancelRestore(): Promise<void> {
    if (activeJobId.value === null) return;
    cancelling.value = true;
    try {
      await ipc.cancelRestoreJob(activeJobId.value);
    } catch (e) {
      // A cancel that races completion is benign; surface other errors.
      cancelling.value = false;
      errorCode.value = toErrorCode(e);
    }
  }

  /** M8-P2-4: reconcile the active job's current state from the backend by id, so
   * a view remount or a missed terminal event does not leave progress stale.
   * Called after start + on (re)subscription. A no-op if no job is active. */
  async function reconcileJob(): Promise<void> {
    if (activeJobId.value === null) return;
    try {
      const status = await ipc.getRestoreJob(activeJobId.value);
      onProgress(status);
    } catch {
      // The job id may have been pruned (terminal long ago); leave state as is.
    }
  }

  /** Ingest a `restore:progress` event (or a reconcile snapshot): store the
   * latest status and, on a terminal state (done / cancelled), clear the
   * restoring + cancelling flags so the UI re-enables the controls. */
  function onProgress(status: RestoreJobStatus): void {
    // Ignore a snapshot for a different (stale) job than the one we track.
    if (activeJobId.value !== null && status.jobId !== activeJobId.value) return;
    job.value = status;
    if (status.done) {
      restoring.value = false;
      cancelling.value = false;
    }
  }

  // --- restore:progress subscription (no listener leak) ---------------------
  let unlistenProgress: UnlistenFn | null = null;
  let desiredSubscribed = false;

  /** Subscribe to the `restore:progress` live stream (idempotent). On
   * (re)subscription it ALSO reconciles the active job by id (M8-P2-4), so a view
   * remount that missed a terminal event recovers the current state. */
  async function subscribeProgress(): Promise<void> {
    if (desiredSubscribed) return;
    desiredSubscribed = true;
    const un = await onRestoreProgress((status) => onProgress(status));
    if (!desiredSubscribed) {
      // Unsubscribed before the listener resolved: tear it down now.
      un();
      return;
    }
    unlistenProgress = un;
    // M8-P2-4: reconcile after (re)subscribing so a remount recovers state.
    await reconcileJob();
  }

  /** Stop the `restore:progress` subscription. */
  function unsubscribeProgress(): void {
    desiredSubscribed = false;
    if (unlistenProgress) {
      unlistenProgress();
      unlistenProgress = null;
    }
  }

  return {
    sources,
    sourceId,
    prefix,
    nodes,
    treeTruncated,
    query,
    searchResults,
    selectedKeys,
    destPath,
    destToken,
    asOf,
    job,
    activeJobId,
    loading,
    restoring,
    cancelling,
    errorCode,
    breadcrumbs,
    isSearching,
    rows,
    isEmpty,
    selectedCount,
    canRestore,
    keyOf,
    loadSources,
    selectSource,
    openFolder,
    goToBreadcrumb,
    runSearch,
    isSelected,
    toggleSelect,
    clearSelection,
    setDestination,
    setAsOf,
    pickDestination,
    selectedItems,
    startRestore,
    cancelRestore,
    reconcileJob,
    onProgress,
    subscribeProgress,
    unsubscribeProgress,
  };
});
