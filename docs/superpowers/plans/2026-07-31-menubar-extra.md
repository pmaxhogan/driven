# macOS Menu Bar Extra Implementation Plan (PR 1 of settings redesign)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Live, configurable text next to the Driven tray icon on macOS (upload speed / % / files / ETA while syncing; configurable idle text), driven by a new `macos.menu_bar` settings sub-group.

**Architecture:** A new `src-tauri/src/menubar.rs` module holds (a) pure, unit-tested config/formatting/rate code compiled on all platforms and (b) a 1 Hz engine task (spawned on macOS only) that aggregates per-account progress recorded by the existing event bridge and calls `TrayIcon::set_title`. Settings extend the existing SPEC s22 `macos` KV group end to end (storage struct, DTO, patch, merge arm, UI card in the current Rules tab).

**Tech Stack:** Rust (tauri 2.11.5, tokio), Vue 3 + Pinia + vitest, rust_i18n locales.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-31-settings-redesign-menubar-design.md` s2.
- No `driven-core` changes; nothing in `crates/` may depend on `src-tauri`.
- Do NOT consume `ThroughputProbe::take_bytes()` (owned by `AdaptiveController`).
- ASCII `-` only - no em/en dashes anywhere. LF line endings.
- Comments: match the repo's density and style (explain constraints/why, cite SPEC/DESIGN sections where neighboring code does).
- Wire enum for idle mode: exactly `"none" | "lastBackupAge" | "uploadedToday"`.
- Defaults: speed ON, percent ON, files OFF, eta OFF, idle `"none"`.
- Metric order in the title: percent, speed, files, ETA, joined by `" · "`.
- Rust tests: `cargo test --workspace`. UI tests: `pnpm --dir ui test:unit`.
- Commit after each green step; conventional commit subjects (branch commits are squashed away, but keep them tidy).
- Work on branch `feat/settings-redesign` (already exists, has the spec commit).

---

### Task 1: Settings schema - `menu_bar` sub-group in the `macos` KV group

**Files:**
- Modify: `src-tauri/src/commands/dtos.rs` (~line 820, `MacosSettings`; ~line 1052, `MacosSettingsPatch`)
- Modify: `src-tauri/src/commands/settings.rs` (~line 1253 `storage::Macos`, ~line 2879 `default_macos`)
- Test: inline `#[cfg(test)]` in `src-tauri/src/commands/settings.rs` (append near the existing macos-group tests around line 3267)

**Interfaces:**
- Produces: `dtos::MenuBarSettings { show_upload_speed: bool, show_percent: bool, show_files: bool, show_eta: bool, idle: String }` (camelCase on the wire), `dtos::MenuBarSettingsPatch` (all `Option`), field `MacosSettings::menu_bar`, field `MacosSettingsPatch::menu_bar: Option<MenuBarSettingsPatch>`, `Default for MenuBarSettings`.
- Consumes: existing `storage::Macos`, `load_group`/`store_group`, `default_macos()`.

- [ ] **Step 1: Write the failing round-trip test**

