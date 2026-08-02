// Banner Task 5 (docs/superpowers/specs/2026-08-01-pause-banner-design.md):
// pure helpers behind the unified pause/status banner. No store/component
// imports - StatusBanner.vue (Task 6) supplies the live inputs (progress
// store's per-account OrchestratorState map, pause store's PauseState,
// settings store's ScheduleSettings) and renders whatever this returns.
//
// Lives alongside the repo's other pure-function "stores" (formatBytes.ts,
// activityEventLabel.ts) rather than a new ui/src/lib/ - `ui/src/lib/` does
// not exist and this is the established home for extracted pure helpers.

import type { OrchestratorState, PauseState, ScheduleSettings } from "../ipc/types";

export type BannerReason =
  | "captivePortal"
  | "offline"
  | "dnsFailed"
  | "noInternet"
  | "destinationUnreachable"
  | "metered"
  | "battery"
  | "schedule"
  | "manualTimed"
  | "manualIndefinite";

export type BannerAction = "resume" | "retry" | "bypass";

export interface BannerModel {
  reason: BannerReason;
  action: BannerAction;
  gear: "power" | "schedule" | "offline" | null;
  /** schedule only: minute-of-day for the "resumes at HH:MM" render. */
  resumeAtMinute: number | null;
}

// -----------------------------------------------------------------------------
// Schedule "next window open" calculation
// -----------------------------------------------------------------------------

const MS_PER_MINUTE = 60_000;
const MINUTES_PER_DAY = 1_440;

/**
 * Euclidean division/remainder, matching Rust's `div_euclid`/`rem_euclid` for
 * a positive divisor: the remainder is always in `[0, divisor)`, even for a
 * negative dividend (a pre-epoch or backwards-jumped clock reading). Plain
 * `%`/`Math.floor` division in JS already agrees with this for a positive
 * divisor, so these are thin, self-documenting wrappers.
 */
function euclidDiv(dividend: number, divisor: number): number {
  return Math.floor(dividend / divisor);
}
function euclidMod(dividend: number, divisor: number): number {
  return dividend - divisor * euclidDiv(dividend, divisor);
}

/**
 * Mirrors `ScheduleConfig::allows` (crates/driven-core/src/types.rs:420-446)
 * EXACTLY, operating on an already-computed local total-minute count (rather
 * than re-deriving the UTC offset per call) so `nextWindowOpenMinute` can
 * scan candidate minutes cheaply.
 */
function allowsAtLocalTotalMinute(schedule: ScheduleSettings, totalMin: number): boolean {
  if (!schedule.enabled) return true;
  const minuteOfDay = euclidMod(totalMin, MINUTES_PER_DAY);
  // 1970-01-01 was a Thursday (`Date.getDay() === 4`); offsetting the local
  // day count by 4 before the mod-7 lands on the Sunday-indexed weekday,
  // same as the Rust comment at types.rs:427-429.
  const dayIndex = euclidDiv(totalMin, MINUTES_PER_DAY);
  const dayOfWeek = euclidMod(dayIndex + 4, 7);
  if (!schedule.days[dayOfWeek]) return false;
  const { startMinute: s, endMinute: e } = schedule;
  if (s === e) return true; // whole day allowed; only the day-of-week gates.
  if (s < e) return minuteOfDay >= s && minuteOfDay < e;
  return minuteOfDay >= s || minuteOfDay < e; // wraps past midnight.
}

/**
 * The local minute-of-day (0..=1439) at which the schedule's window next
 * opens, or `null` if no enabled day ever opens it (every `days` entry
 * false). Mirrors `ScheduleConfig::allows` (types.rs:420-446) via
 * {@link allowsAtLocalTotalMinute} above.
 *
 * If the window is ALREADY open at `nowMs` this returns the current local
 * minute-of-day (the scan includes the starting minute) - in practice
 * `bannerModel` only calls this while a schedule pause is active, so the
 * window is closed at call time, but the function stays well-defined
 * either way.
 *
 * Implemented as a brute-force per-minute scan (bounded to one week, <=
 * 10,080 iterations) rather than a closed-form day-skip calculation:
 * reusing the exact per-instant check that mirrors the Rust `allows` gives
 * behavioural parity by construction instead of by re-derived day-skip
 * reasoning, and a week-long scan is trivial cost for a UI helper called at
 * most once per banner render.
 */
