# Unified Pause/Status Banner Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A persistent amber banner in the main window whenever backups are paused for ANY reason, with the reason, the right one-click action (Resume / Retry / one-shot gate bypass), and a settings deep-link.

**Architecture:** driven-core gains a one-shot `bypass_gates_once` flag on the orchestrator and a `pause_when_offline` config gate exemption; src-tauri plumbs the new setting end to end (mirroring `skip_on_battery`) and adds a bypass parameter to `sync_now`; the UI replaces `PausedBanner` with a `StatusBanner` driven by a pure, table-tested `bannerModel` helper over the existing pause/progress/settings stores.

**Tech Stack:** Rust (driven-core, tauri commands), Vue 3 + Pinia + vitest.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-01-pause-banner-design.md` (read the "Code-reality notes" - Backoff IS the destination-unreachable signal; ServiceDown is display-only).
- Priority order (highest wins): CaptivePortal > Offline > DnsFailed > NoInternet > destination-unreachable(Backoff) > Metered > Battery > Schedule > Manual.
- The one-shot bypass applies ONLY to Metered / Battery / Schedule gates. Never to the network family, never to a Manual pause, never to the Drive breaker's Backoff.
- `pause_when_offline=false` exempts EXACTLY Offline / NoInternet / DnsFailed. CaptivePortal and Backoff still pause. Default `true`.
- The bypass flag is consumed ONLY when a bypassable gate would actually have failed - an open-gates cycle must not burn it.
- Wire field names: `pauseWhenOffline` (global group, camelCase DTO / snake_case storage with `#[serde(default = ...)] = true`).
- ASCII "-" only in code/comments/strings; LF; repo comment style (explain why, cite SPEC/DESIGN like neighbors).
- Rust: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`. UI: `pnpm --dir ui test:unit` (+ the package.json typecheck script). Coverage gate is regression-vs-main: every new .ts/.vue file needs real tests.
- Branch: `feat/pause-banner` (exists, has the spec commit). Conventional commit subjects.
- Do not modify tray.rs / menubar.rs (v2.6.0 surfaces stay as shipped).

---

### Task 1: driven-core - `bypass_gates_once` on the orchestrator

**Files:**
- Modify: `crates/driven-core/src/orchestrator.rs` (struct fields ~line 376-500, `new` ~524-556, `evaluate_gates` ~946-1012, trait `Orchestrator` ~305-334, impl ~2471+)
- Test: existing orchestrator test module in the same file

**Interfaces:**
- Produces: trait method `fn bypass_gates_once(&self)` on `pub trait Orchestrator` WITH a default no-op body (so `FakeOrchestrator`/`SlowDrainOrchestrator` in src-tauri compile unchanged); `SyncOrchestrator` overrides it to set a new `bypass_gates_once: AtomicBool` field (init `false` in `new`).
- Consumes: existing `evaluate_gates` gate sequence and `GateDecision`.

- [ ] **Step 1: Write the failing tests** (use the file's existing orchestrator test harness - fake power/network/clock; copy the nearest gate test's setup):

```rust
/// A one-shot bypass lets exactly one cycle through a closed battery gate;
/// the next cycle gates again (spec 2026-08-01 s"One-shot gate bypass").
#[tokio::test]
async fn bypass_gates_once_passes_one_battery_gated_cycle() {
    // power: on battery; config: skip_on_battery = true
    // 1) evaluate_gates() == Pause(Battery)
    // 2) orch.bypass_gates_once(); evaluate_gates() == Proceed
    // 3) evaluate_gates() == Pause(Battery)  // flag consumed
}

/// The bypass does NOT override the network family or a manual pause.
#[tokio::test]
async fn bypass_gates_once_never_bypasses_network_or_manual() {
    // network probe -> NoInternet: bypass set -> still Pause(NoInternet),
    //   AND the flag is NOT consumed (assert a later battery-only cycle
    //   still passes using the same armed flag)
    // manual pause set -> bypass set -> still Pause(Manual)
}

