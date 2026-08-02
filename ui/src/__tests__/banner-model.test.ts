import { describe, it, expect } from "vitest";

import { bannerModel, nextWindowOpenMinute } from "../stores/bannerModel";
import type { OrchestratorState, PauseState, ScheduleSettings } from "../ipc/types";

// Banner Task 5 (docs/superpowers/specs/2026-08-01-pause-banner-design.md):
// pure aggregation of per-account OrchestratorState + the manual PauseState
// into one banner, plus a client-side mirror of `ScheduleConfig::allows`
// (crates/driven-core/src/types.rs:420-446) for the schedule "resumes at
// HH:MM" readout.

function paused(reason: string): OrchestratorState {
  return { state: "paused", reason };
}
function backoff(until: number): OrchestratorState {
  return { state: "backoff", until };
}
function idle(): OrchestratorState {
  return { state: "idle", last_run_at: null };
}
function executing(): OrchestratorState {
  return { state: "executing", progress: {} };
}

function schedule(overrides: Partial<ScheduleSettings> = {}): ScheduleSettings {
  return {
    enabled: true,
    startMinute: 0,
    endMinute: 0,
    days: [true, true, true, true, true, true, true],
    utcOffsetMinutes: 0,
    ...overrides,
  };
}

const TIMED: PauseState = { kind: "timed", until_ms: 999_999 };
const INDEFINITE: PauseState = { kind: "indefinite" };

