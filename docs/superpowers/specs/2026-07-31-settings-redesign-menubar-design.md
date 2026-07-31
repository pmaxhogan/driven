# Settings redesign + macOS menu bar extra - design

Date: 2026-07-31
Status: approved direction (claude.ai/design project "Driven Settings Redesign",
id `4277694a-54ea-4455-9f65-d15f2d8378ad`); this doc is the written spec.

Visual mockups (canonical, user-edited):

- `a-sidebar-macos.html` - chosen layout (Direction A, sidebar). Nav icons are
  inline stroke SVGs (user edit), not emoji.
- `d-menubar-preview.html` - menu bar extra states ("Lgtm" from user).
- `e-accounts-sources.html` - Accounts/Sources rows; status pills are subtle
  dot + text (user edit), not filled badges.
- `f-about-redesign.html` - About reduced to identity.

Out of scope (deferred to a separate epic): a real WidgetKit Control Center /
desktop widget. It requires an Apple Developer ID + notarization pipeline the
repo does not have (macOS builds ship unsigned today; the only signing keys are
the minisign updater pair), a Swift `.appex`, and App Group plumbing. The
throughput derivation built here is a prerequisite it will reuse.

## 1. Settings information architecture (Direction A)

One `Settings` view hosting a left sidebar + routed content pane, replacing the
current four top tabs (`Accounts / Sources / Rules / About`).

### Sidebar

- Search field on top: v1 filters nav items by label + per-page keyword list
  (client-side only; no fuzzy engine).
- Object pages: **Accounts** (with a count badge when any account needs
  reauth), **Sources**.
- "Preferences" group: **General**, **Schedule & Power**, **Performance**,
  **macOS** (or **Windows** - the platform page, label adapts; hidden on
  Linux), **Network**, **Privacy & Data**, **Advanced**.
- Footer: `Driven <version> · <channel> · About` - the only entry point to
  About.

### Section mapping (old Rules wall -> new pages)

