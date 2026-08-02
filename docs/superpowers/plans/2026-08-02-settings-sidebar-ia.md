# Settings Sidebar IA Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the four top settings tabs (Accounts / Sources / Rules / About) with one Settings shell hosting a left sidebar and routed content pages, per spec `docs/superpowers/specs/2026-07-31-settings-redesign-menubar-design.md` s1 (Direction A, mock `a-sidebar-macos.html`).

**Architecture:** `Settings.vue` shrinks to a shell (sidebar + `<RouterView>`); the 760-line Rules form decomposes into seven page components under `ui/src/views/settings/`; Accounts/Sources/About become routed children; old flat routes redirect. Existing form logic (clamping, patch round-trip, VSS/APFS polling) moves with its section, unchanged in behaviour.

**Tech Stack:** Vue 3 + vue-router 4 (nested children routes), Pinia stores (unchanged), vitest + VTU, vue-i18n.

## Global Constraints

- ASCII `-` only - no em/en dashes anywhere (code, locale strings, comments, commit messages).
- LF line endings.
- Conventional PR title: `feat(ui): settings sidebar IA` (squash merge; branch commits need not be conventional).
- Behaviour-preserving moves: every setting reachable in the old UI remains reachable and functional; no store or backend changes; `SettingsPatch` payload shapes unchanged.
- Locale: `no-missing-keys` is an ERROR - a `t()` call site and its key must exist together in every commit. Moved markup keeps its existing `settings.rules.*` / `about.*` keys (keys do NOT move namespaces); only net-new keys are added (`settings.nav.*`, page titles). Orphaned keys only warn - clean them at the end, never mid-move.
- Coverage gate is aggregate (epsilon 0.1pp vs main): every new `.vue` gets a real mount test in the same task that creates it.
- The updater event subscription stays owned by App.vue (R2-P1-3) - never move it into About or any page.
- Out of scope (PR 4): Accounts/Sources row redesign (mock E), `StatusPill`, `ToggleSwitch`, `SettingRow` extraction. Moved cards keep their existing inline checkbox/select markup verbatim.

## Locked decisions (spec ambiguities resolved)

- **Page set + routes** (children of `/settings`): `accounts`, `sources`, `general`, `schedule-power`, `performance`, `platform`, `network`, `privacy`, `advanced`, `about`.
- **Redirects:** `/settings` -> `/settings/accounts`; `/accounts` -> `/settings/accounts`; `/sources` -> `/settings/sources`; `/rules` -> `/settings/general`; `/about` -> `/settings/about`.
- **Card -> page mapping** (source line ranges are Settings.vue at commit time of this plan; re-locate by section heading if drifted):

