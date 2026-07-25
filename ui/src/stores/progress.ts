import { defineStore } from "pinia";
import { computed, ref } from "vue";
import type { UnlistenFn } from "@tauri-apps/api/event";

import * as ipc from "../ipc/commands";
import {
  onSyncSourceProgress,
  onSyncStatusChanged,
  type SourceProgressPayload,
  type SyncStatusChangedPayload,
} from "../ipc/events";
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
 *
 * The `executing` STATE is only ever emitted once per source, carrying a ZEROED
 * `ExecProgress` - the moving counters arrive on the separate
 * `sync:source_progress` channel. So the store subscribes to both and folds the
 * latest per-account tick over the state's embedded snapshot; without the tick
 * the "determinate" branch never has a non-zero total and the bar stays an
 * indeterminate sweep for the whole upload.
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

/** The phase the bar reports, in the order it wins when several accounts are
 * working at once. Executing is the most informative (it has a real percent), so
 * it outranks the pre-flight phases; `power_check` is the least. Mirrors the
 * WORKING_STATES set - anything not listed here is not an active run. */
const PHASE_PRECEDENCE = ["executing", "verifying", "planning", "scanning", "power_check"] as const;

/** One of PHASE_PRECEDENCE, or null when no run is active. */
export type SyncPhase = (typeof PHASE_PRECEDENCE)[number] | null;

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

/** Coerce an untyped wire `progress` object into an ExecProgress, or null when it
 * is not an object at all. Shared by the `executing` state's embedded snapshot
 * and the `sync:source_progress` ticks, so both go through one reader. */