/// An open-gates cycle does not burn the flag.
#[tokio::test]
async fn bypass_survives_an_open_gates_cycle() {
    // all gates open; bypass set; evaluate_gates() == Proceed;
    // then close battery gate; evaluate_gates() == Proceed (flag used now);
    // then evaluate_gates() == Pause(Battery)
}
```

- [ ] **Step 2: Run, verify compile failure** (`cargo test -p driven-core bypass`).
- [ ] **Step 3: Implement.**
  - Field `bypass_gates_once: std::sync::atomic::AtomicBool` next to `suspended` (~:478), documented: one-shot, set by the app layer, consumed by the first cycle where a Metered/Battery/Schedule gate would otherwise close (spec 2026-08-01).
  - Trait: `fn bypass_gates_once(&self) {}` default body + doc; `SyncOrchestrator` impl: `self.bypass_gates_once.store(true, Ordering::SeqCst);`
  - In `evaluate_gates`, restructure the metered (~:986), battery (~:991), and schedule (~:1001) checks so each failing check first consults the flag: `if self.bypass_gates_once.swap(false, Ordering::SeqCst) { /* skip ALL THREE bypassable gates this cycle */ }` - implement as: evaluate the three bypassable gates; if any would pause AND `swap(false)` returns true, skip all three (one consume covers the cycle); if the swap returns false, pause with the FIRST failing reason as today. Manual/network/breaker checks run before and are untouched.
  - Ordering note in a comment: the flag is only read on the run-loop task, `SeqCst` is for simplicity not necessity.
- [ ] **Step 4: Run, verify pass** + `cargo clippy -p driven-core --all-targets -- -D warnings`.
- [ ] **Step 5: Commit** - `git commit -am "feat(core): one-shot gate bypass on the orchestrator"`

---

### Task 2: driven-core - `pause_when_offline` config + gate exemption

**Files:**
- Modify: `crates/driven-core/src/orchestrator.rs` (`OrchestratorConfig` ~:148-247, `Default` ~:250-291, `evaluate_gates` ~:975-981, `complete_resume` ~:2283-2292)
- Test: same file's test module

**Interfaces:**
- Produces: `pub pause_when_offline: bool` on `OrchestratorConfig` (doc: default true; false exempts Offline/NoInternet/DnsFailed - NOT CaptivePortal - for LAN/local destinations; spec 2026-08-01), `Default` = `true`.
- Consumes: `pause_reason_for_network` (~:2404), the reachability short-circuit (~:975-977).

- [ ] **Step 1: Failing tests:**

```rust
/// pause_when_offline=false exempts exactly the reachability family.
#[tokio::test]
async fn offline_exemption_skips_reachability_but_not_captive_portal() {
    // cfg.pause_when_offline = false
    // (a) OS reachability false, probe -> Offline   => Proceed
    // (b) probe -> NoInternet                        => Proceed
    // (c) probe -> DnsFailed                         => Proceed
    // (d) probe -> CaptivePortal                     => Pause(CaptivePortal)
}

/// Default behaviour unchanged: with the setting true, Offline still pauses.
#[tokio::test]
async fn offline_still_pauses_by_default() { /* cfg default; probe Offline => Pause(Offline) */ }