| Card (lines) | Destination page |
|---|---|
| Startup / autoStartOnLogin (795-812) | platform (launch-at-login with platform copy) |
| Power and network (815-890): skipOnBattery, pauseWhenOffline, skipOnMetered + metered mode/cap | schedule-power |
| Schedule window (893-958) | schedule-power |
| Performance and bandwidth (961-1167): bandwidthCapMbps, defaultConcurrentUploads, adaptiveParallelismEnabled, ioPriority | performance |
| - scanIntervalSecs (inside 961-1167) | general |
| - deepVerifyIntervalSecs (inside 961-1167) | advanced |
| - VSS degraded banner + windows.vssMode + vssHelper (inside 961-1167) | platform |
| - macos.apfsSnapshot + TCC note (inside 961-1167) | platform |
| Menu bar extra (1170-1256) | platform |
| Small-file bundling (1259-1276) | advanced |
| Integrity scrub (1279-1344) | advanced |
| Backup hooks (1347-1396) | advanced |
| Custom root CA (1399-1430) | network |
| Proxy (1433-1488) | network |
| Privacy/telemetry (1491-1516) | privacy (single home - About's duplicate is removed) |
| About.vue channel selector (250-267) | general |
| About.vue check-for-updates (269-299) | general AND about (an action, duplicated per mock F) |

- **Gear links:** all three `bannerModel` gear values route to real pages with NO hash (recon: nothing consumes `route.hash` today, and the destination pages are now short): `power` -> `/settings/schedule-power`, `schedule` -> `/settings/schedule-power`, `offline` -> `/settings/schedule-power` (pauseWhenOffline lives on schedule-power beside the other pause gates - one "when does Driven pause?" page).
- **Platform page:** rendered from the existing `settings.windows` / `settings.macos` nullability; label "macOS" or "Windows"; hidden from the nav when both are null (Linux).
- **Sidebar search:** client-side label + per-page keyword filter, no fuzzy engine.
- **About page:** identity only - version/channel/up-to-date/last-check, check-for-updates, diagnostics export, release notes, license fineprint. Reached ONLY from the sidebar footer (`Driven <version> · <channel> · About`).
- **Log level:** has no UI control today; do NOT add one (out of scope).

## File Structure

- Create: `ui/src/composables/useSettingsForm.ts` (commitPatch + RANGES/clamps + minutesToHHMM, extracted verbatim)
- Create: `ui/src/views/settings/GeneralPage.vue`, `SchedulePowerPage.vue`, `PerformancePage.vue`, `PlatformPage.vue`, `NetworkPage.vue`, `PrivacyPage.vue`, `AdvancedPage.vue`, `AboutPage.vue` (thin wrapper), `AccountsPage.vue` (thin wrapper), `SourcesPage.vue` (thin wrapper)
- Create: `ui/src/components/SettingsNav.vue`
- Modify: `ui/src/views/Settings.vue` (shell), `ui/src/views/About.vue` (identity-only), `ui/src/router.ts`, `ui/src/App.vue` (nav match), `ui/src/components/StatusBanner.vue` (gear targets), `ui/src/locales/en-US.json`
- Tests: `ui/src/__tests__/settings-pages.test.ts` (new), `settings-nav.test.ts` (new), plus updates to `app-shell.test.ts`, `settings-components.test.ts`, `status-banner.test.ts`, `about-*.test.ts`

---

### Task 1: useSettingsForm composable

**Files:**
- Create: `ui/src/composables/useSettingsForm.ts`
- Test: `ui/src/__tests__/use-settings-form.test.ts`
- Modify: `ui/src/views/Settings.vue` (delete the moved script, import the composable)

**Interfaces:**
- Produces: `useSettingsForm()` returning `{ commitPatch(patch: SettingsPatch, successKey?: string): Promise<void>, clampToRange, parseOptionalClamped, parseRequiredClamped, RANGES }` - extracted VERBATIM from Settings.vue lines 282-346 (RANGES const, clamp helpers, commitPatch incl. its toast push and error swallowing). Also export the pure pair `minutesToHHMM(minute: number): string` / `hhmmToMinutes(v: string): number | null` (lines 189-200) as plain module functions (StatusBanner already duplicates minutesToHHMM - do NOT touch StatusBanner here, just note it).

- [ ] **Step 1: Write failing tests** - commitPatch success pushes the toast + calls `settings.patch` with the given payload; commitPatch swallows a rejecting patch (no throw, no success toast); clampToRange clamps below/above/in-range against `RANGES.scanIntervalSecs`; minutesToHHMM(75) === "01:15"; hhmmToMinutes("7:05") === 425 and hhmmToMinutes("junk") === null. Mock the settings store with `vi.fn()` patches (mirror settings-stores.test.ts's store-mock harness).
- [ ] **Step 2: Run - expect FAIL** (module not found).
- [ ] **Step 3: Create the composable by MOVING the code** from Settings.vue (delete there, import `useSettingsForm` + the time helpers there; the ~25 `set*` handlers stay in Settings.vue for now, now calling the composable's commitPatch).
- [ ] **Step 4: Full UI suite + typecheck green** (`pnpm --dir ui test:unit`, `npx vue-tsc --noEmit` from ui/). The existing Rules-tab tests must pass unchanged - this is the proof the move is behaviour-preserving.
- [ ] **Step 5: Commit** `refactor(ui): extract settings form helpers into a composable`.

### Task 2: Rules page components (pure moves, no routing change)

**Files:**
- Create: the seven `ui/src/views/settings/*Page.vue` (General, SchedulePower, Performance, Platform, Network, Privacy, Advanced)
- Test: `ui/src/__tests__/settings-pages.test.ts`
- Modify: `ui/src/views/Settings.vue` (Rules tab body becomes the seven pages rendered in sequence), `settings-components.test.ts` (re-point moved assertions)

**Interfaces:**
- Consumes: `useSettingsForm` (Task 1).
- Produces: each page is a self-contained SFC with NO props, reading `useSettingsStore` directly; page order when stacked matches the old card order per the mapping table. PlatformPage owns the VSS + APFS status refs and polling (moved verbatim from Settings.vue 60-140 incl. the onUnmounted timer clears) and the menu-bar preview computeds (694-711). Each page renders `null`-guards exactly as the old cards did (`v-if="settings.settings"` etc.).

- [ ] **Step 1: Write failing mount tests** - one describe per page in settings-pages.test.ts: mounts with the mocked store fixture (copy the fixture from settings-components.test.ts), asserts the page's headline control exists and one representative patch round-trip per page (e.g. SchedulePowerPage: toggle `pause-when-offline-toggle` -> patch `{global:{pauseWhenOffline:false}}`; PlatformPage: mock `getVssHelperStatus`/`getApfsHelperStatus`; PrivacyPage: telemetry toggle calls `setTelemetryEnabled`).
- [ ] **Step 2: Run - expect FAIL.**
- [ ] **Step 3: Move each card block + its `set*` handlers + card-local state** out of Settings.vue into its page per the mapping table, splitting the Performance card's contents (scan interval -> GeneralPage, deep-verify -> AdvancedPage, VSS/APFS -> PlatformPage, the rest -> PerformancePage) and the Power-and-network card wholly into SchedulePowerPage together with the Schedule-window card. GeneralPage for now holds ONLY scan interval (channel arrives in Task 5). Keep all `t()` keys unchanged. Settings.vue's Rules branch becomes `<GeneralPage/><SchedulePowerPage/>...<AdvancedPage/>` stacked in order.
- [ ] **Step 4: Update settings-components.test.ts** "Settings Rules tab" describe: where a test mounts Settings and pokes a moved control, it still passes (the pages render stacked); fix only imports/selectors that referenced deleted local names.
- [ ] **Step 5: Full suite + typecheck green. Commit** `refactor(ui): split the rules form into per-section page components`.

### Task 3: SettingsNav + shell + routes

**Files:**
- Create: `ui/src/components/SettingsNav.vue`
- Test: `ui/src/__tests__/settings-nav.test.ts`
- Modify: `ui/src/router.ts`, `ui/src/views/Settings.vue`, `ui/src/App.vue`, `ui/src/locales/en-US.json`, `ui/src/__tests__/app-shell.test.ts`

**Interfaces:**
- Consumes: the page components (Task 2) as route children.
- Produces: nested route tree - `/settings` renders Settings.vue (shell) with children `{ path: "accounts", component: AccountsPage }` ... etc. and redirect records for the five old paths per Locked decisions. Settings.vue template becomes: sidebar (`<SettingsNav/>`) + `<main><RouterView/></main>` (keep `cardCls` etc. exported to pages via the existing shared class strings - move any still-shared consts into `ui/src/views/settings/shared.ts` if both shell and pages need them).
- SettingsNav contract: renders (a) a search `<input data-testid="settings-nav-search">` filtering items by label + a per-page keyword array (const `NAV_KEYWORDS: Record<page, string[]>` inside SettingsNav - seed each page with its card headings, e.g. schedule-power: ["battery","metered","offline","schedule","pause"]); (b) object pages Accounts (badge count = accounts needing reauth, from `useAccountsStore`) and Sources; (c) a "Preferences" group: General, Schedule & Power, Performance, platform label (`macOS`/`Windows`, item hidden when `settings.settings?.macos == null && settings.settings?.windows == null`), Network, Privacy & Data, Advanced; (d) footer `Driven <version> · <channel> · About` linking `/settings/about` (`data-testid="settings-nav-about"`), version via the same `getVersion()` IPC About.vue uses, channel from `useUpdaterStore`. Active item styled via `RouterLink`'s active class.

- [ ] **Step 1: Write failing tests** - settings-nav.test.ts: renders all visible items with router stubs; search "batt" filters to Schedule & Power; platform item hidden when both platform groups null, labeled "macOS" when macos non-null; footer links `/settings/about`; reauth badge shows count. app-shell.test.ts updates: top-nav settings href stays `/settings`; "keeps Settings active" assertions now navigate to `/settings/accounts` and `/rules` (asserting the redirect lands on `/settings/general` and nav stays active).
- [ ] **Step 2: Run - expect FAIL.**
- [ ] **Step 3: Implement** SettingsNav, the nested routes + redirects (children array + `{ path: "/rules", redirect: "/settings/general" }` style records), the Settings.vue shell, and simplify App.vue's settings `match` to prefix `/settings` (drop the flat-path array). Add locale keys `settings.nav.*` (group labels, page labels, searchPlaceholder). The old `tab` prop and tab strip are deleted.
- [ ] **Step 4: Full suite + typecheck green** (this is the task where router-first-run.test.ts and any `/rules`-mounting test surface breakage - fix by following redirects, never by weakening what a test asserted).
- [ ] **Step 5: Commit** `feat(ui): settings sidebar navigation and routed pages`.

### Task 4: Accounts / Sources / About as routed pages

**Files:**
- Create: `ui/src/views/settings/AccountsPage.vue`, `SourcesPage.vue`, `AboutPage.vue` (each a thin wrapper: page heading + the existing component/view unchanged: `<AccountList/>`, `<SourceTable/>`, `<About/>`)
- Test: extend `settings-pages.test.ts` (mount each wrapper; assert the inner component renders)
- Modify: none beyond Task 3's route table (which references these; if Task 3 landed with inline `component:` imports of AccountList/SourceTable/About directly, replace with the wrappers here)

**Interfaces:**
- Consumes: AccountList.vue / SourceTable.vue / About.vue unchanged - their own lifecycle subscriptions are component-scoped (recon-verified) and move for free.

- [ ] **Step 1: Failing wrapper mount tests.**
- [ ] **Step 2: Implement wrappers + wire routes.**
- [ ] **Step 3: Full suite + typecheck green. Commit** `feat(ui): route accounts, sources and about as settings pages`.

### Task 5: About cleanup + moves (channel -> General, telemetry -> Privacy only)

**Files:**
- Modify: `ui/src/views/About.vue` (delete the channel-selector card 250-267 and the telemetry card 302-328; keep update-banner, version/license, check-for-updates, diagnostics, release notes), `ui/src/views/settings/GeneralPage.vue` (add the channel selector + a check-for-updates action, markup moved verbatim from About.vue, keys stay `about.*`), `about-telemetry-preview.test.ts` (its subject moved: re-point telemetry-preview assertions at PrivacyPage), `about-mac-gating.test.ts` (verify still-relevant), `settings-components.test.ts` "Settings About tab" describe, `settings-pages.test.ts` (GeneralPage now also asserts channel select -> `updater.setChannel`)
- Test: as above

**Interfaces:**
- Consumes: `useUpdaterStore` (channel state/actions - unchanged); PrivacyPage from Task 2 is already the telemetry home (Settings.vue's old Privacy card), so About's copy is simply deleted - telemetry then has exactly one home.

- [ ] **Step 1: Update/write failing tests first** (GeneralPage channel round-trip; About no longer renders a channel select or telemetry toggle; PrivacyPage still owns the preview modal).
- [ ] **Step 2: Implement the moves/deletions.** The display-language note (About 302-328 block) moves to GeneralPage as a placeholder line per the spec mapping.
- [ ] **Step 3: Full suite + typecheck green. Commit** `feat(ui): make about identity-only, move channel and telemetry home`.

### Task 6: StatusBanner gear retarget

**Files:**
- Modify: `ui/src/components/StatusBanner.vue` (`onGear`: `router.push("/settings/schedule-power")` for all three gear values - keep the mapping switch so a future gear value can diverge; delete the `/rules#` construction), `ui/src/__tests__/status-banner.test.ts` (gear assertions now expect `/settings/schedule-power`)
- Test: as above

- [ ] **Step 1: Update the gear-push expectations first - expect FAIL.**
- [ ] **Step 2: Implement. Full suite green. Commit** `feat(ui): point banner gear links at the new settings pages`.

### Task 7: Full green + locale sweep + smoke + ship

- [ ] **Step 1:** Delete now-orphaned locale keys (`settings.tabs.*`, any `settings.rules.sections.*` heading no longer rendered) - run `pnpm --dir ui lint` and clean every `no-unused-keys` warning this PR introduced (pre-existing warnings stay).
- [ ] **Step 2:** Full gates: `pnpm --dir ui test:unit`, `vue-tsc --noEmit`, lint, prettier; `cargo test --workspace` untouched-but-verify.
- [ ] **Step 3:** Live smoke (sandboxed dev app): sidebar renders with search; each page reachable + one control per page exercised; old URLs redirect; About via footer only; gear link from a battery pause lands on Schedule & Power; Linux-hidden platform item not verifiable on macOS - assert label "macOS".
- [ ] **Step 4:** Ship via `/d -mp`: PR `feat(ui): settings sidebar IA`, drive CI green (coverage epsilon is the main risk - the new pages must carry their mount tests), squash merge, merge the release PR, verify the release.
