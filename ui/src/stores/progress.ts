import { defineStore } from "pinia";
import { computed, ref } from "vue";
import type { UnlistenFn } from "@tauri-apps/api/event";

import * as ipc from "../ipc/commands";
import { onSyncStatusChanged, type SyncStatusChangedPayload } from "../ipc/events";
import type { ExecProgress, GlobalSyncStatus, OrchestratorState } from "../ipc/types";

/**
 * Global backup-progress store (issue #46). Owns the `sync:status_changed`
 * subscription (registered once at the app root in App.vue, mirroring the
 * updater store) and derives whether a backup/sync run is in progress plus a
 * determinate completion percent. The thin top-of-app progress bar
 * (`GlobalProgressBar.vue`) is a pure render of this state, so the active/percent
 * logic here is unit-testable without a backend.
 *
 * "A backup is running" = ANY account's orchestrator is in a WORKING state. The
 * determinate percent comes from the `executing` phase's byte/file totals; the
 * scan/plan/verify phases carry no reliable total, so the bar runs indeterminate
 * for them.
 */

/** OrchestratorState discriminants (SPEC s5) that mean a backup/sync run is
 * actively working - the same group the tray renders as "Syncing"
 * (src-tauri/src/tray.rs `for_state` / `state_severity`). `idle`, `paused`,
 * `backoff` (a Drive-unreachable attention state, NOT work) and `error` are not
 * an active run. The discriminant is the snake_case `state` tag the Rust enum
 * serializes (`#[serde(rename_all = "snake_case", tag = "state")]`). */
const WORKING_STATES: ReadonlySet<string> = new Set([
  "power_check",
  "scanning",
  "planning",
  "executing",
  "verifying",
]);

/** Read the snake_case `state` discriminant of an OrchestratorState. */
function stateTag(state: OrchestratorState): string {
  const tag = state["state"];
  return typeof tag === "string" ? tag : "";
}

/** Read a finite numeric field from an untyped wire object, defaulting to 0. */
function numField(obj: Record<string, unknown>, key: string): number {
  const v = obj[key];
  return typeof v === "number" && Number.isFinite(v) ? v : 0;
}

/** Extract the ExecProgress carried by an `executing` state, or null otherwise. */
function execProgressOf(state: OrchestratorState): ExecProgress | null {
  if (stateTag(state) !== "executing") return null;
  const p = state["progress"];
  if (p === null || typeof p !== "object") return null;
  const o = p as Record<string, unknown>;
  return {
    files_done: numField(o, "files_done"),
    files_total: numField(o, "files_total"),
    bytes_done: numField(o, "bytes_done"),
    bytes_total: numField(o, "bytes_total"),
    trashes_done: numField(o, "trashes_done"),
    trashes_total: numField(o, "trashes_total"),
    errors: numField(o, "errors"),
  };
}

/** Clamp a fraction into [0, 1] (and map NaN to 0). */
function clamp01(n: number): number {
  if (Number.isNaN(n)) return 0;
  return Math.min(1, Math.max(0, n));
}