/// The post-wake re-probe honours the exemption too (recon: complete_resume).
#[tokio::test]
async fn resume_reprobe_honours_offline_exemption() {
    // drive complete_resume (or its extracted decision helper) with
    // probe -> NoInternet and pause_when_offline=false: must NOT re-pause.
}
```

- [ ] **Step 2: Run, verify failure.**
- [ ] **Step 3: Implement.**
  - `evaluate_gates` ~:975: when `!power.network_reachable`, if `cfg.pause_when_offline` return `Pause(Offline)` as today; else FALL THROUGH to the probe (comment: an exempted machine must still surface CaptivePortal - spec).
  - ~:978-981: after mapping `pause_reason_for_network(net)`, if `!cfg.pause_when_offline` and the reason is `Offline | NoInternet | DnsFailed`, continue past the network gate instead of pausing; `CaptivePortal` pauses regardless.
  - `complete_resume` ~:2283-2292: apply the same reason filter (extract a small `fn network_pause_applies(cfg, reason) -> bool` used by both sites so the rule lives once).
- [ ] **Step 4: Run, verify pass** + clippy. Also `cargo test -p driven-core` full (the exhaustive-literal warning: struct-literal config sites outside the crate use `..Default::default()`, but scan `rg "OrchestratorConfig \{" crates src-tauri` and fill any exhaustive literal - recon found `src-tauri/src/commands/settings.rs:1578` which Task 3 owns).
- [ ] **Step 5: Commit** - `git commit -am "feat(core): pause_when_offline gate exemption"`

---

### Task 3: src-tauri - setting plumbing + sync_now bypass parameter

**Files:**
- Modify: `src-tauri/src/commands/settings.rs` (storage::Global ~:1081-1125 + serde default fn ~:1134, From impls ~:1156/:1182, merge arm ~:454, default_global ~:2908, load_orchestrator_config ~:1550-1600 incl. the exhaustive literal ~:1578)
- Modify: `src-tauri/src/commands/dtos.rs` (GlobalSettings ~:694-751, GlobalSettingsPatch ~:997-1060)
- Modify: `src-tauri/src/commands/sync.rs` (`sync_now` ~:145-186)
- Modify: `src-tauri/src/tray.rs:1127` ONLY if the signature change requires a new argument at the existing call site (pass `None`) - no behavior change.
- Test: settings.rs + sync.rs inline test modules; sibling of the assembly.rs:1542 config-derivation test.

**Interfaces:**
- Produces: `GlobalSettings.pause_when_offline: bool` (wire `pauseWhenOffline`), `GlobalSettingsPatch.pause_when_offline: Option<bool>`, merge arm setting `orchestrator_affecting = true`, `load_orchestrator_config` mapping it into `OrchestratorConfig`; `sync_now(state, source_id, bypass_gates: Option<bool>)` - when `bypass_gates == Some(true)`, call `orchestrator.bypass_gates_once()` on each targeted account's handle BEFORE `trigger(TickSource::Manual)`.
- Consumes: Tasks 1-2.

- [ ] **Step 1: Failing tests:**

```rust
/// pauseWhenOffline round-trips through the storage layer with default true
/// (legacy global blobs without the field still deserialize as true).
#[tokio::test]
async fn pause_when_offline_defaults_true_and_round_trips() { /* mirror the
    skip_on_battery storage tests + a legacy-json deserialization assert */ }

/// The merge arm applies the field and marks the patch orchestrator-affecting
/// (storage-layer replication per this file's established pattern).
#[tokio::test]
async fn update_settings_merges_pause_when_offline() { /* mirror
    update_settings_merge_round_trips_a_single_field at ~:3202 */ }
```

And in sync.rs's test module (it has repo-backed tests ~:428): a test that `sync_now` with `bypass_gates: Some(true)` calls `bypass_gates_once` before `trigger` - use the FakeOrchestrator in app_state.rs (~:1346): add a recorded-calls Vec to it for `bypass_gates_once` (it gets the default no-op from Task 1; override it to record) and assert ordering.

- [ ] **Step 2: Run, verify failure.**
- [ ] **Step 3: Implement** exactly along the `skip_on_battery` template (all 12 recon sites): storage field with `#[serde(default = "default_pause_when_offline")]` fn returning `true`; both From impls; DTO + patch; merge arm `if let Some(v) = g.pause_when_offline { cur.pause_when_offline = v; orchestrator_affecting = true; }`; `default_global` `pause_when_offline: true`; `load_orchestrator_config` mapping; fill the exhaustive `OrchestratorConfig` literal at ~:1578. Do NOT touch migration 0002 (serde default is the pattern - recon note 13). `sync_now` gains the optional `bypass_gates` param (additive; the UI invoke sends camelCase `bypassGates`); tray.rs call site passes `None`.
- [ ] **Step 4: Run, verify pass**; `cargo test -p driven-app` full; clippy workspace; fmt.
- [ ] **Step 5: Commit** - `git commit -am "feat: pause_when_offline setting and sync_now gate bypass"`

---

### Task 4: UI - setting toggle + types + ipc

