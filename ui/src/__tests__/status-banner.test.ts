// @vitest-environment jsdom
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { mount, flushPromises } from "@vue/test-utils";
import { createPinia, setActivePinia } from "pinia";

// StatusBanner tests (Banner Task 6, docs/superpowers/specs/2026-08-01-pause-banner-design.md).
// Absorbs the old PausedBanner suite unchanged (manual pause: hidden when
// unpaused, timed vs indefinite copy, the live countdown, Resume success +
// failure) and extends it with the gate-driven reasons the new component
// renders via `bannerModel`: battery/metered bypass, offline retry, schedule
// "resumes at HH:MM", the gear deep link, and cross-account priority. The
// backend seams are `invoke` (resume_sync / sync_now) and `listen`, mocked as
// before; `vue-router` is mocked per settings-components.test.ts so the gear
// button's `router.push` can be asserted without a real router.

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string, args?: unknown) => invokeMock(cmd, args),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => vi.fn()),
}));

const pushMock = vi.fn();
vi.mock("vue-router", () => ({
  useRouter: () => ({ push: pushMock }),
  useRoute: () => ({ params: {} }),
}));

import { i18n } from "../i18n";
import StatusBanner from "../components/StatusBanner.vue";
import { usePauseStore } from "../stores/pause";
import { useProgressStore } from "../stores/progress";
import { useSettingsStore } from "../stores/settings";
import type { OrchestratorState, ScheduleSettings } from "../ipc/types";

const BANNER = '[data-testid="paused-banner"]';
const RESUME = '[data-testid="paused-banner-resume"]';
const ERROR = '[data-testid="paused-banner-error"]';
const RETRY = '[data-testid="status-banner-retry"]';
const BYPASS = '[data-testid="status-banner-bypass"]';
const GEAR = '[data-testid="status-banner-gear"]';

function perAccount(accountId: string, state: OrchestratorState) {
  return { account_id: accountId, state };
}
function paused(reason: string): OrchestratorState {
  return { state: "paused", reason };
}
function backoff(until: number): OrchestratorState {
  return { state: "backoff", until };
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

// Only `global.schedule` is read by StatusBanner, so a partial cast (same
// idiom as about-mac-gating.test.ts's FAKE_SETTINGS) is enough here - no need
// to hand-construct every SettingsDto field.
function makeSettings(sched: ScheduleSettings) {
  return { global: { schedule: sched } } as never;
}

function mountBanner() {
  const pinia = createPinia();
  setActivePinia(pinia);
  const pause = usePauseStore();
  const progress = useProgressStore();
  const settings = useSettingsStore();
  const wrapper = mount(StatusBanner, { global: { plugins: [pinia, i18n] } });
  return { pause, progress, settings, wrapper };
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  pushMock.mockReset();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("StatusBanner - manual pause (absorbed PausedBanner behaviour)", () => {
  it("renders nothing while sync is not paused", async () => {
    const { wrapper } = mountBanner();
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).exists()).toBe(false);
  });

  it("shows the indefinite copy for a pause-until-I-resume", async () => {
    const { pause, wrapper } = mountBanner();
    pause.ingest({ kind: "indefinite" });
    await wrapper.vm.$nextTick();

    const banner = wrapper.find(BANNER);
    expect(banner.exists()).toBe(true);
    expect(banner.text()).toContain("Backups paused indefinitely");
    expect(banner.attributes("role")).toBe("status");
  });

  it("shows the minutes left for a timed pause", async () => {
    const { pause, wrapper } = mountBanner();
    pause.ingest({ kind: "timed", until_ms: Date.now() + 27 * 60_000 });
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).text()).toContain("Backups paused - 27m left");
  });

  it("counts the timed pause down on its one-second tick", async () => {
    vi.useFakeTimers();
    const { pause, wrapper } = mountBanner();
    const until = Date.now() + 30 * 60_000;
    pause.ingest({ kind: "timed", until_ms: until });
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).text()).toContain("30m left");

    // Advance real wall-clock time by 5 minutes, then let the component's
    // interval fire so it re-reads the clock.
    vi.setSystemTime(Date.now() + 5 * 60_000);
    await vi.advanceTimersByTimeAsync(1_000);
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).text()).toContain("25m left");
  });

  // The banner's hide edge is now debounced 500ms (smoke fix) so a transient
  // orchestrator blip never flashes the banner away - so an intentional,
  // durable clear (pause.ingest(null) here) must be observed past that delay,
  // not on the very next tick.
  it("hides once the pause clears (past the 500ms hide debounce)", async () => {
    vi.useFakeTimers();
    const { pause, wrapper } = mountBanner();
    pause.ingest({ kind: "indefinite" });
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).exists()).toBe(true);

    pause.ingest(null);
    await vi.advanceTimersByTimeAsync(500);
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).exists()).toBe(false);
  });

  it("resumes on a single click of the Resume button", async () => {
    vi.useFakeTimers();
    const { pause, wrapper } = mountBanner();
    pause.ingest({ kind: "timed", until_ms: Date.now() + 10 * 60_000 });
    await wrapper.vm.$nextTick();

    await wrapper.find(RESUME).trigger("click");
    // advanceTimersByTimeAsync also drains the microtask queue at each step,
    // so this both lets the resume_sync promise settle AND clears the 500ms
    // hide debounce armed the instant the store optimistically clears.
    await vi.advanceTimersByTimeAsync(500);
    await wrapper.vm.$nextTick();

    expect(invokeMock).toHaveBeenCalledWith("resume_sync", undefined);
    expect(wrapper.find(BANNER).exists()).toBe(false);
  });

  it("keeps the banner and surfaces the error when resuming fails", async () => {
    invokeMock.mockRejectedValue(new Error("resume blew up"));
    const { pause, wrapper } = mountBanner();
    pause.ingest({ kind: "indefinite" });
    await wrapper.vm.$nextTick();

    await wrapper.find(RESUME).trigger("click");
    await flushPromises();

    // Backups are still paused, so the banner must not disappear.
    expect(wrapper.find(BANNER).exists()).toBe(true);
    expect(wrapper.find(ERROR).text()).toContain("resume blew up");
    // ...and the button is usable again for a retry.
    expect(wrapper.find(RESUME).attributes("disabled")).toBeUndefined();
  });
});