| New page | Settings |
|---|---|
| General | scan interval, display language (placeholder), update channel + check for updates |
| Schedule & Power | skip on battery, metered mode + metered cap, schedule window |
| Performance | bandwidth cap, concurrent uploads, adaptive parallelism, I/O priority |
| macOS (platform) | **menu bar extra (new, s2)**, APFS locked-file snapshot, launch at login |
| Windows (platform) | VSS mode + helper, launch at login |
| Network | proxy mode + URL, custom root CA |
| Privacy & Data | telemetry toggle + preview (single home; About's duplicate removed) |
| Advanced | integrity scrub, backup hooks, small-file bundling, deep-verify interval, log level |

Launch-at-login is one cross-platform setting surfaced on the platform page
with platform-appropriate copy (macOS Login Item / Windows startup app).

### Routes

New: `/settings/<page>` per sidebar item. Old deep links redirect:
`/accounts -> /settings/accounts`, `/sources -> /settings/sources`,
`/rules -> /settings/general`, `/about -> /settings/about`.

### Accounts & Sources (mock E)

- Accounts: avatar + name row, provider chip, dot-style status pills
  (Connected / Reconnect required / Encrypted), reauth warning rendered inline
  under its own account row (not a detached banner), per-row `Reconnect` +
  overflow menu.
- Sources: dense table - name + `~/`-abbreviated path (+ E2E pill,
  "respects .gitignore" note), destination as `provider -> remote path` route,
  live status cell (dot + "Backed up · scanned 1 h ago" / "Backing up · 62%" /
  "Paused - account needs reconnect"), enable switch, overflow menu
  (Run now · Edit exclusions · Versioning · Recovery phrase · Remove) replacing
  the four-button strip. Disabled/blocked rows dim.

### About (mock F)

Identity only: app icon, version + channel + up-to-date state + last-check
time, Check for updates, Export diagnostic bundle, release notes list,
license/site fineprint. Update channel selector, telemetry, and display
language move to their sections above.

### Component extraction

New reusable components (each needs a vitest mount test - the `coverage`
required check is regression-vs-main and fails on an untested new `.vue`):
`SettingsNav`, `SettingRow` (label/hint/control slot), `ToggleSwitch`,
`StatusPill`, per-page section components. `Settings.vue` shrinks to shell +
router glue. Existing form logic (clamping, patch round-trip, VSS/APFS status
polling) moves with its section, unchanged in behaviour.

### Store / backend impact

None required for the IA move: pages keep calling `useSettingsStore.patch()`
with the same `SettingsPatch` groups. Platform pages render from the existing
`settings.windows` / `settings.macos` nullability contract.

## 2. macOS menu bar extra

Live text next to the tray icon via `TrayIcon::set_title()` (available in the
pinned tauri 2.11.5; currently unused). macOS only - Windows/Linux trays do
not render titles, so the whole feature is `cfg(target_os = "macos")`-gated
and its settings live in the `macos` group.

### Settings schema (KV group `macos`, SPEC s22 pattern)

```jsonc
macos.menuBar: {
  showUploadSpeed: true,   // "84 Mbps"
  showPercent:     true,   // "62%"
  showFiles:       false,  // "341/2.1k"
  showEta:         false,  // "~4m"
  idle: "none"             // "none" | "lastBackupAge" | "uploadedToday"
}
```

Rust: extend `storage::Macos` + `MacosSettings` DTO + `MacosSettingsPatch`
(mirror the `apfs_snapshot` merge arm). UI: "Menu bar" card on the macOS page
with a live preview strip (mock A) - metric chips + idle select; a soft
warning when >2 metrics are enabled (width).

### Data flow (no driven-core changes)

1. The per-account event bridge (`assembly.rs` `SourceProgress` arm) already
   sees every `ExecProgress` tick. It updates a shared
   `TrayMetrics` (`Arc<Mutex<HashMap<AccountId, AccountProgress>>>`):
   cumulative `bytes_done/bytes_total`, `files_done/files_total`, last-update
   `Instant`. `StateChanged` marks accounts syncing/idle and prunes entries.
2. A 1 Hz tokio task owned by `tray.rs` (pattern: the existing
   `start_sync_animation` interval; runs only while any account syncs, plus a
   60 s slow tick while idle) aggregates across accounts, derives the rate
   from byte deltas, formats the title, and calls `set_title`.
3. Rate is EMA-smoothed over ~3 s. ETA = remaining bytes / smoothed rate,
   hidden until >10 s of samples and rate is stable. Percent =
   sum(bytes_done)/sum(bytes_total).
4. Idle title: `none` -> `set_title(None)`; `lastBackupAge` -> "✓ 2h" from the
   most recent successful sync timestamp (queried from state, cached, 60 s
   refresh); `uploadedToday` -> "1.2 GB today" via the existing
   `activity_summary` window query (60 s refresh). Paused counts as idle.
5. Settings changes apply immediately (update_settings side-effect notifies
   the tray task; no restart).

Explicitly rejected: consuming `ThroughputProbe::take_bytes()` (already
drained by `AdaptiveController` - a second consumer would corrupt AIMD), and
`activity_summary` for the live rate (60 s floor, debounced).

### Formatting rules

- Speed in bits/s, speedtest-style auto-scale: `bit/s -> kbit/s -> Mbit/s ->
  Gbit/s`, rendered compactly as "84 Mbps", <= 3 significant digits.
- Files: compact counts ("341/2.1k"). ETA: "~4m" (minutes; "~40s" under a
  minute; "~1h 20m" over an hour). Separator: " · ". Metric order: percent,
  speed, files, ETA.
- All user-visible words (e.g. "today") through `rust_i18n` / `en-US.yml`;
  tray rebuild-on-locale-change already exists.

### Tray menu header (mock D, included)

Two disabled header rows while syncing: `Backing up "<source>" - 62%, ~4 min
left` and `84 Mbps · 341 of 2,148 files`, refreshed when the menu state
changes (menus are rebuilt, not live; refresh on state transitions and on a
coarse timer while open is NOT attempted in v1 - rows reflect
last-known-at-open values).

### Testing

- Pure functions (`format_speed_bits`, `format_title`, EMA/ETA stability)
  unit-tested with injected time; no `Instant::now()` in logic under test.
- Settings round-trip test mirroring the existing macOS-group tests in
  `settings.rs`.
- UI: menu bar card mount test (chips toggle -> patch payload; preview text).

## 3. Acceptance criteria

1. On macOS, during a sync, the menu bar shows the configured metrics next to
   the Driven icon, updating ~1 Hz, defaulting to "62% · 84 Mbps" style; idle
   behaviour follows the configured mode; disabling everything clears the
   title.
2. Settings window shows the sidebar IA; every setting reachable in the old
   UI remains reachable and functional; old routes redirect.
3. About contains no settings; telemetry appears exactly once (Privacy &
   Data); update channel lives in General.
4. Platform page shows macOS content on macOS, Windows content on Windows,
   absent on Linux.
5. `cargo test`, `pnpm -C ui test`, and the UI coverage gate pass; no
   regression in existing settings behaviour (clamping, error banner, VSS/APFS
   status polling).

## 4. Implementation phasing (separate PRs)

1. `feat(ui): menu bar extra settings + tray title engine` - Rust schema +
   tray task + macOS card rendered inside the *current* Rules tab (small,
   ships the headline feature independently).
2. `feat(ui): settings sidebar IA` - layout shell, routing/redirects, section
   moves, About cleanup.
3. `feat(ui): accounts and sources redesign` - row/table redesign per mock E.

Each lands green through the normal `/d -mp` flow; PR titles conventional.
