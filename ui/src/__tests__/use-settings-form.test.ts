// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach } from "vitest";
import { defineComponent, h } from "vue";
import { mount } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

// useSettingsForm tests: the composable was extracted verbatim from
// Settings.vue (RANGES, the clamp helpers, commitPatch, and the HH:MM<->minute
// pair), so this is the proof the move preserved behaviour before Settings.vue
// starts importing it. `@tauri-apps/api/core`'s `invoke` is the seam - same
// mocking approach as settings-stores.test.ts's store-mock harness - so
// commitPatch's calls into the real settings store are observable without a
// backend.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => undefined),
}));

import { i18n } from "../i18n";
import { useToastsStore } from "../stores/toasts";
import {
  useSettingsForm,
  clampToRange,
  minutesToHHMM,
  hhmmToMinutes,
  RANGES,
  type UseSettingsForm,
} from "../composables/useSettingsForm";

// `useSettingsForm` calls `useI18n()` / `useSettingsStore()` / `useToastsStore()`,
// all of which need an active component instance + Pinia - mount a bare host
// component (mirrors how ToastHost's tests exercise useBackupToasts) rather than
// calling the composable directly.
function mountForm() {
  let form!: UseSettingsForm;
  const Host = defineComponent({
    setup() {
      form = useSettingsForm();
      return () => h("div");
    },
  });
  const pinia = createPinia();
  setActivePinia(pinia);
  mount(Host, { global: { plugins: [pinia, i18n] } });
  return { form, toasts: useToastsStore() };
}

beforeEach(() => {
  invokeMock.mockReset();
});

describe("useSettingsForm", () => {
  describe("commitPatch", () => {
    it("patches the settings store and pushes a success toast", async () => {
      invokeMock.mockResolvedValueOnce({}); // update_settings response
      const { form, toasts } = mountForm();

      await form.commitPatch({ global: { skipOnBattery: false } });

      expect(invokeMock).toHaveBeenCalledWith("update_settings", {
        patch: { global: { skipOnBattery: false } },
      });
      expect(toasts.toasts).toHaveLength(1);
      expect(toasts.toasts[0]).toMatchObject({ kind: "success", message: "Settings saved" });
    });

    it("swallows a rejecting patch - no throw, no success toast", async () => {
      invokeMock.mockRejectedValueOnce(new Error("boom"));
      const { form, toasts } = mountForm();

      await expect(form.commitPatch({ global: { skipOnBattery: false } })).resolves.toBeUndefined();
      expect(toasts.toasts).toHaveLength(0);
    });
  });

  describe("clampToRange", () => {
    it("clamps below, above, and passes through in-range against RANGES.scanIntervalSecs", () => {
      const [min, max] = RANGES.scanIntervalSecs;
      expect(clampToRange(min - 100, RANGES.scanIntervalSecs)).toBe(min);
      expect(clampToRange(max + 100, RANGES.scanIntervalSecs)).toBe(max);
      expect(clampToRange(1_000, RANGES.scanIntervalSecs)).toBe(1_000);
    });
  });

  describe("minutesToHHMM / hhmmToMinutes", () => {
    it("minutesToHHMM(75) formats as 01:15", () => {
      expect(minutesToHHMM(75)).toBe("01:15");
    });

    it("hhmmToMinutes parses a valid time and rejects junk", () => {
      expect(hhmmToMinutes("7:05")).toBe(425);
      expect(hhmmToMinutes("junk")).toBeNull();
    });
  });
});
