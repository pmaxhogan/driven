import { defineStore } from "pinia";
import { ref } from "vue";

import * as ipc from "../ipc/commands";
import { toErrorCode } from "../ipc/errors";
import type { SettingsDto, SettingsPatch } from "../ipc/types";

// Settings store (SPEC s11.6, s22; DESIGN s8.2 Rules + About tabs). Holds the
// settings snapshot + loading/error flags. `refresh` loads via get_settings;
// `patch` round-trips a partial update through update_settings and replaces the
// snapshot with the authoritative result the backend returns (so derived /
// clamped values - e.g. an out-of-range concurrent-uploads override - reflect
// what was actually stored).
//
// Errors are stored as the stable SPEC s24 CODE (via toErrorCode), never
// `String(e)`: a Tauri structured `{ code, message }` error stringifies to the
// literal "[object Object]", which a previous version rendered straight into the
// Rules tab - so a rejected value showed "[object Object]" AND (via the template's
// v-else-if chain) hid the entire form until an app restart. The view localizes
// the code via t(`errors.${code}.long`) and keeps the form visible.
export const useSettingsStore = defineStore("settings", () => {
  const settings = ref<SettingsDto | null>(null);
  const loading = ref(false);
  const errorCode = ref<string | null>(null);

  async function refresh(): Promise<void> {
    loading.value = true;
    errorCode.value = null;
    try {
      settings.value = await ipc.getSettings();
    } catch (e) {
      errorCode.value = toErrorCode(e);
    } finally {
      loading.value = false;
    }
  }

  async function patch(p: SettingsPatch): Promise<void> {
    errorCode.value = null;
    try {
      settings.value = await ipc.updateSettings(p);
    } catch (e) {
      errorCode.value = toErrorCode(e);
      throw e;
    }
  }

  // SPEC s16 (M9b R2-P1-1): toggle anonymous telemetry through the DEDICATED
  // set_telemetry_enabled command (NOT the generic update_settings patch), so the
  // backend flips the in-flight ping cancel flag IMMEDIATELY - a disable click
  // while a ping is building still aborts that send. After the toggle commits we
  // refresh the snapshot so the stored value is reflected authoritatively. (The
  // backend also routes update_settings' telemetry branch through the same
  // cancel-preserving path, so either route is safe; this is the explicit one.)
  async function setTelemetryEnabled(enabled: boolean): Promise<void> {
    errorCode.value = null;
    try {
      await ipc.setTelemetryEnabled(enabled);
      if (settings.value) {
        settings.value = { ...settings.value, telemetry: { ...settings.value.telemetry, enabled } };
      } else {
        await refresh();
      }
    } catch (e) {
      errorCode.value = toErrorCode(e);
      throw e;
    }
  }

  return { settings, loading, errorCode, refresh, patch, setTelemetryEnabled };
});
