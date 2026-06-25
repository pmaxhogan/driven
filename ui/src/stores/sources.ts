import { defineStore } from "pinia";
import { ref } from "vue";

import * as ipc from "../ipc/commands";
import type { AddSourceRequest, AddSourceResult, SourceDto, SourcePatch } from "../ipc/types";

// Sources store (SPEC s11.2; DESIGN s8.2 Sources tab). Holds the source list +
// loading/error flags and the full CRUD over the typed IPC wrappers. The
// add-source wizard (folder pickers + exclusion preview) drives `add`; the
// SourceTable rows drive `update` (enabled toggle), `remove`, and `syncNow`.
export const useSourcesStore = defineStore("sources", () => {
  const sources = ref<SourceDto[]>([]);
  const loading = ref(false);
  const error = ref<string | null>(null);

  async function refresh(): Promise<void> {
    loading.value = true;
    error.value = null;
    try {
      sources.value = await ipc.listSources();
    } catch (e) {
      error.value = String(e);
    } finally {
      loading.value = false;
    }
  }

  async function add(req: AddSourceRequest): Promise<AddSourceResult> {
    const result = await ipc.addSource(req);
    await refresh();
    return result;
  }

  async function update(sourceId: string, patch: SourcePatch): Promise<SourceDto> {
    const updated = await ipc.updateSource(sourceId, patch);
    await refresh();
    return updated;
  }

  async function remove(sourceId: string, deleteRemote: boolean): Promise<void> {
    await ipc.removeSource(sourceId, deleteRemote);
    await refresh();
  }

  /** Trigger a one-shot sync of a single source (SPEC s11.3 sync_now). */
  async function syncNow(sourceId: string): Promise<void> {
    await ipc.syncNow(sourceId);
  }

  /** M9c D4 (M6 R4-P1-1, DATA-SAFETY): backend-reveal the recovery phrase for a
   * source awaiting an ack. Records the reveal so the ack can proceed; returns the
   * 24 words for one-time display. */
  async function revealRecoveryPhrase(sourceId: string): Promise<string[]> {
    return ipc.revealRecoveryPhrase(sourceId);
  }

  /** M9c D4: acknowledge the recovery phrase was saved, ENABLING the first
   * encrypted source so backups can begin. Rejected by the backend unless a real
   * reveal was recorded first. Refreshes the list so the now-enabled source shows. */
  async function ackRecoveryPhrase(sourceId: string): Promise<SourceDto> {
    const updated = await ipc.ackRecoveryPhraseSaved(sourceId);
    await refresh();
    return updated;
  }

  return {
    sources,
    loading,
    error,
    refresh,
    add,
    update,
    remove,
    syncNow,
    revealRecoveryPhrase,
    ackRecoveryPhrase,
  };
});