describe("bannerModel", () => {
  it.each([
    ["captive_portal", "captivePortal", "retry", null],
    ["offline", "offline", "retry", "offline"],
    ["dns_failed", "dnsFailed", "retry", "offline"],
    ["no_internet", "noInternet", "retry", "offline"],
    ["service_down", "destinationUnreachable", "retry", null],
    ["metered", "metered", "bypass", "power"],
    ["battery", "battery", "bypass", "power"],
  ] as const)("maps paused reason %s to (%s, %s, gear=%s)", (wireReason, reason, action, gear) => {
    const model = bannerModel({ a: paused(wireReason) }, null, null, 0);
    expect(model).toEqual({ reason, action, gear, resumeAtMinute: null });
  });

  it("maps Backoff to destinationUnreachable/retry/no-gear", () => {
    const model = bannerModel({ a: backoff(123_456) }, null, null, 0);
    expect(model).toEqual({
      reason: "destinationUnreachable",
      action: "retry",
      gear: null,
      resumeAtMinute: null,
    });
  });

  it("maps a schedule pause to bypass/schedule-gear with a computed resumeAtMinute", () => {
    const sched = schedule({ startMinute: 600, endMinute: 660 });
    // now = local minute 480 (08:00); the window next opens at minute 600.
    const model = bannerModel({ a: paused("schedule") }, null, sched, 8 * 60 * 60_000);
    expect(model).toEqual({
      reason: "schedule",
      action: "bypass",
      gear: "schedule",
      resumeAtMinute: 600,
    });
  });

  it("maps manual paused reason + a timed PauseState to manualTimed/resume/no-gear", () => {
    const model = bannerModel({ a: paused("manual") }, TIMED, null, 0);
    expect(model).toEqual({
      reason: "manualTimed",
      action: "resume",
      gear: null,
      resumeAtMinute: null,
    });
  });

  it("maps manual paused reason + an indefinite PauseState to manualIndefinite", () => {
    const model = bannerModel({ a: paused("manual") }, INDEFINITE, null, 0);
    expect(model).toEqual({
      reason: "manualIndefinite",
      action: "resume",
      gear: null,
      resumeAtMinute: null,
    });
  });

  it("treats a manual paused reason with pause=null as manualIndefinite (pause is the source of truth for timed-vs-indefinite)", () => {
    const model = bannerModel({ a: paused("manual") }, null, null, 0);
    expect(model?.reason).toBe("manualIndefinite");
  });

  it("chooses timed vs indefinite from the pause argument, not the per-account reason string", () => {
    // Two accounts both report the generic "manual" reason (it carries no
    // timed/indefinite info); only the standalone `pause` argument decides.
    const states = { a: paused("manual"), b: paused("manual") };
    expect(bannerModel(states, TIMED, null, 0)?.reason).toBe("manualTimed");
    expect(bannerModel(states, INDEFINITE, null, 0)?.reason).toBe("manualIndefinite");
  });

  it("falls back to a synthetic manual candidate when pause is set but no account state reflects it yet", () => {
    // Covers the gap between the pause store flipping and the next
    // orchestrator tick updating `states` (or a stale/empty states map).
    const model = bannerModel({ a: idle(), b: executing() }, TIMED, null, 0);
    expect(model).toEqual({
      reason: "manualTimed",
      action: "resume",
      gear: null,
      resumeAtMinute: null,
    });
  });

  it("does NOT fabricate a banner from an elapsed timed pause when no account is actually paused", () => {
    // The pause store does not self-expire locally - `pause` stays populated
    // with a past `until_ms` until the backend's `sync:pause_changed(null)`
    // event lands. An elapsed timed pause must not push the synthetic manual
    // candidate, or the banner would outlive the pause it describes.
    const elapsed: PauseState = { kind: "timed", until_ms: 1_000 };
    const nowMs = 5_000; // now is well past until_ms
    const model = bannerModel({ a: idle(), b: executing() }, elapsed, null, nowMs);
    expect(model).toBeNull();
  });

  it("falls back to manualIndefinite (not a stale manualTimed) when states report manual but the pause has already elapsed", () => {
    // Here an account's state still says "manual" (the race hasn't cleared
    // yet), but the standalone PauseState is already expired - rendering
    // must not show a stale/negative countdown.
    const elapsed: PauseState = { kind: "timed", until_ms: 1_000 };
    const nowMs = 5_000;
    const model = bannerModel({ a: paused("manual") }, elapsed, null, nowMs);
    expect(model).toEqual({
      reason: "manualIndefinite",
      action: "resume",
      gear: null,
      resumeAtMinute: null,
    });
  });

  describe("priority ordering (highest wins)", () => {
    it("captive portal beats offline", () => {
      const states = { a: paused("captive_portal"), b: paused("offline") };
      expect(bannerModel(states, null, null, 0)?.reason).toBe("captivePortal");
    });

    it("offline beats battery", () => {
      const states = { a: paused("offline"), b: paused("battery") };
      expect(bannerModel(states, null, null, 0)?.reason).toBe("offline");
    });

    it("backoff (destination unreachable) beats metered", () => {
      const states = { a: backoff(1), b: paused("metered") };
      expect(bannerModel(states, null, null, 0)?.reason).toBe("destinationUnreachable");
    });

    it("battery beats schedule", () => {
      const states = { a: paused("battery"), b: paused("schedule") };
      expect(bannerModel(states, null, schedule(), 0)?.reason).toBe("battery");
    });

    it("schedule beats manual", () => {
      const states = { a: paused("schedule"), b: paused("manual") };
      expect(bannerModel(states, INDEFINITE, schedule(), 0)?.reason).toBe("schedule");
    });
  });

  it("still banners when one account is syncing and another is paused", () => {
    const states = { a: executing(), b: paused("battery") };
    expect(bannerModel(states, null, null, 0)?.reason).toBe("battery");
  });

  it("returns null when no account is paused/backed-off and pause is null", () => {
    const states = { a: idle(), b: executing() };
    expect(bannerModel(states, null, null, 0)).toBeNull();
  });

  it("ignores an unknown/forward-compat paused reason", () => {
    expect(bannerModel({ a: paused("some_future_reason") }, null, null, 0)).toBeNull();
  });

  it("ignores an unknown paused reason on one account while still honouring a known reason on another", () => {
    const states = { a: paused("some_future_reason"), b: paused("battery") };
    expect(bannerModel(states, null, null, 0)?.reason).toBe("battery");
  });

  it("returns resumeAtMinute=null for a schedule pause when all days are disabled", () => {
    const sched = schedule({
      startMinute: 600,
      endMinute: 660,
      days: [false, false, false, false, false, false, false],
    });
    const model = bannerModel({ a: paused("schedule") }, null, sched, 0);
    expect(model).toEqual({
      reason: "schedule",
      action: "bypass",
      gear: "schedule",
      resumeAtMinute: null,
    });
  });

  it("returns resumeAtMinute=null for a schedule pause when the schedule argument is null", () => {
    const model = bannerModel({ a: paused("schedule") }, null, null, 0);
    expect(model).toEqual({
      reason: "schedule",
      action: "bypass",
      gear: "schedule",
      resumeAtMinute: null,
    });
  });
});