describe("StatusBanner - gate-driven reasons (Banner Task 6)", () => {
  it("renders the battery label + bypass button, and bypass calls sync_now with bypassGates: true", async () => {
    const { progress, wrapper } = mountBanner();
    progress.ingest(perAccount("acct-1", paused("battery")));
    await wrapper.vm.$nextTick();

    const banner = wrapper.find(BANNER);
    expect(banner.exists()).toBe(true);
    expect(banner.text()).toContain("Paused - on battery power");

    await wrapper.find(BYPASS).trigger("click");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("sync_now", { sourceId: null, bypassGates: true });
  });

  it("renders the offline label + retry button, and retry calls sync_now with bypassGates: null", async () => {
    const { progress, wrapper } = mountBanner();
    progress.ingest(perAccount("acct-1", paused("no_internet")));
    await wrapper.vm.$nextTick();

    const banner = wrapper.find(BANNER);
    expect(banner.exists()).toBe(true);
    expect(banner.text()).toContain("Paused - no internet connection");

    await wrapper.find(RETRY).trigger("click");
    await flushPromises();

    expect(invokeMock).toHaveBeenCalledWith("sync_now", { sourceId: null, bypassGates: null });
  });

  it("routes the gear on a battery pause to /settings/schedule-power", async () => {
    const { progress, wrapper } = mountBanner();
    progress.ingest(perAccount("acct-1", paused("battery")));
    await wrapper.vm.$nextTick();

    await wrapper.find(GEAR).trigger("click");

    expect(pushMock).toHaveBeenCalledWith("/settings/schedule-power");
  });

  it("renders 'resumes at HH:MM' for a schedule pause, computed via nextWindowOpenMinute", async () => {
    vi.useFakeTimers();
    // Freeze at local minute 480 (08:00 UTC, utcOffsetMinutes: 0); the window
    // opens at minute 600 (10:00).
    vi.setSystemTime(new Date("2026-08-01T08:00:00.000Z"));

    const { progress, settings, wrapper } = mountBanner();
    settings.settings = makeSettings(schedule({ startMinute: 600, endMinute: 660 }));
    progress.ingest(perAccount("acct-1", paused("schedule")));
    await wrapper.vm.$nextTick();

    expect(wrapper.find(BANNER).text()).toContain("resumes at 10:00");
  });

  it("lets captive portal beat a simultaneous battery pause across two accounts", async () => {
    const { progress, wrapper } = mountBanner();
    progress.ingest(perAccount("acct-battery", paused("battery")));
    progress.ingest(perAccount("acct-portal", paused("captive_portal")));
    await wrapper.vm.$nextTick();

    const banner = wrapper.find(BANNER);
    expect(banner.text()).toContain("Wi-Fi needs sign-in");
    expect(banner.text()).not.toContain("battery power");
    expect(wrapper.find(RETRY).exists()).toBe(true);
    // Captive portal has no gear target.
    expect(wrapper.find(GEAR).exists()).toBe(false);
  });

  it("renders the destination-unreachable label + retry for a Backoff state", async () => {
    const { progress, wrapper } = mountBanner();
    progress.ingest(perAccount("acct-1", backoff(Date.now() + 60_000)));
    await wrapper.vm.$nextTick();

    expect(wrapper.find(BANNER).text()).toContain("the backup destination is unreachable");
    expect(wrapper.find(RETRY).exists()).toBe(true);
  });

  it("never throws when the on-mount settings refresh fails - the banner still renders", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "get_settings") return Promise.reject(new Error("settings backend down"));
      return Promise.resolve(undefined);
    });
    const { progress, wrapper } = mountBanner();
    progress.ingest(perAccount("acct-1", paused("battery")));
    await flushPromises();
    await wrapper.vm.$nextTick();

    expect(wrapper.find(BANNER).text()).toContain("Paused - on battery power");
  });
});

