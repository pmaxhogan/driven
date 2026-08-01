# Unified pause/status banner - design

Date: 2026-08-01
Status: design approved by the user in-session 2026-07-31 ("agreed on your
sequencing plan"); this doc is the written spec. PR 2 of the settings
redesign sequence (after the v2.6.0 menu bar extra, before the sidebar IA).

## Problem

Gate-driven pauses (battery / metered / network family / schedule) are
invisible in the main window: the amber `PausedBanner` only renders MANUAL
pauses, and the tray tooltip is the only surface explaining a gate pause.
During the v2.6.0 smoke the user hit exactly this: backups "paused" with no
in-window explanation, and tray "Resume" flipping straight back to paused
because a resume cannot override a closed gate.

## Goals

One persistent amber banner at the top of the window whenever backups are
not running for ANY pause reason, stating why, offering the right one-click
action for that reason, and deep-linking to the setting that controls the
gate where one exists.

## Reason model

`PauseReason` (driven-core types.rs): Manual, Battery, Metered, Offline,
ServiceDown, NoInternet, CaptivePortal, DnsFailed, Schedule.

Priority when several accounts pause for different reasons (highest wins,
user-directed: network always beats battery; captive portal tops because
only the user can fix it):

```
CaptivePortal > Offline > DnsFailed > NoInternet > ServiceDown
  > Metered > Battery > Schedule > Manual
```

Banner visibility: shown whenever at least one account is paused (any
reason). Aggregation across accounts picks the highest-priority reason. An
account actively syncing does not hide the banner if another is paused -
the label names the reason, not "everything is stopped".

## Per-reason banner content

| Reason | Label | Primary action | Gear link |
|---|---|---|---|
| Manual (timed) | "Backups paused - 27m left" (live countdown, existing) | Resume | none |
| Manual (indefinite) | "Backups paused indefinitely" | Resume | none |
| Battery | "Paused - on battery power" | "Back up anyway" (one-shot bypass) | battery setting |
| Metered | "Paused - on a metered network" | "Back up anyway" (one-shot bypass) | metered setting |
| Offline / NoInternet / DnsFailed | "Paused - no internet connection" (one label for the family) | "Retry" (fresh probe + sync tick = existing sync-now path) | NEW offline-gate setting |
| CaptivePortal | "Paused - Wi-Fi needs sign-in" | "Retry" | none |
| ServiceDown | "Paused - the backup destination is unreachable" | "Retry" | none |
| Schedule | "Waiting for backup window - resumes at HH:MM" (computed client-side from the schedule config) | "Back up now" (one-shot bypass) | schedule setting |

All labels via vue-i18n; the reason wording mirrors the tray tooltip copy.
The existing `PausedBanner` manual behaviour (countdown tick, Resume with
error handling) is absorbed unchanged into the new component.

## New backend surface

### One-shot gate bypass

`sync_now` does NOT bypass gates today (verified in the smoke: a manual
tick still hits `gate closed; pausing reason=Battery`). Add a per-account
one-shot bypass:

- driven-core: the orchestrator gains a `bypass_gates_once` flag
  (set-and-consumed semantics: the next gate check that would fail on
  Battery / Metered / Schedule passes instead and CLEARS the flag; gates
  re-apply from the following cycle). The flag does NOT bypass the network
  family (Offline/NoInternet/DnsFailed/CaptivePortal/ServiceDown) - you
  cannot will a network into existence; for those the action is Retry.
- src-tauri: a `sync_now_bypassing_gates(account_id)` command (or a
  `bypass: bool` parameter on the existing sync-now IPC - pick whichever
  fits the existing command shape better) that sets the flag and triggers
  a manual tick. Fires for every account when the UI's action is global
  (match the existing sync-now scope).

### Offline-gate setting

New global setting `pause_when_offline: bool`, default `true` (SPEC s22
`global` group, alongside `skip_on_battery`):

- `true` (default): current behaviour - the network probes gate the cycle.
- `false`: the Offline / NoInternet / DnsFailed probe outcomes no longer
  pause the cycle (for LAN/local-folder destinations, or when the probe is
  wrong). CaptivePortal and ServiceDown still pause - the portal genuinely
  blocks, and ServiceDown is destination-specific reachability, not
  general-internet health.
- Storage/DTO/patch/merge-arm/UI plumbing mirrors `skip_on_battery`
  exactly. Surfaced in the Rules form next to the battery/metered toggles
  ("Keep backing up without internet access" - inverted phrasing on the
  UI toggle is fine as long as the wire field is `pauseWhenOffline`).
- Orchestrator-affecting: changing it reconfigures running orchestrators
  (same path as the battery/metered settings).

### Pause-state surface to the UI

The banner needs per-account `(reason, since)`. `sync:status_changed`
already carries the full `OrchestratorState` per account, including
`Paused { reason }` - the UI progress store keeps the latest state per
account. Reuse that; do NOT add a new event channel. The schedule
"resumes at HH:MM" is computed in the UI from the settings store's
schedule config (next window-open time in local tz).

## UI structure

- Rename/extend `PausedBanner.vue` -> `StatusBanner.vue` (keep the file
  history via git mv). Inputs: pause store (manual pause state), progress
  store (per-account OrchestratorState), settings store (schedule config
  for the resume time; nothing else).
- Pure helper `bannerModel(inputs) -> { reason, label parts, action kind,
  gear target } | null` unit-tested for the priority order, multi-account
  aggregation, and each reason's action/gear mapping.
- Actions dispatch: Resume -> existing pause store resume; Retry ->
  existing sync-now; Back up anyway / Back up now -> the new bypass IPC.
  Busy/error states match the existing banner's Resume handling.
- Gear: a small icon button routing to `/rules` (the current settings
  page; the sidebar IA PR will retarget these links) - carry a query/hash
  (e.g. `/rules#power`) so the IA PR can scroll-anchor later without a
  contract change.
- The banner slots where PausedBanner renders today (under the global
  progress bar in App.vue).

## Testing

- Rust: gate-bypass consumed-once semantics (bypassed cycle runs, next
  cycle gates again); bypass does NOT apply to the network family;
  `pause_when_offline=false` exempts exactly Offline/NoInternet/DnsFailed;
  settings round-trip + merge arm tests mirroring skip_on_battery's.
- UI: bannerModel priority/aggregation table tests; per-reason render +
  action dispatch tests (vitest, existing store-mock harness); the
  StatusBanner mount test satisfies the coverage gate for the renamed
  component.
- Coverage gate: net-new Rust logic is small and unit-testable (flag
  consume, gate exemption) - no expected regression.

## Out of scope

- The sidebar IA (PR 3) and any settings-page restructuring; gear links
  target the current Rules page.
- Tray menu changes (v2.6.0 already shows the pause reason there).
- Multi-reason simultaneous display (only the highest-priority reason
  renders; the rest are visible per-account in the tray tooltip).

## Acceptance criteria

1. On battery with default settings: amber banner "Paused - on battery
   power" with "Back up anyway" + gear; clicking "Back up anyway" runs
   exactly one sync cycle, after which the gate pauses again; the gear
   lands on the Rules power section.
2. With Wi-Fi off: banner "Paused - no internet connection" with Retry +
   gear; enabling the new setting ("keep backing up without internet")
   makes a local-folder destination sync while offline.
3. Manual pause behaviour is unchanged from today (countdown, Resume).
4. Priority: offline beats battery when both hold.
5. `cargo test --workspace`, clippy, fmt, UI suite + coverage gate green.