In the existing `#[cfg(test)]` module of `settings.rs` (same style as the surrounding macos tests - they build an in-memory repo; mirror the neighbouring test's setup exactly):

```rust
/// SPEC s22 `macos.menu_bar`: an unseeded group reads as the documented
/// defaults (speed+percent on, files/eta off, idle "none"), and a stored
/// group round-trips through the storage form unchanged.
#[test]
fn menu_bar_defaults_and_storage_roundtrip() {
    let d = default_macos();
    assert!(d.menu_bar.show_upload_speed);
    assert!(d.menu_bar.show_percent);
    assert!(!d.menu_bar.show_files);
    assert!(!d.menu_bar.show_eta);
    assert_eq!(d.menu_bar.idle, "none");

    // storage round-trip preserves every field
    let mut dto = default_macos();
    dto.menu_bar.show_files = true;
    dto.menu_bar.idle = "uploadedToday".to_string();
    let stored = storage::Macos::from(dto.clone());
    let back: MacosSettings = stored.into();
    assert!(back.menu_bar.show_files);
    assert_eq!(back.menu_bar.idle, "uploadedToday");

    // a legacy on-disk blob without `menu_bar` still deserialises (serde default)
    let legacy: storage::Macos =
        serde_json::from_value(serde_json::json!({ "apfs_snapshot": true })).unwrap();
    let dto: MacosSettings = legacy.into();
    assert!(dto.apfs_snapshot);
    assert_eq!(dto.menu_bar.idle, "none");
}
```

- [ ] **Step 2: Run it, verify it fails to compile** (`menu_bar` field does not exist)

Run: `cargo test -p driven-app menu_bar_defaults_and_storage_roundtrip` (the src-tauri crate; if the package name differs check `src-tauri/Cargo.toml [package].name` and use that)
Expected: compile error, `no field menu_bar`.

- [ ] **Step 3: Add the DTO types** in `dtos.rs`, directly below `MacosSettings`:

```rust
/// SPEC s22 `macos.menu_bar`: the macOS menu bar extra (live tray title).
/// Which metrics render while a backup runs, and what shows when idle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MenuBarSettings {
    /// "84 Mbps" - last-second upload bandwidth, speedtest-style bit units.
    pub show_upload_speed: bool,
    /// "62%" - bytes_done/bytes_total across all running syncs.
    pub show_percent: bool,
    /// "341/2.1k" - files done/total, compact counts.
    pub show_files: bool,
    /// "~4m" - remaining bytes over the smoothed rate.
    pub show_eta: bool,
    /// Idle title mode: `none` (icon only) | `lastBackupAge` ("2h") |
    /// `uploadedToday` ("1.2 GB today").
    pub idle: String,
}

impl Default for MenuBarSettings {
    fn default() -> Self {
        MenuBarSettings {
            show_upload_speed: true,
            show_percent: true,
            show_files: false,
            show_eta: false,
            idle: "none".to_string(),
        }
    }
}

/// Partial [`MenuBarSettings`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MenuBarSettingsPatch {
    pub show_upload_speed: Option<bool>,
    pub show_percent: Option<bool>,
    pub show_files: Option<bool>,
    pub show_eta: Option<bool>,
    pub idle: Option<String>,
}
```

Then add to `MacosSettings` (with `#[serde(default)]` so pre-existing wire blobs stay valid) and to `MacosSettingsPatch`:

```rust
    /// Menu bar extra configuration (spec 2026-07-31 s2).
    #[serde(default)]
    pub menu_bar: MenuBarSettings,
```

```rust
    /// See [`MacosSettings::menu_bar`].
    pub menu_bar: Option<MenuBarSettingsPatch>,
```

- [ ] **Step 4: Mirror in `storage::Macos`** (settings.rs storage mod; snake_case on disk):

```rust
    /// `snake_case` on-disk form of `macos.menu_bar`. Absent in DBs written
    /// before the menu bar extra landed; defaults keep older rows valid.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct MenuBar {
        pub show_upload_speed: bool,
        pub show_percent: bool,
        pub show_files: bool,
        pub show_eta: bool,
        pub idle: String,
    }

    impl Default for MenuBar {
        fn default() -> Self {
            MenuBarSettings::default().into()
        }
    }

    impl From<MenuBar> for MenuBarSettings { /* field-by-field */ }
    impl From<MenuBarSettings> for MenuBar { /* field-by-field */ }
```

Write the two `From` impls out in full (five fields each; no shortcuts). Add to `storage::Macos`:

```rust
        #[serde(default)]
        pub menu_bar: MenuBar,
```

and thread the field through the existing `From<Macos> for MacosSettings` / `From<MacosSettings> for Macos` impls (`menu_bar: s.menu_bar.into()`). Import `MenuBarSettings` in the storage `use` list. `default_macos()` needs `menu_bar: MenuBarSettings::default()`.

- [ ] **Step 5: Run the test, verify pass:** `cargo test -p driven-app menu_bar_defaults_and_storage_roundtrip` -> PASS. Also `cargo test -p driven-app` (no regressions in the settings suite).

- [ ] **Step 6: Commit** - `git add -A && git commit -m "feat: macos.menu_bar settings schema"`

---

### Task 2: `update_settings` merge arm + idle-enum validation

**Files:**
- Modify: `src-tauri/src/commands/settings.rs` (macos branch of `update_settings`, ~line 678-703)
- Test: same inline test module

**Interfaces:**
- Consumes: Task 1 types.
- Produces: patching `{ macos: { menuBar: {...} } }` persists and round-trips through `update_settings`'s returned `SettingsDto`; invalid `idle` rejected with `ErrorCode::InvalidInput`; a local `menubar_changed: bool` side-effect flag exists in `update_settings` (Task 5 hooks it).

- [ ] **Step 1: Write the failing tests** (same module; follow the neighbouring update_settings tests' repo/State setup exactly - they construct the command's `State` from an in-memory repo):

```rust
/// A menuBar patch merges field-wise (untouched fields keep their values)
/// and the authoritative returned DTO reflects the store.
#[tokio::test]
async fn update_settings_merges_menu_bar_patch() { /* build state as neighbours do */
    // patch: macos.menu_bar { show_files: Some(true), idle: Some("lastBackupAge") }
    // assert returned dto.macos (on macOS builds) has show_files true,
    // show_upload_speed still true (default preserved), idle "lastBackupAge".
    // NB: `SettingsDto.macos` is None off-macOS; read the group back via
    // load_group::<storage::Macos> instead so the test is platform-neutral.
}

/// An unknown idle mode is rejected as invalid input and nothing persists.
#[tokio::test]
async fn update_settings_rejects_bad_menu_bar_idle() {
    // patch idle: Some("sometimes") -> Err with ErrorCode::InvalidInput;
    // load_group afterwards still returns the default "none".
}
```

Fill the bodies concretely by copying the setup lines of the nearest existing `update_settings` test in the file (do not invent a new harness).

- [ ] **Step 2: Run, verify both fail** (patch field compiles after Task 1 but the merge arm ignores `menu_bar`, so the first test's assertion fails and the second returns Ok).

- [ ] **Step 3: Implement the merge arm.** In the `if let Some(m) = patch.macos` block, after the `apfs_snapshot` handling, before `store_group`:

```rust
        if let Some(mb) = m.menu_bar {
            // Menu bar extra (spec 2026-07-31 s2). Persist on every host (same
            // rationale as apfs_snapshot above); only the macOS tray reads it.
            if let Some(v) = mb.idle {
                const IDLE_MODES: [&str; 3] = ["none", "lastBackupAge", "uploadedToday"];
                if !IDLE_MODES.contains(&v.as_str()) {
                    return Err(CommandError::with_code(
                        ErrorCode::InvalidInput,
                        format!("macos.menuBar.idle must be one of {IDLE_MODES:?}, got {v:?}"),
                    ));
                }
                cur.menu_bar.idle = v;
            }
            if let Some(v) = mb.show_upload_speed {
                cur.menu_bar.show_upload_speed = v;
            }
            if let Some(v) = mb.show_percent {
                cur.menu_bar.show_percent = v;
            }
            if let Some(v) = mb.show_files {
                cur.menu_bar.show_files = v;
            }
            if let Some(v) = mb.show_eta {
                cur.menu_bar.show_eta = v;
            }
            menubar_changed = true;
        }
```

Declare `let mut menubar_changed = false;` next to the other side-effect flags (~line 387) with a one-line comment; note the validation must run BEFORE any field is written (reject-then-persist-nothing, matching the command's existing all-or-nothing behaviour for invalid input - the arm returns early before `store_group`). Add `#[allow(unused_variables)]`? No - Task 5 consumes the flag; until then silence the lint by underscore-reading it once at the end of the function:

```rust
    // Task 5 (menubar engine) will react to this; keep the flag observable now
    // so the merge arm stays honest about having a side effect.
    let _ = menubar_changed;
```

- [ ] **Step 4: Run, verify pass:** both new tests + full `cargo test -p driven-app`.

- [ ] **Step 5: Commit** - `git commit -am "feat: update_settings merge arm for macos.menu_bar"`

---

### Task 3: `menubar.rs` pure core - config + formatters (TDD)

**Files:**
- Create: `src-tauri/src/menubar.rs`
- Modify: `src-tauri/src/lib.rs` (add `mod menubar;` next to `mod tray;`)
- Test: `#[cfg(test)]` module inside `menubar.rs`

**Interfaces:**
- Consumes: `crate::commands::dtos::MenuBarSettings`.
- Produces (exact signatures Tasks 4-6 use):
  - `pub enum IdleMode { None, LastBackupAge, UploadedToday }`
  - `pub struct MenuBarConfig { pub speed: bool, pub percent: bool, pub files: bool, pub eta: bool, pub idle: IdleMode }`
  - `impl MenuBarConfig { pub fn from_settings(s: &MenuBarSettings) -> Self }` (unknown idle string -> `IdleMode::None`)
  - `pub fn format_speed_bits(bps: f64) -> String` - "84 Mbps" style
  - `pub fn format_compact_count(n: u64) -> String` - "341", "2.1k", "3.4M"
  - `pub fn format_eta(secs: u64) -> String` - "~40s", "~4m", "~1h 20m"
  - `pub fn format_percent(done: u64, total: u64) -> Option<String>` - `None` when `total == 0`
  - `pub struct TitleMetrics { pub bytes_done: u64, pub bytes_total: u64, pub files_done: u64, pub files_total: u64, pub rate_bps: Option<f64>, pub eta_secs: Option<u64> }`
  - `pub fn format_title(cfg: &MenuBarConfig, m: &TitleMetrics) -> Option<String>` - `None` when nothing is enabled or nothing renders

- [ ] **Step 1: Write the failing tests** (all pure; no platform gates):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_scales_bit_units_with_3_sig_digits() {
        assert_eq!(format_speed_bits(0.0), "0 bps");
        assert_eq!(format_speed_bits(999.0), "999 bps");
        assert_eq!(format_speed_bits(84_200.0), "84.2 kbps");
        assert_eq!(format_speed_bits(84_000_000.0), "84 Mbps");
        assert_eq!(format_speed_bits(1_240_000_000.0), "1.24 Gbps");
    }

    #[test]
    fn compact_counts() {
        assert_eq!(format_compact_count(341), "341");
        assert_eq!(format_compact_count(2_148), "2.1k");
        assert_eq!(format_compact_count(3_400_000), "3.4M");
    }

    #[test]
    fn eta_formats_by_magnitude() {
        assert_eq!(format_eta(40), "~40s");
        assert_eq!(format_eta(4 * 60), "~4m");
        assert_eq!(format_eta(60 * 60 + 20 * 60), "~1h 20m");
    }

    #[test]
    fn title_joins_enabled_metrics_in_spec_order() {
        let cfg = MenuBarConfig { speed: true, percent: true, files: false, eta: false, idle: IdleMode::None };
        let m = TitleMetrics {
            bytes_done: 62, bytes_total: 100,
            files_done: 341, files_total: 2148,
            rate_bps: Some(84_000_000.0), eta_secs: Some(240),
        };
        assert_eq!(format_title(&cfg, &m).unwrap(), "62% · 84 Mbps");
        let all = MenuBarConfig { speed: true, percent: true, files: true, eta: true, idle: IdleMode::None };
        assert_eq!(format_title(&all, &m).unwrap(), "62% · 84 Mbps · 341/2.1k · ~4m");
        let none = MenuBarConfig { speed: false, percent: false, files: false, eta: false, idle: IdleMode::None };
        assert_eq!(format_title(&none, &m), None);
    }

    #[test]
    fn title_omits_unavailable_metrics() {
        // no totals yet (scan phase): percent+eta render nothing; speed absent
        // until a rate sample exists -> whole title is None
        let cfg = MenuBarConfig { speed: true, percent: true, files: false, eta: true, idle: IdleMode::None };
        let m = TitleMetrics { bytes_done: 0, bytes_total: 0, files_done: 0, files_total: 0, rate_bps: None, eta_secs: None };
        assert_eq!(format_title(&cfg, &m), None);
    }

    #[test]
    fn config_from_settings_maps_idle_and_tolerates_unknown() {
        use crate::commands::dtos::MenuBarSettings;
        let mut s = MenuBarSettings::default();
        s.idle = "uploadedToday".into();
        assert!(matches!(MenuBarConfig::from_settings(&s).idle, IdleMode::UploadedToday));
        s.idle = "garbage".into();
        assert!(matches!(MenuBarConfig::from_settings(&s).idle, IdleMode::None));
    }
}
```

- [ ] **Step 2: Run, verify compile failure** (`cargo test -p driven-app menubar`).

- [ ] **Step 3: Implement.** Module doc comment explaining the feature + spec pointer. Formatting rules:
  - `format_speed_bits`: units `bps`/`kbps`/`Mbps`/`Gbps`, thresholds at 1000; render with 3 significant digits (`{:.0}` >= 100, `{:.1}` >= 10, `{:.2}` < 10), trim a trailing `.0`/`.00` (so 84.0 -> "84").
  - `format_compact_count`: < 1000 verbatim; < 1_000_000 -> `{:.1}k` trimming `.0`; else `{:.1}M` trimming `.0`.
  - `format_eta`: < 60 -> `~{s}s`; < 3600 -> `~{m}m` (round to nearest minute); else `~{h}h {m}m` (omit ` {m}m` when 0).
  - `format_percent`: `None` if `total == 0`, else `Some(format!("{}%", done * 100 / total))` (integer, floor).
  - `format_title`: build `Vec<String>` in order percent -> speed -> files (needs `files_total > 0`) -> eta, join `" · "`, `None` if empty.

- [ ] **Step 4: Run, verify pass**, plus `cargo clippy -p driven-app --all-targets -- -D warnings` clean for the new file.

- [ ] **Step 5: Commit** - `git commit -am "feat: menubar pure formatting core"`

---

### Task 4: Rate estimation + cross-account aggregation (TDD, injected time)

**Files:**
- Modify: `src-tauri/src/menubar.rs`
- Test: same inline module

**Interfaces:**
- Consumes: Task 3 types.
- Produces:
  - `pub struct RateEstimator { /* private */ }` with `pub fn new() -> Self` and `pub fn sample(&mut self, total_bytes: u64, now: std::time::Instant) -> Option<f64>` - returns the EMA-smoothed bytes/sec AFTER >= `MIN_SAMPLE_WINDOW` (2 s) of history; a `total_bytes` DECREASE (new sync cycle) resets the estimator; smoothing time-constant `EMA_TAU_SECS = 3.0`.
  - `pub fn eta_secs(rate_bps_bytes: f64, remaining_bytes: u64) -> Option<u64>` - `None` until `rate >= 1.0` byte/s; caps at 99h to avoid absurd strings.
  - `#[derive(Clone, Copy, Default)] pub struct AccountProgress { pub bytes_done: u64, pub bytes_total: u64, pub files_done: u64, pub files_total: u64, pub active: bool }`
  - `pub fn aggregate(accounts: impl Iterator<Item = AccountProgress>) -> AccountProgress` - sums the four counters over ACTIVE accounts only; `active` = any active.

- [ ] **Step 1: Failing tests** (drive `sample()` with a hand-built `Instant` sequence - `Instant::now()` once + `Duration` additions, so no sleeping and no wall-clock flake):

```rust
    #[test]
    fn rate_needs_a_window_then_smooths() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut r = RateEstimator::new();
        assert_eq!(r.sample(0, t0), None); // first sample: no interval yet
        assert_eq!(r.sample(1_000_000, t0 + Duration::from_millis(1000)), None); // < 2s window
        let v = r.sample(2_000_000, t0 + Duration::from_millis(2000)).unwrap();
        assert!((v - 1_000_000.0).abs() < 50_000.0, "steady 1 MB/s, got {v}");
        // a burst decays toward the new rate rather than jumping (EMA)
        let v2 = r.sample(12_000_000, t0 + Duration::from_millis(3000)).unwrap();
        assert!(v2 > 1_000_000.0 && v2 < 10_000_000.0, "smoothed, got {v2}");
    }

    #[test]
    fn rate_resets_on_counter_regression() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut r = RateEstimator::new();
        r.sample(5_000_000, t0);
        r.sample(6_000_000, t0 + Duration::from_secs(1));
        // new sync cycle: totals restart from 0 - estimator must not compute a
        // negative delta or a bogus huge rate
        assert_eq!(r.sample(0, t0 + Duration::from_secs(2)), None);
    }

    #[test]
    fn aggregate_sums_only_active_accounts() {
        let a = AccountProgress { bytes_done: 10, bytes_total: 100, files_done: 1, files_total: 2, active: true };
        let b = AccountProgress { bytes_done: 90, bytes_total: 900, files_done: 3, files_total: 4, active: false };
        let agg = aggregate([a, b].into_iter());
        assert_eq!(agg.bytes_done, 10);
        assert_eq!(agg.bytes_total, 100);
        assert!(agg.active);
    }

    #[test]
    fn eta_requires_a_real_rate() {
        assert_eq!(eta_secs(0.5, 1_000), None);
        assert_eq!(eta_secs(1_000.0, 4_000), Some(4));
    }
```

- [ ] **Step 2: Run, verify failure.**
- [ ] **Step 3: Implement.** `RateEstimator` keeps `last: Option<(u64, Instant)>`, `first_at: Option<Instant>`, `ema: Option<f64>`. On each sample: regression (`total < last.0`) -> full reset (return `None`); else instantaneous rate = `delta_bytes / dt`; `alpha = (dt / EMA_TAU_SECS).clamp(0.0, 1.0)`; `ema = ema * (1 - alpha) + inst * alpha` (seed with `inst` on first interval); return `Some(ema)` only once `now - first_at >= MIN_SAMPLE_WINDOW`. Guard `dt <= 0` (return current state unchanged).
- [ ] **Step 4: Run, verify pass** + clippy clean.
- [ ] **Step 5: Commit** - `git commit -am "feat: menubar rate estimator and aggregation"`

---

### Task 5: Engine - shared state, bridge wiring, 1 Hz title task, settings hook

**Files:**
- Modify: `src-tauri/src/menubar.rs` (engine section)
- Modify: `src-tauri/src/assembly.rs` (`SourceProgress` arm ~line 1218, `SyncStatus` arm ~line 1142)
- Modify: `src-tauri/src/lib.rs` (spawn after `tray::build(&handle)?` at ~line 485)
- Modify: `src-tauri/src/commands/settings.rs` (replace the `let _ = menubar_changed;` stub)
- Modify: `src-tauri/Cargo.toml` (add `chrono = { version = "0.4", default-features = false, features = ["std", "clock"] }`)
- Test: engine decision logic stays in pure helpers tested inline; the spawned task itself is thin glue.

**Interfaces:**
- Consumes: Tasks 3-4; `tray::TRAY_ID` (make the const `pub(crate)` if it is private); `AppState` (`app.state::<AppState>()`), `StateRepo::list_accounts()` (`last_synced_at: Option<i64>` on rows) and `StateRepo::activity_summary(day_start_ms, week_start_ms, window_start, throughput_window_ms)` (returns a summary with `bytes_today: u64`) - mirror the exact call in `commands/activity.rs:265-274`.
- Produces:
  - `pub fn record_progress(account_id: AccountId, p: &driven_core::types::ExecProgress)`
  - `pub fn note_state(account_id: AccountId, state: &driven_core::types::OrchestratorState)` - active iff `matches!(state, Scanning { .. } | Planning { .. } | Executing { .. })`; on `Idle`/`Paused`/error the account's entry flips `active: false` (keep counters); on `Idle` also remove the entry.
  - `pub fn start(app: &tauri::AppHandle)` - no-op unless `cfg!(target_os = "macos")`.
  - `pub fn config_changed(app: &tauri::AppHandle)` - reloads config into the engine and pokes an immediate refresh.

- [ ] **Step 1: Implement the shared state + recorders** (no test first - these are two-line mutex writes; the pure logic is already covered):

```rust
static METRICS: LazyLock<Mutex<HashMap<AccountId, AccountProgress>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Config cache, loaded at start() and on config_changed(); read each tick.
static CONFIG: LazyLock<Mutex<MenuBarConfig>> =
    LazyLock::new(|| Mutex::new(MenuBarConfig::from_settings(&MenuBarSettings::default())));
/// Bumped by config_changed() so the engine re-renders immediately-ish
/// (next 1 s tick) and idle caches invalidate.
static CONFIG_GEN: AtomicU64 = AtomicU64::new(0);
```

`record_progress` upserts the entry (sets the four counters, `active: true`). `note_state` per the interface above. Recover poisoned locks with `unwrap_or_else(|e| e.into_inner())` (house style, see tray.rs).

- [ ] **Step 2: Wire the bridge.** In `assembly.rs`:
  - `SourceProgress` arm, after building `payload`: `crate::menubar::record_progress(account_id, &progress);` - place it BEFORE `progress` is moved into the payload (it takes a reference; the payload construction moves `progress`, so call first).
  - `SyncStatus` arm, next to `tray::apply_state`: `crate::menubar::note_state(account_id, &state);` (before `state` moves into the payload).

- [ ] **Step 3: Implement the engine task.** In `menubar.rs`:

```rust
pub fn start(app: &AppHandle) {
    if !cfg!(target_os = "macos") {
        return; // tray titles only render on macOS (spec s2)
    }
    load_config_from_store(app); // async read of the macos group; spawn it
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut rate = RateEstimator::new();
        let mut idle = IdleCache::default();
        loop {
            ticker.tick().await;
            tick(&app, &mut rate, &mut idle).await;
        }
    });
}
```

`tick()` (a free async fn, logic split into pure helpers where possible):
1. Snapshot `CONFIG` and the `aggregate(METRICS)` under their locks (never `.await` while holding either - copy out, then drop the guards).
2. If `agg.active`: `rate_bps = rate.sample(agg.bytes_done, Instant::now())`; `eta = rate_bps.and_then(|r| eta_secs(r, agg.bytes_total.saturating_sub(agg.bytes_done)))` when `cfg.eta`; convert the byte rate to bits (`* 8.0`) for display; `title = format_title(&cfg, &metrics)`.
3. Else: `rate = RateEstimator::new()` (reset for the next cycle) and `title = idle.title(&app, &cfg).await` (below).
4. `let Some(tray) = app.tray_by_id(tray::TRAY_ID) else { return }; let _ = tray.set_title(title.as_deref());` - `set_title(None)` clears. Only call when the string actually changed since the last tick (cache the last value in the task) so the OS is not re-painted at 1 Hz while idle.

`IdleCache`: `{ value: Option<String>, fetched_at: Option<Instant>, gen: u64 }` - re-fetches when older than 60 s, when `CONFIG_GEN` changed, or when never fetched:
- `IdleMode::None` -> `None`.
- `IdleMode::LastBackupAge` -> `app.state::<AppState>().state().list_accounts().await` -> max of `last_synced_at` -> `Some(format_age(now_ms - ts))` where `pub fn format_age(ms: i64) -> String` renders "5m" / "2h" / "3d" (add it in this task with three inline unit tests, same rounding rules as `format_eta` without the `~`); `None` if no account ever synced.
- `IdleMode::UploadedToday` -> local midnight via `chrono::Local::now().date_naive().and_hms_opt(0,0,0)` -> ms -> `state.activity_summary(day_start_ms, day_start_ms, day_start_ms, 1).await` -> `Some(format!("{} today", format_bytes(b)))` using a new `pub fn format_bytes(n: u64) -> String` ("1.2 GB", "840 MB", "12 kB" - decimal units, 2 sig digits; three inline unit tests). On any repo error: log at `debug` and render `None` (icon only) - never a stale or panicking title.
5. Add `use tauri::Manager;` if not already imported (needed for `app.state`).

- [ ] **Step 4: Hook settings + startup.**
  - `lib.rs` setup, directly after `tray::build(&handle)?;`: `menubar::start(&handle);`
  - `settings.rs`: replace `let _ = menubar_changed;` with

```rust
    if menubar_changed {
        // Re-render the menu bar extra with the new config on the next tick
        // (spec s2: settings apply immediately, no restart).
        crate::menubar::config_changed(&app);
    }
```

  `config_changed` re-runs `load_config_from_store(app)` and bumps `CONFIG_GEN`. `load_config_from_store` reads via the same `load_group::<storage::Macos>` used by `get_settings` - expose a small `pub(crate) async fn load_menubar_settings(state: &dyn StateRepo) -> MenuBarSettings` next to `load_apfs_snapshot_enabled` (settings.rs ~line 211) and call that, so the storage structs stay private to settings.rs.

- [ ] **Step 5: Add the chrono dependency** to `src-tauri/Cargo.toml` as listed above; run `cargo deny check licenses` if `deny.toml` gates it (chrono is MIT/Apache-2.0 - passes).

- [ ] **Step 6: Verify green:** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`. On this Mac also smoke it live: `cargo tauri dev`, run a sync on a seeded fixture (`just dev-seeded` in a second terminal is NOT needed - use the normal dev app with the fake remote: `DRIVEN_USE_FAKE_REMOTE=1 cargo tauri dev`), and confirm the title appears next to the tray icon while syncing and clears when idle.

- [ ] **Step 7: Commit** - `git commit -am "feat: menu bar title engine wired to event bridge"`

---

### Task 6: Settings UI - menu bar card (current Rules tab, macOS section)

**Files:**
- Modify: `ui/src/ipc/types.ts` (`MacosSettings` ~line 407, `MacosSettingsPatch` ~line 551)
- Modify: `ui/src/views/Settings.vue` (macOS block inside the performance section, ~line 1032)
- Modify: `ui/src/locales/en-US.json` (settings.rules.* additions)
- Test: `ui/src/__tests__/settings-components.test.ts` (append)

**Interfaces:**
- Consumes: Task 1 wire shape; `useSettingsStore.patch()`.
- Produces: UI controls that emit `settings.patch({ macos: { menuBar: { ... } } })`.

- [ ] **Step 1: Extend the TS types:**

```ts
export interface MenuBarSettings {
  showUploadSpeed: boolean;
  showPercent: boolean;
  showFiles: boolean;
  showEta: boolean;
  /** "none" | "lastBackupAge" | "uploadedToday" */
  idle: string;
}

export interface MacosSettings {
  apfsSnapshot: boolean;
  /** Menu bar extra configuration (spec 2026-07-31 s2). */
  menuBar: MenuBarSettings;
}

export interface MenuBarSettingsPatch {
  showUploadSpeed?: boolean;
  showPercent?: boolean;
  showFiles?: boolean;
  showEta?: boolean;
  idle?: string;
}
// MacosSettingsPatch gains: menuBar?: MenuBarSettingsPatch;
```

- [ ] **Step 2: Write the failing component test** (append to `settings-components.test.ts`; copy the file's existing mount/stub pattern for Settings.vue with a seeded settings store - the fixture's `macos` object needs the new `menuBar` field added wherever the suite builds a `SettingsDto`):

```ts
it("menu bar card toggles a metric via a macos.menuBar patch", async () => {
  // seed macos: { apfsSnapshot: false, menuBar: { showUploadSpeed: true,
  //   showPercent: true, showFiles: false, showEta: false, idle: "none" } }
  // find [data-testid="menubar-files-toggle"], set checked, trigger change
  // assert updateSettings was called with { macos: { menuBar: { showFiles: true } } }
});

it("menu bar idle select patches macos.menuBar.idle", async () => {
  // set [data-testid="menubar-idle-select"] to "uploadedToday"
  // assert patch payload { macos: { menuBar: { idle: "uploadedToday" } } }
});

it("menu bar preview reflects enabled metrics", async () => {
  // [data-testid="menubar-preview"] text contains "62%" and "84 Mbps" when
  // speed+percent are on; turning showPercent off in the seeded settings
  // renders a preview without "62%"
});
```

- [ ] **Step 3: Run, verify fail:** `pnpm --dir ui test:unit -- settings-components`

- [ ] **Step 4: Implement the card.** Inside the existing `v-if="settings.settings.macos"` region of the Rules form, add a new sibling `<section :class="cardCls" data-testid="menubar-setting">` with:
  - `<h3>{{ t("settings.rules.sections.menuBar") }}</h3>` + explainer `<p>`.
  - Static preview strip `data-testid="menubar-preview"`: a small dark pill rendering `sampleTitle` - a computed that formats the constant sample metrics (62%, 84 Mbps, 341/2.1k, ~4m) honouring the four toggles in their spec order. Keep the sample values as constants; this is a preview, not live data.
  - Four checkboxes bound to `settings.settings.macos.menuBar.*`, each `@change` calling a `patchMenuBar(field, value)` helper that does `settings.patch({ macos: { menuBar: { [field]: value } } })` with the same error-swallowing pattern as the neighbouring toggles; `data-testid`s: `menubar-speed-toggle`, `menubar-percent-toggle`, `menubar-files-toggle`, `menubar-eta-toggle`.
  - A width hint `<p class="text-xs ...">` shown when 3+ metrics are enabled: `t("settings.rules.menuBarWidthHint")`.
  - Idle `<select data-testid="menubar-idle-select">` with the three options, bound + patched the same way.
- [ ] **Step 5: Locale strings** in `ui/src/locales/en-US.json` under `settings.rules`:

```json
"sections": { "menuBar": "Menu bar" },
"menuBarIntro": "Show live backup info next to the Driven icon in the menu bar.",
"menuBarShowLabel": "While backing up, show",
"menuBarSpeed": "Upload speed",
"menuBarPercent": "Percent complete",
"menuBarFiles": "Files",
"menuBarEta": "Time remaining",
"menuBarWidthHint": "More than two metrics makes the menu bar item wide.",
"menuBarIdleLabel": "When idle, show",
"menuBarIdleNone": "Nothing (icon only)",
"menuBarIdleLastBackup": "Time since last backup",
"menuBarIdleUploadedToday": "Uploaded today"
```

(merge into the existing `sections` object rather than duplicating the key).

- [ ] **Step 6: Run, verify pass:** `pnpm --dir ui test:unit` full suite + `pnpm --dir ui exec vue-tsc --noEmit` if the repo runs a typecheck script (check `ui/package.json` scripts; run whatever `check`/`typecheck` script exists).

- [ ] **Step 7: Commit** - `git commit -am "feat(ui): menu bar extra settings card"`

---

### Task 7: Tray menu status header rows

**Files:**
- Modify: `src-tauri/src/tray.rs` (`build_menu` ~line 1025, `mod menu_id` ~line 109)
- Modify: `src-tauri/src/menubar.rs` (engine tick)
- Modify: `src-tauri/locales/en-US.yml` (`tray:` block)
- Test: pure text-derivation helper tested inline in `menubar.rs`

**Interfaces:**
- Consumes: Task 5 aggregate + formatters.
- Produces: `pub fn menu_status_lines(agg: &AccountProgress, rate_bps_bits: Option<f64>, eta_secs: Option<u64>) -> Option<(String, String)>` in `menubar.rs`; two disabled `MenuItem`s registered by `tray::build_menu` and exposed via `pub(crate) fn status_menu_items() -> Option<(MenuItem<Wry>, MenuItem<Wry>)>`.

- [ ] **Step 1: Failing test** for the pure helper:

```rust
    #[test]
    fn menu_status_lines_render_while_active_only() {
        let idle = AccountProgress::default();
        assert_eq!(menu_status_lines(&idle, None, None), None);
        let agg = AccountProgress { bytes_done: 62, bytes_total: 100, files_done: 341, files_total: 2148, active: true };
        let (l1, l2) = menu_status_lines(&agg, Some(84_000_000.0), Some(240)).unwrap();
        assert_eq!(l1, "Backing up - 62%, ~4m left");
        assert_eq!(l2, "84 Mbps · 341 of 2,148 files");
    }
```

(Thousands separator: format `files_total` with a small local helper - insert `,` every three digits; no new dependency. Localised words come from `rust_i18n::t!` keys - in the TEST compare against the en-US strings above.)

- [ ] **Step 2: Run, verify fail.**
- [ ] **Step 3: Implement.** `menu_status_lines`: `None` unless `agg.active`; line 1 = `t!("tray.menu_status.line1", percent, eta)` shaped "Backing up - {percent}, {eta} left" (omit ", {eta} left" when `eta` is `None`); line 2 = "{speed} · {done} of {total} files" (omit the speed segment when `None`). Add `tray.menu_status.line1/line2` keys to `src-tauri/locales/en-US.yml`.
- [ ] **Step 4: Register the rows.** In `tray.rs` `build_menu`, prepend two `MenuItem`s (ids `menu_id::STATUS_1`, `menu_id::STATUS_2`) built with `.enabled(false)` and empty initial text, stash their handles in a `static STATUS_ITEMS: Mutex<Option<(MenuItem<Wry>, MenuItem<Wry>)>>` (cleared + re-set on `rebuild`), expose `status_menu_items()`. In the menubar engine tick, when the derived lines change, call `item.set_text(..)` on both (empty string when `None` - tauri renders an empty disabled row; if that looks bad in the live smoke test, instead call `item.set_enabled(false)` and text " " - judgement call at smoke time, note the choice in a comment). `on_menu_event` ignores the two ids (disabled items do not fire, no arm needed).
- [ ] **Step 5: Run tests + clippy; smoke live** (`DRIVEN_USE_FAKE_REMOTE=1 cargo tauri dev`, open the tray menu mid-sync).
- [ ] **Step 6: Commit** - `git commit -am "feat: live status header rows in the tray menu"`

---

### Task 8: Full-suite green + PR

- [ ] **Step 1:** `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check && pnpm --dir ui test:unit`
- [ ] **Step 2:** Re-read the diff (`git diff main...HEAD`) for: em/en dashes (fix to `-`), stray `todo!`/`unimplemented!`, comment style drift, `console.log`.
- [ ] **Step 3:** Push and open the PR with a conventional title: `feat(ui): configurable macOS menu bar extra with live backup metrics`. Body: summary, spec link, test evidence, note that the settings UI placement is temporary pending the sidebar IA PR. (Handled by the orchestrating session via /d -pm, not by a task subagent.)