describe("StatusBanner - clock freshness when ticking arms (fix round 1)", () => {
  // Regression for a stale-clock bug: `now` is only ever refreshed inside the
  // ticking interval, so if the component sat mounted-but-hidden for a long
  // stretch (no interval running) before a reason appeared, the FIRST render
  // used to read `now` from whenever it was last set - potentially hours
  // stale - rather than the instant the reason appeared. Proven here via the
  // expired-manual-pause gate: a timed pause whose `until_ms` has ALREADY
  // passed relative to the real (fresh) clock, but is still in the future
  // relative to the stale one, must render as expired (no banner), not as a
  // live countdown.
  it("gates an already-expired timed pause using a FRESH clock, not the clock from mount time", async () => {
    vi.useFakeTimers();
    const mountedAt = Date.now();
    const { pause, wrapper } = mountBanner();
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).exists()).toBe(false);

    // The component sits idle - hidden, no timer running - for 2 hours. `now`
    // is never touched during this stretch (nothing ticks while hidden).
    vi.setSystemTime(mountedAt + 2 * 60 * 60_000);

    // A timed pause arrives whose expiry (1h after mount) is already an hour
    // in the past relative to the REAL clock, but still looks like 1h in the
    // FUTURE relative to the stale mount-time clock - the exact condition
    // that would make the old code show a live (wrong) countdown.
    pause.ingest({ kind: "timed", until_ms: mountedAt + 60 * 60_000 });
    await wrapper.vm.$nextTick();

    expect(wrapper.find(BANNER).exists()).toBe(false);
  });
});

describe("StatusBanner - hide debounce (smoke fix)", () => {
  // Same root cause as GlobalProgressBar's flicker: a paused account's
  // periodic orchestrator tick briefly clears (PowerCheck) before re-pausing,
  // and `bannerModel` reads the same `progress.states`, so `model` can blip
  // to null and back within one tick. Only the HIDE edge debounces here -
  // showing a fresh reason, and swapping between two different reasons, both
  // stay instant (proven by the existing gate-driven-reasons tests above,
  // none of which advance any timer to see their banner appear).
  it("never unmounts for a blip shorter than 500ms (e.g. a transient PowerCheck tick)", async () => {
    vi.useFakeTimers();
    const { progress, wrapper } = mountBanner();
    progress.ingest(perAccount("acct-1", paused("battery")));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).exists()).toBe(true);

    // The reason clears for 200ms (the transient tick) then comes right back
    // - well under the debounce window.
    progress.ingest(perAccount("acct-1", { state: "power_check" }));
    await vi.advanceTimersByTimeAsync(200);
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).exists()).toBe(true);
    expect(wrapper.find(BANNER).text()).toContain("Paused - on battery power");

    progress.ingest(perAccount("acct-1", paused("battery")));
    await vi.advanceTimersByTimeAsync(1_000);
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).exists()).toBe(true);
  });

  it("hides once the cleared reason holds for the full 500ms", async () => {
    vi.useFakeTimers();
    const { progress, wrapper } = mountBanner();
    progress.ingest(perAccount("acct-1", paused("battery")));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).exists()).toBe(true);

    progress.ingest(perAccount("acct-1", { state: "power_check" }));
    await vi.advanceTimersByTimeAsync(499);
    expect(wrapper.find(BANNER).exists()).toBe(true);

    await vi.advanceTimersByTimeAsync(1);
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).exists()).toBe(false);
  });

  it("swaps reason-to-reason instantly - not a hide+show, no debounce", async () => {
    const { progress, wrapper } = mountBanner();
    progress.ingest(perAccount("acct-1", paused("battery")));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).text()).toContain("Paused - on battery power");

    progress.ingest(perAccount("acct-1", paused("captive_portal")));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BANNER).text()).toContain("Wi-Fi needs sign-in");
  });

  it("disables the action button during the hide linger - no dispatch against a reason that already cleared", async () => {
    vi.useFakeTimers();
    const { progress, wrapper } = mountBanner();
    progress.ingest(perAccount("acct-1", paused("battery")));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BYPASS).attributes("disabled")).toBeUndefined();

    progress.ingest(perAccount("acct-1", { state: "power_check" }));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BYPASS).attributes("disabled")).toBeDefined();

    // ...and re-enables instantly once the reason comes back, same as the
    // label swap above.
    progress.ingest(perAccount("acct-1", paused("battery")));
    await wrapper.vm.$nextTick();
    expect(wrapper.find(BYPASS).attributes("disabled")).toBeUndefined();
  });
});