**Files:**
- Modify: `ui/src/ipc/types.ts` (GlobalSettings ~:310-345, GlobalSettingsPatch ~:509-535)
- Modify: `ui/src/ipc/commands.ts` (`syncNow` ~:235-237)
- Modify: `ui/src/views/Settings.vue` (Power and network section ~:806-830)
- Modify: `ui/src/locales/en-US.json` (settings.rules keys ~:340-360)
- Test: `ui/src/__tests__/settings-components.test.ts` (+ fixture sweeps in settings-stores.test.ts:67, ipc-commands.test.ts:52)

**Interfaces:**
- Produces: `GlobalSettings.pauseWhenOffline: boolean` (+ patch field); `syncNow(sourceId: string | null, bypassGates?: boolean)` wrapper passing `{ sourceId, bypassGates: bypassGates ?? null }`; a "Pause backups when the internet is unreachable" checkbox (`data-testid="pause-when-offline-toggle"`, checked by default) with a hint naming the LAN/local-folder use case, patching `{ global: { pauseWhenOffline } }` via the existing `commitPatch`.
- Consumes: Task 3 wire shape.

- [ ] **Step 1: Failing test** (settings-components.test.ts, existing harness):

```ts
it("pause-when-offline toggle patches global.pauseWhenOffline", async () => {
  // uncheck [data-testid="pause-when-offline-toggle"]
  // assert update_settings invoked with { global: { pauseWhenOffline: false } }
});
```

- [ ] **Step 2: Run, verify fail** (plus the fixture type errors surface every fixture needing `pauseWhenOffline: true`).
- [ ] **Step 3: Implement** (checkbox markup mirrors the battery toggle at Settings.vue:811-819 including a `data-testid`; locale keys `pauseWhenOfflineLabel` + `pauseWhenOfflineNote`; sweep ALL fixtures flagged by vue-tsc).
- [ ] **Step 4: Run, verify pass** - full `pnpm --dir ui test:unit` + typecheck script.
- [ ] **Step 5: Commit** - `git commit -am "feat(ui): pause-when-offline setting toggle"`

---

### Task 5: UI - bannerModel pure helper + schedule next-open calc

**Files:**
- Create: `ui/src/lib/bannerModel.ts`
- Test: `ui/src/__tests__/banner-model.test.ts` (new file - coverage gate requires it)

**Interfaces:**
- Produces:
  - `type BannerReason = "captivePortal" | "offline" | "dnsFailed" | "noInternet" | "destinationUnreachable" | "metered" | "battery" | "schedule" | "manualTimed" | "manualIndefinite"`
  - `type BannerAction = "resume" | "retry" | "bypass"`
  - `interface BannerModel { reason: BannerReason; action: BannerAction; gear: "power" | "schedule" | "offline" | null; resumeAtMinute: number | null /* schedule only: minute-of-day for HH:MM render */ }`
  - `function bannerModel(states: Record<string, OrchestratorState>, pause: PauseState | null, schedule: ScheduleSettings | null, nowMs: number): BannerModel | null`
  - `function nextWindowOpenMinute(schedule: ScheduleSettings, nowMs: number): number | null` - mirrors `ScheduleConfig::allows` semantics EXACTLY (fixed utcOffsetMinutes; Euclidean minute math; `Date.getDay()` day indexing; start==end whole-day; start>end midnight wrap needing both days enabled); returns the local minute-of-day when the window next opens, `null` if enabled windows never open (all days off).
  - Wire-state narrowing: a `paused` state is `{ state: "paused", reason: string }` with snake_case reasons (`manual|battery|metered|offline|no_internet|captive_portal|dns_failed|schedule|service_down`); `backoff` is `{ state: "backoff", until: number }`. Unknown reasons are ignored (forward-compat).
- Consumes: types.ts `OrchestratorState` (opaque record), `PauseState`, `ScheduleSettings`.

