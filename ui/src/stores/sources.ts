import { defineStore } from "pinia";
import { ref } from "vue";

import * as ipc from "../ipc/commands";
import type { AddSourceRequest, SourceDto, SourcePatch } from "../ipc/types";

// Sources store (SPEC s11.2; DESIGN s8.2 Sources tab). Holds the source list +
// loading/error flags. M6 scaffold: action SIGNATURES frozen; the sources
// implementer enriches the add-source wizard flow (folder picker + exclusion
// preview live in the AddSourceWizard component via the IPC wrappers directly).
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

  async function add(req: AddSourceRequest): Promise<SourceDto> {
    const created = await ipc.addSource(req);
    await refresh();
    return created;
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

  async function syncNow(sourceId: string): Promise<void> {
    await ipc.syncNow(sourceId);
  }

  return { sources, loading, error, refresh, add, update, remove, syncNow };
});
