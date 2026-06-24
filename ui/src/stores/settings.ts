import { defineStore } from "pinia";
import { ref } from "vue";

import * as ipc from "../ipc/commands";
import type { SettingsDto, SettingsPatch } from "../ipc/types";

// Settings store (SPEC s11.6, s22; DESIGN s8.2 Rules + About tabs). Holds the
// settings snapshot + loading/error flags. `refresh` loads via get_settings;
// `patch` round-trips a partial update through update_settings and replaces the
// snapshot with the authoritative result the backend returns (so derived /
// clamped values - e.g. an out-of-range concurrent-uploads override - reflect
// what was actually stored).
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
    error.value = null;
    try {
      settings.value = await ipc.updateSettings(p);
    } catch (e) {
      error.value = String(e);
      throw e;
    }
  }

  return { settings, loading, error, refresh, patch };
});