- [ ] **Step 1: Failing table tests** covering: each single-account reason maps to the right (reason, action, gear); Backoff -> destinationUnreachable/retry/no-gear; priority pairs (captive beats offline; offline beats battery; backoff beats metered; battery beats schedule; schedule beats manual); manual timed vs indefinite chosen from `pause` (PauseState) not the per-account state; multi-account (one syncing + one paused still banners); no paused accounts -> null; unknown reason string -> ignored; `nextWindowOpenMinute` cases: inside-window returns null-ish semantics (banner not shown for schedule when window open - the model only queries it when a schedule pause exists), simple later-today open, midnight-wrap, next-enabled-day skip, all-days-off -> null.
- [ ] **Step 2: Run, verify fail.**
- [ ] **Step 3: Implement** (pure functions, no imports from stores; document the deliberate divergence from the tray's `pause_reason_rank` with the spec citation).
- [ ] **Step 4: Run, verify pass** + typecheck.
- [ ] **Step 5: Commit** - `git commit -am "feat(ui): banner model with reason priority and schedule next-open calc"`

---

### Task 6: UI - StatusBanner component replacing PausedBanner

**Files:**
- Rename: `git mv ui/src/components/PausedBanner.vue ui/src/components/StatusBanner.vue`
- Modify: `ui/src/App.vue` (:8 import, :137 mount), `ui/src/locales/en-US.json` (extend the pauseBanner block into a statusBanner block)
- Modify tests: `ui/src/__tests__/paused-banner.test.ts` -> `status-banner.test.ts` (extend), `ui/src/__tests__/app-shell.test.ts` (:35, :151)
- Test: extended status-banner tests

**Interfaces:**
- Consumes: `bannerModel` (Task 5), pause store, progress store (`states`), settings store (`settings.global.schedule`), `syncNow(null)` / `syncNow(null, true)` (Task 4), router.
- Produces: the rendered banner. `data-testid`s: keep `paused-banner`, `paused-banner-resume`, `paused-banner-error` for the manual path (existing tests keep passing), add `status-banner-retry`, `status-banner-bypass`, `status-banner-gear`.

- [ ] **Step 1: Failing tests** (extend the existing mount harness; seed the progress store's `states` via its `ingest` with wire-shaped payloads):
  - battery-paused account renders "Paused - on battery power" + bypass button; clicking invokes `sync_now` with `{ sourceId: null, bypassGates: true }`.
  - offline-paused renders the no-internet label + retry; retry invokes `sync_now` with `{ sourceId: null, bypassGates: null }`.
  - gear on battery routes to `/rules#power` (router push mock per settings-components.test.ts:39-44).
  - schedule pause renders "resumes at HH:MM" using `nextWindowOpenMinute` (freeze time with fake timers).
  - manual timed keeps the existing countdown/Resume behavior (existing assertions keep passing after the rename).
  - captive portal beats a simultaneous battery pause (two accounts seeded).
- [ ] **Step 2: Run, verify fail.**
- [ ] **Step 3: Implement.** Keep the `Transition` fade, amber styling, `role="status"`. Busy/error handling for retry/bypass mirrors `onResume` (disable while in flight, error span). Label composition through `t()` with per-reason keys (`statusBanner.battery`, ... reuse the tray wording); keep `pauseBanner.*` keys for the manual path. Gear = small icon button -> `router.push("/rules#" + anchor)`.
- [ ] **Step 4: Run, verify pass** - full UI suite + typecheck (App.vue/app-shell updates included).
- [ ] **Step 5: Commit** - `git commit -am "feat(ui): unified status banner for all pause reasons"`

---

### Task 7: Full green + live smoke + PR

- [ ] **Step 1:** `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check && pnpm --dir ui test:unit`
- [ ] **Step 2:** Diff sweep: em/en dashes, todo!/unimplemented!, console.log, stray testids.
- [ ] **Step 3:** Live smoke (controller, sandboxed data dir as before): on battery, banner shows the battery row; "Back up anyway" runs exactly one cycle then re-pauses; toggling the new offline setting while offline lets a local-folder sync run; manual pause still counts down.
- [ ] **Step 4:** PR: `feat(ui): explain pauses in-app with a status banner and one-shot bypass` -> /d -mp -> release PR (2.7.0) per the established flow. (Controller-owned, not a task subagent.)