export function nextWindowOpenMinute(schedule: ScheduleSettings, nowMs: number): number | null {
  const localMs = nowMs + schedule.utcOffsetMinutes * MS_PER_MINUTE;
  const startTotalMin = euclidDiv(localMs, MS_PER_MINUTE);
  const WEEK_MINUTES = 7 * MINUTES_PER_DAY;
  for (let i = 0; i <= WEEK_MINUTES; i++) {
    const totalMin = startTotalMin + i;
    if (allowsAtLocalTotalMinute(schedule, totalMin)) {
      return euclidMod(totalMin, MINUTES_PER_DAY);
    }
  }
  return null;
}

// -----------------------------------------------------------------------------
// Reason priority + aggregation
// -----------------------------------------------------------------------------

/**
 * Pre-manual-split canonical reason, ordered highest-priority first (DESIGN
 * "Reason model": `CaptivePortal > Offline > DnsFailed > NoInternet >
 * ServiceDown > Metered > Battery > Schedule > Manual`).
 *
 * This is a UI-level severity order and DELIBERATELY diverges from the
 * tray's `pause_reason_rank` (src-tauri/src/menubar.rs:485-503), which picks
 * one reason for its single status row via an arbitrary declaration-order
 * ranking (Manual first) - not a severity judgement. Per the design doc's
 * Code-reality notes: "The banner's severity-priority order is a UI-level
 * decision and MAY disagree; do not 'fix' the tray to match."
 */
type CanonicalReason =
  | "captivePortal"
  | "offline"
  | "dnsFailed"
  | "noInternet"
  | "destinationUnreachable"
  | "metered"
  | "battery"
  | "schedule"
  | "manual";

const PRIORITY: readonly CanonicalReason[] = [
  "captivePortal",
  "offline",
  "dnsFailed",
  "noInternet",
  "destinationUnreachable",
  "metered",
  "battery",
  "schedule",
  "manual",
];

/**
 * Maps a wire-format snake_case `Paused` reason string to its canonical
 * banner reason. `service_down` maps to the SAME slot as `Backoff` -
 * `destinationUnreachable` - per the design doc's Code-reality notes:
 * `PauseReason::ServiceDown` is never produced by the engine today, but the
 * live "destination unreachable" signal (`OrchestratorState::Backoff`) sits
 * in exactly the priority slot ServiceDown was reserved for. Any other
 * unrecognised string is forward-compat-ignored (returns `undefined`).
 */
function canonicalPausedReason(reason: string): CanonicalReason | undefined {
  switch (reason) {
    case "manual":
      return "manual";
    case "battery":
      return "battery";
    case "metered":
      return "metered";
    case "offline":
      return "offline";
    case "no_internet":
      return "noInternet";
    case "captive_portal":
      return "captivePortal";
    case "dns_failed":
      return "dnsFailed";
    case "schedule":
      return "schedule";
    case "service_down":
      return "destinationUnreachable";
    default:
      return undefined;
  }
}

/**
 * Narrows an opaque per-account `OrchestratorState` to its canonical banner
 * reason, or `undefined` if the account is not paused/backed-off (or
 * reports an unrecognised paused reason). Reads the wire shapes directly:
 * `{ state: "paused", reason: "<snake_case>" }` / `{ state: "backoff",
 * until: <ms> }` (design doc's Code-reality notes).
 */
function canonicalReasonForState(state: OrchestratorState): CanonicalReason | undefined {
  if (state.state === "backoff") return "destinationUnreachable";
  if (state.state === "paused" && typeof state.reason === "string") {
    return canonicalPausedReason(state.reason);
  }
  return undefined;
}

