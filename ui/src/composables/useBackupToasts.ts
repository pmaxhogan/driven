import { onBeforeUnmount, onMounted } from "vue";
import { useI18n } from "vue-i18n";
import type { UnlistenFn } from "@tauri-apps/api/event";

import { onActivityNew, onSyncStatusChanged, type SyncStatusChangedPayload } from "../ipc/events";
import type { ActivityEntry, GlobalSyncStatus, OrchestratorState } from "../ipc/types";
import { useToastsStore } from "../stores/toasts";

/**
 * Turns the backend's live backup events into toasts. Called once from
 * `ToastHost.vue`, which is mounted at the app root and never unmounted, so the
 * subscriptions are app-lifetime exactly like the ones App.vue owns for the
 * progress / pause / updater stores.
 *
 * The hard part is NOT plumbing the events - it is deciding which ones deserve
 * to interrupt the user. Driven runs unattended and re-scans on a watcher /
 * interval cadence, so a naive "toast every orchestrator cycle" would fire
 * every few minutes forever. Both signals here are therefore gated on the run
 * having actually done something:
 *
 *  - **"Backup started"** fires when an account enters `executing`, but ONLY if
 *    the `planning` state that immediately preceded it produced at least one op
 *    (uploads + trashes). The orchestrator enters `executing` even for an empty
 *    plan, so the plan totals are the only pre-execution signal that this cycle
 *    has real work; without the gate, every idle poll would toast.
 *  - **"Backup complete"** rides the `backup_done` activity row, which the
 *    orchestrator already writes under exactly the policy we want
 *    (driven-core/src/orchestrator.rs): a user-initiated "Run now" ALWAYS logs
 *    it, a scheduled cycle only when it executed ops, and failed ops suppress
 *    it. So the "did this cycle change anything / was it user-initiated?"
 *    question is answered server-side, and the row carries the file count.
 *
 * On top of that both are debounced by `RUN_TOAST_DEBOUNCE_MS`, which collapses
 * a multi-account or multi-source run (one `backup_done` row per source) into a
 * single toast rather than one per source.
 */

/** Minimum gap between two "started" toasts, and between two "complete" toasts.
 * A run over several sources / accounts closes each one separately; the user
 * cares that "a backup ran", not that it ran six times. */
export const RUN_TOAST_DEBOUNCE_MS = 10_000;

/** Read the snake_case `state` discriminant of an OrchestratorState (same shape
 * the progress store reads). */
function stateTag(state: OrchestratorState): string {
  const tag = state["state"];
  return typeof tag === "string" ? tag : "";
}

/** Ops the `planning` state's plan produced (uploads + trashes), or 0 for any
 * other state / a plan we cannot read. */
function plannedOps(state: OrchestratorState): number {
  const plan = state["plan"];
  if (plan === null || typeof plan !== "object") return 0;
  const p = plan as Record<string, unknown>;
  const uploads = typeof p["uploads"] === "number" ? p["uploads"] : 0;
  const trashes = typeof p["trashes"] === "number" ? p["trashes"] : 0;
  return uploads + trashes;
}

/** Normalize either `sync:status_changed` payload shape into a per-account list.
 * The backend currently emits a single-account snapshot; the aggregate
 * `GlobalSyncStatus` is reserved (see ipc/events.ts). */
function accountStates(
  payload: SyncStatusChangedPayload
): Array<{ account_id: string; state: OrchestratorState }> {
  const aggregate = payload as GlobalSyncStatus;
  if (Array.isArray(aggregate.accounts)) {
    return aggregate.accounts.map((a) => ({ account_id: a.account_id, state: a.state }));
  }
  const single = payload as { account_id: string; state: OrchestratorState };
  return [{ account_id: single.account_id, state: single.state }];
}

export function useBackupToasts(): void {
  const { t } = useI18n();
  const toasts = useToastsStore();

  // Ops the last `planning` state produced, per account - the "is there real
  // work?" gate the following `executing` transition consults.
  const plannedByAccount = new Map<string, number>();
  // Accounts currently in `executing`, so a re-emitted `executing` state (or a
  // progress-driven re-render of the same state) toasts only on the EDGE.
  const executingAccounts = new Set<string>();

  let lastStartedAt = 0;
  let lastCompleteAt = 0;

  function toastStarted(): void {
    const now = Date.now();
    if (now - lastStartedAt < RUN_TOAST_DEBOUNCE_MS) return;
    lastStartedAt = now;
    toasts.push({ kind: "info", message: t("toast.backup.started") });
  }

  function toastComplete(fileCount: number): void {
    const now = Date.now();
    if (now - lastCompleteAt < RUN_TOAST_DEBOUNCE_MS) return;
    lastCompleteAt = now;
    const message =
      fileCount > 0
        ? t("toast.backup.completeFiles", { count: fileCount }, fileCount)
        : t("toast.backup.complete");
    toasts.push({ kind: "success", message });
  }

  function ingestStatus(payload: SyncStatusChangedPayload): void {
    const entries = accountStates(payload);
    // An aggregate payload re-states EVERY account, so any account it omits is
    // no longer executing - drop the stale edge state rather than leaving an
    // account latched and missing its next start.
    if (Array.isArray((payload as GlobalSyncStatus).accounts)) {
      const present = new Set(entries.map((e) => e.account_id));
      for (const id of [...executingAccounts]) {
        if (!present.has(id)) executingAccounts.delete(id);
      }
    }
    for (const { account_id: accountId, state } of entries) {
      const tag = stateTag(state);
      if (tag === "planning") {
        plannedByAccount.set(accountId, plannedOps(state));
        continue;
      }
      if (tag !== "executing") {
        executingAccounts.delete(accountId);
        continue;
      }
      if (executingAccounts.has(accountId)) continue;
      executingAccounts.add(accountId);
      if ((plannedByAccount.get(accountId) ?? 0) > 0) toastStarted();
    }
  }

  function ingestActivity(entry: ActivityEntry): void {
    if (entry.eventType !== "backup_done") return;
    toastComplete(entry.fileCount ?? 0);
  }

  let unlisteners: UnlistenFn[] = [];
  let disposed = false;

  onMounted(async () => {
    const registered: UnlistenFn[] = [];
    try {
      registered.push(await onSyncStatusChanged(ingestStatus));
      registered.push(await onActivityNew(ingestActivity));
    } catch (e) {
      // A toast is a nicety, never a reason to break boot: tear down whatever
      // did register so a half-wired subscription cannot leak, and log.
      for (const un of registered) un();
      console.error("backup toast subscribe failed", e);
      return;
    }
    // Unmount may have raced ahead while we awaited `listen`; honor it rather
    // than leaving listeners registered against a dead component.
    if (disposed) {
      for (const un of registered) un();
      return;
    }
    unlisteners = registered;
  });

  onBeforeUnmount(() => {
    disposed = true;
    for (const un of unlisteners) un();
    unlisteners = [];
  });
}
