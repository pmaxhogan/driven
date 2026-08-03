import { useI18n } from "vue-i18n";

import { useSettingsStore } from "../stores/settings";
import { useToastsStore } from "../stores/toasts";
import type { SettingsPatch } from "../ipc/types";

// Shared Rules-tab form plumbing (SDD 2026-08-02 settings-sidebar-ia, task 1),
// extracted from Settings.vue so the later per-tab views can all reach the same
// range table + commit path without duplicating it. This is a MOVE, not a
// rewrite: RANGES / clampToRange / parse*Clamped / commitPatch are the same
// logic that lived at Settings.vue:282-346.

// Backend-enforced numeric ranges (mirror of src-tauri/src/commands/settings.rs:
// check_range bounds). We clamp every numeric field to its range BEFORE patching
// so a typed out-of-range value (e.g. 100 concurrent uploads, a 10s scan
// interval) is corrected in place and never round-trips to a backend rejection -
// the rejection used to brick the whole Rules form. The backend still validates;
// this just keeps the UI from ever sending a value it will refuse.
export const RANGES = {
  bandwidthCapMbps: [1, 100_000],
  meteredBandwidthCapMbps: [1, 100_000],
  defaultConcurrentUploads: [1, 32],
  scanIntervalSecs: [30, 604_800],
  deepVerifyIntervalSecs: [3_600, 31_536_000],
  hookTimeoutSecs: [1, 86_400],
  // The scrub cadence is entered in HOURS but stored in seconds, so this range
  // is the backend's SCRUB_INTERVAL_MIN..MAX (1 hour .. 1 year) expressed in
  // hours - keep the two in step.
  scrubIntervalHours: [1, 8_760],
  scrubSliceSize: [10, 10_000],
  scrubDeepSample: [0, 100],
  // The drill cadence is entered in DAYS but stored in seconds, so this range is
  // the backend's DRILL_INTERVAL_MIN..MAX (1 day .. 1 year) expressed in days -
  // keep the two in step.
  drillIntervalDays: [1, 365],
  // Matches DRILL_SAMPLE_MIN..MAX. The floor is 1, not 0: zero would be a
  // second, redundant kill-switch when `enabled` already is one.
  drillSampleSize: [1, 50],
} as const;

export function clampToRange(value: number, [min, max]: readonly [number, number]): number {
  return Math.min(max, Math.max(min, value));
}

// Accept `string | number`: an `<input type="number">` bound with `v-model`
// yields a number, while an `event.target.value` read yields a string. Coerce
// to a trimmed string first so neither call site crashes on `.trim()`.
//
// Parse an OPTIONAL field ("" = the special "null"/unlimited/auto value), clamped
// to its backend range when a value is present.
export function parseOptionalClamped(
  input: string | number,
  range: readonly [number, number]
): number | null {
  const trimmed = String(input).trim();
  if (trimmed === "") return null;
  const value = Number(trimmed);
  if (!Number.isFinite(value)) return null;
  return clampToRange(Math.floor(value), range);
}

// Parse a REQUIRED field, clamped to its backend range; a non-numeric input
// keeps the current value (fallback).
export function parseRequiredClamped(
  input: string | number,
  range: readonly [number, number],
  fallback: number
): number {
  const value = Number(String(input).trim());
  if (!Number.isFinite(value)) return fallback;
  return clampToRange(Math.floor(value), range);
}

// Minute-of-day (0..1439) -> "HH:MM" for a native <input type="time">.
export function minutesToHHMM(min: number): string {
  const m = ((Math.floor(min) % 1440) + 1440) % 1440;
  const hh = String(Math.floor(m / 60)).padStart(2, "0");
  const mm = String(m % 60).padStart(2, "0");
  return `${hh}:${mm}`;
}

// "HH:MM" -> minute-of-day, or null when the value cannot be parsed (rather
// than silently coercing to midnight).
export function hhmmToMinutes(value: string): number | null {
  const [h, m] = value.split(":").map((n) => Number(n));
  if (!Number.isFinite(h) || !Number.isFinite(m)) return null;
  return (((h * 60 + m) % 1440) + 1440) % 1440;
}

export interface UseSettingsForm {
  commitPatch(patch: SettingsPatch, successKey?: string): Promise<void>;
  clampToRange(value: number, range: readonly [number, number]): number;
  parseOptionalClamped(input: string | number, range: readonly [number, number]): number | null;
  parseRequiredClamped(
    input: string | number,
    range: readonly [number, number],
    fallback: number
  ): number;
  RANGES: typeof RANGES;
}

export function useSettingsForm(): UseSettingsForm {
  const { t } = useI18n();
  const settings = useSettingsStore();
  const toasts = useToastsStore();

  // All Rules commits route through here. The store records the failure as
  // `errorCode` (rendered as the inline banner above the form), so we SWALLOW the
  // rejection rather than let it escape the @change handler as an unhandled promise
  // rejection (which produced a Vue "Unhandled error during execution of native
  // event handler" warning). The form stays usable; the banner explains the error.
  async function commitPatch(p: SettingsPatch, successKey = "toast.settingsSaved"): Promise<void> {
    try {
      await settings.patch(p);
      // The Rules form has no Save button - every control commits on change, and
      // until now did so completely silently. One toast per commit is the whole
      // confirmation. Rapid toggling does not stack: the toast store folds an
      // identical consecutive message back into the toast already showing.
      toasts.push({ kind: "success", message: t(successKey) });
    } catch {
      // errorCode is set on the store and surfaced as the banner.
    }
  }

  return { commitPatch, clampToRange, parseOptionalClamped, parseRequiredClamped, RANGES };
}
