import { defineStore } from "pinia";
import { ref } from "vue";

import * as ipc from "../ipc/commands";
import type { SettingsDto, SettingsPatch } from "../ipc/types";

// Settings store (SPEC s11.6, s22; DESIGN s8.2 Rules + About tabs). Holds the
// settings snapshot + loading/error flags. M6 scaffold: action SIGNATURES
// frozen; the settings implementer enriches per-field patch helpers as needed.
export const useSettingsStore = defineStore("settings", () => {
  const settings = ref<SettingsDto | null>(null);
  const loading = ref(false);
  const error = ref<string | null>(null);

  async function refresh(): Promise<void> {
    loading.value = true;
    error.value = null;
    try {
      settings.value = await ipc.getSettings();
    } catch (e) {
      error.value = String(e);
    } finally {
      loading.value = false;
    }
  }

  async function patch(p: SettingsPatch): Promise<void> {
    settings.value = await ipc.updateSettings(p);
  }

  return { settings, loading, error, refresh, patch };
});