function readExecProgress(p: unknown): ExecProgress | null {
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

/** Extract the ExecProgress carried by an `executing` state, or null otherwise. */
function execProgressOf(state: OrchestratorState): ExecProgress | null {
  if (stateTag(state) !== "executing") return null;
  return readExecProgress(state["progress"]);
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

  // Latest `sync:source_progress` tick per account. The orchestrator runs one
  // source at a time per account and re-enters `executing` with a zeroed
  // snapshot for each, so ONE latest tick per account is the whole picture -
  // and a state change for that account always drops its tick (below), which is
  // what stops a finished run's last tick from leaking into the next one.
  const ticks = ref<Record<string, ExecProgress>>({});

  function isGlobal(payload: SyncStatusChangedPayload): payload is GlobalSyncStatus {
    return Array.isArray((payload as GlobalSyncStatus).accounts);
  }

  /** Fold one status payload into the per-account map (handles both shapes).
   *
   * Any state change for an account DROPS that account's pending progress tick,
   * including a fresh `executing` transition (which carries `ExecProgress::zero()`
   * and marks the head of a NEW source's execution). Without that, the previous
   * run's final tick - typically 100% - would be folded into the new run and the
   * bar would open full before falling back. */
  function ingest(payload: SyncStatusChangedPayload): void {
    if (isGlobal(payload)) {
      const next: Record<string, OrchestratorState> = {};
      for (const a of payload.accounts) next[a.account_id] = a.state;
      states.value = next;
      // An aggregate snapshot re-states EVERY account, so every tick is stale.
      ticks.value = {};
    } else {
      states.value = { ...states.value, [payload.account_id]: payload.state };
      if (payload.account_id in ticks.value) {
        const next = { ...ticks.value };
        delete next[payload.account_id];
        ticks.value = next;
      }
    }
  }

  /** Fold one `sync:source_progress` tick into the per-account map.
   *
   * Gated on the account being CURRENTLY `executing`: a tick that arrives after
   * the run left that state (the executor's final snapshot can trail the
   * transition, and an account we have never seen a status for is not running at
   * all) must not resurrect the bar or contribute to the percent. The `exec`
   * aggregate re-checks the same condition at read time, so even a tick stored
   * and then orphaned by an out-of-order status event is ignored. */
  function ingestProgress(payload: SourceProgressPayload): void {
    const state = states.value[payload.account_id];
    if (!state || stateTag(state) !== "executing") return;
    const progress = readExecProgress(payload.progress);
    if (!progress) return;
    ticks.value = { ...ticks.value, [payload.account_id]: progress };
  }

  /** True while ANY account's orchestrator is in a working state - i.e. a
   * backup/sync run is in progress. Drives the bar's visibility. */
  const active = computed<boolean>(() =>
    Object.values(states.value).some((s) => WORKING_STATES.has(stateTag(s)))
  );

  /** The phase to report while a run is active (highest-precedence working
   * state across accounts), or null when idle. Drives the bar's phase label, so
   * the scan/plan phases are no longer indistinguishable from a stalled app. */
  const phase = computed<SyncPhase>(() => {
    const tags = new Set(Object.values(states.value).map(stateTag));
    return PHASE_PRECEDENCE.find((p) => tags.has(p)) ?? null;
  });

  /** Sum a numeric field over every account whose state carries the given tag. */
  function sumOver(tag: string, key: string): number {
    let total = 0;
    for (const s of Object.values(states.value)) {
      if (stateTag(s) !== tag) continue;
      total += numField(s as Record<string, unknown>, key);
    }
    return total;
  }

  /** Files the scanner has visited so far, summed across scanning accounts. The
   * backend streams this on every throttled scan tick, so it climbs live during
   * the walk instead of sitting at the 0 the single pre-scan transition used to
   * leave it at. */
  const scanned = computed<number>(() => sumOver("scanning", "scanned"));

  /** Upload ops the planner produced, summed across accounts in `planning`. */
  const plannedFiles = computed<number>(() => {
    let total = 0;
    for (const s of Object.values(states.value)) {
      if (stateTag(s) !== "planning") continue;
      const plan = s["plan"];
      if (plan === null || typeof plan !== "object") continue;
      const p = plan as Record<string, unknown>;
      total += numField(p, "uploads") + numField(p, "trashes");
    }
    return total;
  });

  /** Files sampled so far, summed across accounts in `verifying`. */
  const verified = computed<number>(() => sumOver("verifying", "sampled"));

  /** Aggregate execution progress across every account currently `executing`.
   * Scan/plan/verify carry no reliable total, so they contribute nothing here.
   *
   * Per account the FRESHEST reading wins: the latest `sync:source_progress`
   * tick when there is one, else the snapshot embedded in the `executing` state
   * (which is always the zeroed one the transition carried). Iterating `states`
   * rather than `ticks` is what keeps a stale tick out of the sum - an account
   * that is no longer `executing` contributes nothing regardless of what tick it
   * still holds. Multi-account runs still SUM, so two accounts uploading at once
   * produce one combined percent. */
  const exec = computed(() => {
    let filesDone = 0;
    let filesTotal = 0;
    let bytesDone = 0;
    let bytesTotal = 0;
    let trashesDone = 0;
    let trashesTotal = 0;
    for (const [accountId, s] of Object.entries(states.value)) {
      const p = stateTag(s) === "executing" ? (ticks.value[accountId] ?? execProgressOf(s)) : null;
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
  let unlisteners: UnlistenFn[] = [];
  let desiredSubscribed = false;

  /** Subscribe to `sync:status_changed` AND `sync:source_progress` (idempotent).
   * Both are needed for a determinate bar: the first says WHICH phase each
   * account is in, the second carries the moving counters. Registered together
   * so App.vue keeps its single call. A partial failure tears down whatever did
   * register and resets, so a later retry starts clean rather than leaking a
   * half-wired subscription. */
  async function subscribe(): Promise<void> {
    if (desiredSubscribed) return;
    desiredSubscribed = true;
    const registered: UnlistenFn[] = [];
    try {
      registered.push(await onSyncStatusChanged((payload) => ingest(payload)));
      registered.push(await onSyncSourceProgress((payload) => ingestProgress(payload)));
      // unsubscribe() may have raced ahead while we awaited; honor it.
      if (!desiredSubscribed) {
        for (const un of registered) un();
        return;
      }
      unlisteners = registered;
    } catch (e) {
      for (const un of registered) un();
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

  /** Stop every subscription. */
  function unsubscribe(): void {
    desiredSubscribed = false;
    for (const un of unlisteners) un();
    unlisteners = [];
  }

  return {
    states,
    ticks,
    active,
    phase,
    scanned,
    plannedFiles,
    verified,
    percent,
    filesDone,
    filesTotal,
    exec,
    ingest,
    ingestProgress,
    subscribe,
    hydrate,
    unsubscribe,
  };
});