function higherPriority(a: CanonicalReason, b: CanonicalReason): CanonicalReason {
  return PRIORITY.indexOf(a) <= PRIORITY.indexOf(b) ? a : b;
}

/**
 * Aggregates every account's `OrchestratorState` plus the standalone manual
 * `PauseState` into a single banner, or `null` when nothing is paused. An
 * account actively syncing does not hide the banner if another account is
 * paused - the label names the reason, not "everything is stopped" (DESIGN
 * "Reason model").
 *
 * Manual pause is primarily derived from the per-account states (each
 * account's gate check transitions to `Paused { reason: Manual }` -
 * orchestrator.rs's `evaluate_gates`/`GateDecision::Pause` path, applied at
 * orchestrator.rs:2157-2160), but a NOT-YET-EXPIRED `pause` ALSO contributes
 * a synthetic lowest-priority "manual" candidate. This covers the gap
 * between the pause store flipping and the next orchestrator tick reflecting
 * it in `states` (or a stale/empty states map) without ever outranking a
 * real gate-driven reason, since manual already sits at the lowest priority
 * slot. "Not yet expired" matters because the pause store does NOT
 * self-expire a timed pause locally - `pause` stays populated with a
 * past `until_ms` until the backend's `sync:pause_changed(null)` event
 * lands - so an elapsed timed pause with no account actually paused must
 * NOT fabricate a banner; only `indefinite` or `timed` with `until_ms` still
 * in the future contributes the synthetic candidate.
 *
 * Manual timed-vs-indefinite is read from `pause` (its `kind` discriminant),
 * never from the per-account reason string - `Paused { reason: Manual }`
 * carries no timed/indefinite info. `pause === null` while an account still
 * reports "manual" (a stale read, or a resume that raced the next tick)
 * renders as `manualIndefinite`, the safer of the two (no false countdown).
 */
export function bannerModel(
  states: Record<string, OrchestratorState>,
  pause: PauseState | null,
  schedule: ScheduleSettings | null,
  nowMs: number
): BannerModel | null {
  let winning: CanonicalReason | undefined;
  for (const state of Object.values(states)) {
    const reason = canonicalReasonForState(state);
    if (reason === undefined) continue;
    winning = winning === undefined ? reason : higherPriority(winning, reason);
  }
  const pauseStillActive =
    pause !== null && (pause.kind === "indefinite" || pause.until_ms > nowMs);
  if (pauseStillActive) {
    winning = winning === undefined ? "manual" : higherPriority(winning, "manual");
  }
  if (winning === undefined) return null;

  if (winning === "manual") {
    // Render as timed only while the pause is still active (see
    // `pauseStillActive` above) - an expired-but-not-yet-cleared timed
    // pause must never render a stale/negative countdown.
    const reason: BannerReason =
      pauseStillActive && pause !== null && pause.kind === "timed"
        ? "manualTimed"
        : "manualIndefinite";
    return { reason, action: "resume", gear: null, resumeAtMinute: null };
  }

  if (winning === "captivePortal" || winning === "destinationUnreachable") {
    return { reason: winning, action: "retry", gear: null, resumeAtMinute: null };
  }

  if (winning === "offline" || winning === "dnsFailed" || winning === "noInternet") {
    return { reason: winning, action: "retry", gear: "offline", resumeAtMinute: null };
  }

  if (winning === "battery" || winning === "metered") {
    // BannerModel's gear enum has no dedicated "metered" slot - both toggles
    // live in the same Rules power section (DESIGN acceptance criterion 1:
    // "the gear lands on the Rules power section"), so battery and metered
    // share `gear: "power"`.
    return { reason: winning, action: "bypass", gear: "power", resumeAtMinute: null };
  }

  // winning === "schedule"
  return {
    reason: "schedule",
    action: "bypass",
    gear: "schedule",
    resumeAtMinute: schedule === null ? null : nextWindowOpenMinute(schedule, nowMs),
  };
}