describe("nextWindowOpenMinute", () => {
  it("finds a simple later-today opening", () => {
    // Monday 2026-08-03 08:00 UTC, window 10:00-11:00, every day enabled.
    const now = Date.UTC(2026, 7, 3, 8, 0, 0);
    const sched = schedule({ startMinute: 600, endMinute: 660 });
    expect(nextWindowOpenMinute(sched, now)).toBe(600);
  });

  it("wraps past midnight, skipping a disabled evening day to the enabled morning tail", () => {
    // Monday 2026-08-03 02:00 UTC; window 23:00-06:00 wraps midnight.
    // Monday (index 1) is disabled, so the whole of Monday is closed even
    // though 02:00 falls inside the [00:00, 06:00) morning tail; the window
    // reopens at Tuesday 00:00 once Tuesday's day flag is checked.
    const now = Date.UTC(2026, 7, 3, 2, 0, 0);
    const sched = schedule({
      startMinute: 1380,
      endMinute: 360,
      days: [true, false, true, true, true, true, true],
    });
    expect(nextWindowOpenMinute(sched, now)).toBe(0);
  });

  it("skips multiple consecutive disabled days to the next enabled day", () => {
    // Monday 2026-08-03 08:00 UTC; Monday and Tuesday disabled, Wednesday
    // enabled; window 10:00-11:00 (no wrap).
    const now = Date.UTC(2026, 7, 3, 8, 0, 0);
    const sched = schedule({
      startMinute: 600,
      endMinute: 660,
      days: [false, false, false, true, false, false, false],
    });
    expect(nextWindowOpenMinute(sched, now)).toBe(600);
  });

  it("treats start==end as the whole day allowed, gated only by the day-of-week", () => {
    // Monday disabled, Tuesday enabled; start==end means the entire enabled
    // day is open starting at its first minute.
    const now = Date.UTC(2026, 7, 3, 8, 0, 0);
    const sched = schedule({
      startMinute: 300,
      endMinute: 300,
      days: [false, false, true, false, false, false, false],
    });
    expect(nextWindowOpenMinute(sched, now)).toBe(0);
  });

  it("applies a non-zero utcOffsetMinutes and gates on the LOCAL day, not the UTC day", () => {
    // now = 2026-08-04T04:00:00Z (Tuesday, UTC). With utcOffsetMinutes=-480
    // (PST, UTC-8) local time is 2026-08-03 20:00 - Monday evening, a
    // different calendar day than the UTC timestamp. Sunday/Monday are
    // disabled and Tuesday is enabled (start==end whole-day, so only the
    // day-of-week gates): the correct LOCAL day is Monday (disabled), so the
    // window does not open until local Tuesday 00:00. A sign-flip or a
    // dropped offset would instead evaluate the UTC day (already Tuesday,
    // enabled) and return the current UTC minute-of-day (240) immediately -
    // a value distinct enough from the correct answer (0) to catch either
    // mistake.
    const now = Date.UTC(2026, 7, 4, 4, 0, 0);
    const sched = schedule({
      startMinute: 500,
      endMinute: 500,
      utcOffsetMinutes: -480,
      days: [false, false, true, false, false, false, false],
    });
    expect(nextWindowOpenMinute(sched, now)).toBe(0);
  });

  it("returns null when every day is disabled", () => {
    const sched = schedule({
      startMinute: 600,
      endMinute: 660,
      days: [false, false, false, false, false, false, false],
    });
    expect(nextWindowOpenMinute(sched, Date.UTC(2026, 7, 3, 8, 0, 0))).toBeNull();
  });

  it("treats a disabled schedule as always-open, returning the current local minute", () => {
    const sched = schedule({
      enabled: false,
      days: [false, false, false, false, false, false, false],
    });
    // 08:30 UTC, utcOffsetMinutes=0 -> local minute-of-day 510.
    expect(nextWindowOpenMinute(sched, Date.UTC(2026, 7, 3, 8, 30, 0))).toBe(510);
  });

  it("uses Euclidean minute math for a pre-epoch (negative) timestamp", () => {
    // nowMs = -1 -> local total-minute = -1 -> minute-of-day = 1439,
    // day-index = -1 -> dow = (-1 + 4) mod 7 = 3 (Wednesday; 1969-12-31 was
    // a Wednesday, consistent with the epoch-Thursday anchor comment on
    // ScheduleConfig::allows). Only Wednesday enabled + start==end whole-day
    // means it is already open at that instant.
    const sched = schedule({
      startMinute: 300,
      endMinute: 300,
      days: [false, false, false, true, false, false, false],
    });
    expect(nextWindowOpenMinute(sched, -1)).toBe(1439);
  });

  it("uses Euclidean minute math to cross the epoch boundary forward to the next allowed minute", () => {
    // nowMs = -1 -> minute-of-day 1439, dow=3 (Wednesday) disabled here; the
    // very next local minute (totalMin=0, i.e. epoch instant) lands on
    // Thursday (dow 4, enabled) at minute-of-day 0, inside [0, 1).
    const sched = schedule({
      startMinute: 0,
      endMinute: 1,
      days: [true, true, true, false, true, true, true],
    });
    expect(nextWindowOpenMinute(sched, -1)).toBe(0);
  });
});