export const useProgressStore = defineStore("progress", () => {
  // Per-account orchestrator state, keyed by account id. The live
  // `sync:status_changed` event currently carries a SINGLE-account snapshot
  // (assembly.rs `AccountSyncStatusEvent`), so a per-account payload MERGES into
  // the map; a future aggregate `GlobalSyncStatus` payload REPLACES it wholesale.
  // `hydrate()` (via get_sync_status) always supplies the aggregate.
  const states = ref<Record<string, OrchestratorState>>({});

  function isGlobal(payload: SyncStatusChangedPayload): payload is GlobalSyncStatus {
    return Array.isArray((payload as GlobalSyncStatus).accounts);
  }

  /** Fold one status payload into the per-account map (handles both shapes). */
  function ingest(payload: SyncStatusChangedPayload): void {
    if (isGlobal(payload)) {
      const next: Record<string, OrchestratorState> = {};
      for (const a of payload.accounts) next[a.account_id] = a.state;
      states.value = next;
    } else {
      states.value = { ...states.value, [payload.account_id]: payload.state };
    }
  }

  /** True while ANY account's orchestrator is in a working state - i.e. a
   * backup/sync run is in progress. Drives the bar's visibility. */
  const active = computed<boolean>(() =>
    Object.values(states.value).some((s) => WORKING_STATES.has(stateTag(s)))
  );

  /** Aggregate execution progress across every account currently `executing`.
   * Scan/plan/verify carry no reliable total, so they contribute nothing here. */
  const exec = computed(() => {
    let filesDone = 0;
    let filesTotal = 0;
    let bytesDone = 0;
    let bytesTotal = 0;
    let trashesDone = 0;
    let trashesTotal = 0;
    for (const s of Object.values(states.value)) {
      const p = execProgressOf(s);
      if (!p) continue;
      filesDone += p.files_done;
      filesTotal += p.files_total;
      bytesDone += p.bytes_done;
      bytesTotal += p.bytes_total;
      trashesDone += p.trashes_done;
      trashesTotal += p.trashes_total;
    }
    return { filesDone, filesTotal, bytesDone, bytesTotal, trashesDone, trashesTotal };
  });

  /** Determinate completion fraction (0..1) when a real total is known, or null
   * when the run is active but has no measurable total yet (-> indeterminate
   * bar). Prefers bytes (smoothest), falling back to op counts (uploads +
   * trashes) for delete-only plans that move no bytes. Null when no run is
   * active. */
  const percent = computed<number | null>(() => {
    if (!active.value) return null;
    const e = exec.value;
    // Bytes are the smoothest signal, but ONLY for an upload-only plan. In a
    // MIXED upload+delete plan, deletes move no bytes, so a pure byte fraction
    // would hit 100% the instant uploads finish while trash ops are still
    // pending (codex P2). When the plan has trash ops, fall through to op counts
    // (uploads + trashes) so the bar cannot read 100% until BOTH are done.
    if (e.bytesTotal > 0 && e.trashesTotal === 0) {
      return clamp01(e.bytesDone / e.bytesTotal);
    }
    const opsTotal = e.filesTotal + e.trashesTotal;
    if (opsTotal > 0) return clamp01((e.filesDone + e.trashesDone) / opsTotal);
    // Active with measurable bytes but no op counts (rare): use the byte fraction.
    if (e.bytesTotal > 0) return clamp01(e.bytesDone / e.bytesTotal);
    return null;
  });

  /** Files uploaded so far across executing accounts (for the bar's a11y label). */
  const filesDone = computed<number>(() => exec.value.filesDone);
  /** Total upload ops across executing accounts (0 when nothing is executing). */
  const filesTotal = computed<number>(() => exec.value.filesTotal);

  // --- event subscription (App.vue owns the app-lifetime registration) ------
  let unlisten: UnlistenFn | null = null;
  let desiredSubscribed = false;

  /** Subscribe to `sync:status_changed` (idempotent). */
  async function subscribe(): Promise<void> {
    if (desiredSubscribed) return;
    desiredSubscribed = true;
    try {
      const un = await onSyncStatusChanged((payload) => ingest(payload));
      // unsubscribe() may have raced ahead while we awaited; honor it.
      if (!desiredSubscribed) {
        un();
        return;
      }
      unlisten = un;
    } catch (e) {
      // Reset so a later retry can re-subscribe; re-throw so the caller can log.
      desiredSubscribed = false;
      throw e;
    }
  }

  /** Seed the per-account map from the backend's CURRENT aggregate status so a
   * run already underway when the webview attaches shows immediately (the
   * one-shot live event may have fired before our listener registered).
   * Best-effort: a failure just leaves the live stream to fill the map. */
  async function hydrate(): Promise<void> {
    try {
      ingest(await ipc.getSyncStatus());
    } catch (e) {
      console.error("progress hydrate failed at app boot", e);
    }
  }

  /** Stop the subscription. */
  function unsubscribe(): void {
    desiredSubscribed = false;
    if (unlisten) {
      unlisten();
      unlisten = null;
    }
  }

  return {
    states,
    active,
    percent,
    filesDone,
    filesTotal,
    exec,
    ingest,
    subscribe,
    hydrate,
    unsubscribe,
  };
});
