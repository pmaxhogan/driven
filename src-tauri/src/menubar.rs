//! macOS menu bar extra: pure formatting core + the 1 Hz title engine
//! (SPEC s22, DESIGN s2
//! `docs/superpowers/specs/2026-07-31-settings-redesign-menubar-design.md`).
//!
//! The first half of this file is pure and platform-independent - no
//! `Instant::now()`, no macOS APIs, no tray access - so it compiles and is
//! unit-tested on all three CI targets, matching DESIGN s2 "Testing": pure
//! functions unit-tested with injected time, no live clock in the logic
//! under test. [`MenuBarConfig`] mirrors the
//! [`crate::commands::dtos::MenuBarSettings`] wire DTO in enum form; the
//! `format_*` functions turn a live [`TitleMetrics`] snapshot into the
//! "62% · 84 Mbps · 341/2.1k · ~4m" tray-title string per the DESIGN s2
//! formatting rules. [`RateEstimator`] and [`aggregate`] turn raw per-tick
//! byte/file counters into the `TitleMetrics` those feed on.
//!
//! The second half (below "engine") is the live wiring. The orchestrator
//! event bridge in `assembly.rs` pushes every `SourceProgress` tick through
//! [`record_progress`] and every state transition through [`note_state`]
//! into a process-wide metrics map; [`start`] spawns a 1 Hz task that
//! aggregates that map, renders a title, and writes it to the tray. Only
//! the platform gate in `start` is `cfg`-dependent (tray titles render
//! only on macOS) - every decision helper stays cross-platform and
//! unit-tested. The engine never `.await`s while holding either static's
//! lock: each tick copies the config + aggregate out and drops the guards
//! before the idle-title query runs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};

use driven_core::state::StateRepo;
use driven_core::time::{Clock, SystemClock};
use driven_core::types::{AccountId, ExecProgress, OrchestratorState};

use crate::app_state::AppState;
use crate::commands::dtos::MenuBarSettings;
use crate::tray;

const TARGET: &str = "driven::app::menubar";

/// Idle-title mode (DESIGN s2 `macos.menuBar.idle`). Wire values are the
/// camelCase strings on [`MenuBarSettings::idle`]; an unrecognised value
/// degrades to [`IdleMode::None`] rather than failing settings load.
/// [`IdleCache::title`] switches on this to pick the idle-title branch
/// (DESIGN s2 data-flow step 4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IdleMode {
    /// Icon only, no idle title.
    #[default]
    None,
    /// "2h" since the most recent successful sync.
    LastBackupAge,
    /// "1.2 GB today" via the activity-summary window query.
    UploadedToday,
}

/// Which metrics render in the live tray title, plus the idle-title mode.
/// Built once per settings change via [`MenuBarConfig::from_settings`] and
/// cached in [`CONFIG`], which every tick copies out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuBarConfig {
    /// "84 Mbps" - last-second upload bandwidth.
    pub speed: bool,
    /// "62%" - bytes_done/bytes_total across all running syncs.
    pub percent: bool,
    /// "341/2.1k" - files done/total, compact counts.
    pub files: bool,
    /// "~4m" - remaining bytes over the smoothed rate.
    pub eta: bool,
    pub idle: IdleMode,
}

impl MenuBarConfig {
    /// Maps the wire DTO to the enum form. An unrecognised `idle` string
    /// (e.g. a future value written by a newer app version, or corrupt
    /// storage) degrades to [`IdleMode::None`] rather than erroring -
    /// settings load must never fail on this field. Called from
    /// [`load_config_from_store`] on startup and on every settings change
    /// (DESIGN s2 data-flow step 5).
    pub fn from_settings(s: &MenuBarSettings) -> Self {
        let idle = match s.idle.as_str() {
            "lastBackupAge" => IdleMode::LastBackupAge,
            "uploadedToday" => IdleMode::UploadedToday,
            _ => IdleMode::None,
        };
        MenuBarConfig {
            speed: s.show_upload_speed,
            percent: s.show_percent,
            files: s.show_files,
            eta: s.show_eta,
            idle,
        }
    }
}

/// Live per-tick numbers the aggregator feeds into [`format_title`].
/// `rate_bps`/`eta_secs` are `None` until enough samples exist (DESIGN s2:
/// the EMA needs ~3s of samples, ETA is hidden until >10s and a stable
/// rate); `bytes_total`/`files_total` are `0` during the scan phase before
/// totals are known.
/// [`active_title`] builds one per tick from a [`RateEstimator`] and an
/// [`aggregate`]d [`AccountProgress`] (DESIGN s2 data-flow steps 1-3) and
/// hands it to [`format_title`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TitleMetrics {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub rate_bps: Option<f64>,
    pub eta_secs: Option<u64>,
}

/// Rounds `value` to the 3-significant-digit precision `format_speed_bits`
/// renders with (`.0` at/above 100, `.1` at/above 10, `.2` below that) and
/// returns both the trimmed rendered string and the rounded numeric value,
/// so a caller can detect a rounding carry (e.g. 999.6 -> "1000") and
/// re-scale into the next unit instead of printing an overflowed mantissa.
fn round_3sig(value: f64) -> (String, f64) {
    let rounded = if value >= 100.0 {
        value.round()
    } else if value >= 10.0 {
        (value * 10.0).round() / 10.0
    } else {
        (value * 100.0).round() / 100.0
    };
    let rendered = if value >= 100.0 {
        format!("{rounded:.0}")
    } else if value >= 10.0 {
        format!("{rounded:.1}")
    } else {
        format!("{rounded:.2}")
    };
    (trim_trailing_zeros(&rendered), rounded)
}

/// Speedtest-style auto-scaled bit rate: "84 Mbps", <= 3 significant
/// digits. Units step at 1000 (bps -> kbps -> Mbps -> Gbps, capping at
/// Gbps); the mantissa is rendered with 3 significant digits (`.0` under
/// 1000, `.1` under 10000, `.2` below that) and a trailing `.0`/`.00` is
/// trimmed so a round number like 84 Mbps doesn't print "84.0 Mbps".
///
/// Scaling happens BEFORE rounding to pick the unit, but the display
/// mantissa is only known AFTER rounding (e.g. 999.6 -> "1000" at 3 sig
/// digits) - so once rounded, re-check whether it reached the next unit's
/// threshold and re-scale if so (999,600 bps must render "1 Mbps", not
/// "1000 kbps").
pub fn format_speed_bits(bps: f64) -> String {
    const UNITS: [&str; 4] = ["bps", "kbps", "Mbps", "Gbps"];
    let mut value = bps;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    let (mut rendered, mut rounded) = round_3sig(value);
    while rounded >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
        let next = round_3sig(value);
        rendered = next.0;
        rounded = next.1;
    }
    format!("{rendered} {}", UNITS[unit])
}

/// Compact integer count: "341", "2.1k", "3.4M".
///
/// Same round-then-check-carry shape as `format_speed_bits`: the `k`/`M`
/// mantissa is always 1 decimal place, so a value like 999,950 rounds to
/// "1000.0k" at the `k` tier and must carry into `M` ("1M"), not print
/// "1000k".
pub fn format_compact_count(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    const UNITS: [&str; 2] = ["k", "M"];
    let mut value = n as f64 / 1_000.0;
    let mut unit = 0;
    loop {
        let rounded = (value * 10.0).round() / 10.0;
        if rounded >= 1000.0 && unit < UNITS.len() - 1 {
            value /= 1000.0;
            unit += 1;
        } else {
            let mantissa = trim_trailing_zeros(&format!("{rounded:.1}"));
            return format!("{mantissa}{}", UNITS[unit]);
        }
    }
}

/// Remaining-time estimate: "~40s" under a minute, "~4m" (rounded to the
/// nearest minute) under an hour, else "~1h 20m" (the minutes are omitted
/// when they round to 0, e.g. "~1h").
///
/// The whole `secs` value (at/above 60) is rounded to a total minute count
/// FIRST, then split into hours/minutes - not rounded independently within
/// an hour/minute branch already chosen - so a remainder that rounds up to
/// 60 minutes carries into the next hour instead of rendering "~1h 60m",
/// and a sub-hour total that rounds up to 60 renders "~1h", not "~60m".
pub fn format_eta(secs: u64) -> String {
    if secs < 60 {
        return format!("~{secs}s");
    }
    let total_minutes = (secs as f64 / 60.0).round() as u64;
    if total_minutes < 60 {
        format!("~{total_minutes}m")
    } else {
        let hours = total_minutes / 60;
        let minutes = total_minutes % 60;
        if minutes == 0 {
            format!("~{hours}h")
        } else {
            format!("~{hours}h {minutes}m")
        }
    }
}

/// Elapsed-time label for the `lastBackupAge` idle title: "45s" under a
/// minute, "5m" under an hour, "2h" under a day, else "3d". Same
/// round-then-carry rules as [`format_eta`] (a tier that rounds up to its
/// own ceiling carries into the next one, so 23.7h renders "1d", never
/// "24h") minus the "~" prefix - an age is measured, not estimated.
/// A negative input (clock skew: the stored timestamp is in the future)
/// clamps to 0 rather than underflowing.
pub fn format_age(ms: i64) -> String {
    let secs = ms.max(0) as u64 / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let total_minutes = (secs as f64 / 60.0).round() as u64;
    if total_minutes < 60 {
        return format!("{total_minutes}m");
    }
    let total_hours = (total_minutes as f64 / 60.0).round() as u64;
    if total_hours < 24 {
        return format!("{total_hours}h");
    }
    let days = (total_hours as f64 / 24.0).round() as u64;
    format!("{days}d")
}

/// Decimal-unit byte size for the `uploadedToday` idle title: "1.2 GB",
/// "840 MB", "12 kB". Units step at 1000 (SI, matching the rest of the
/// app's byte rendering); the mantissa carries one decimal below 10 and
/// none at or above it, with a trailing ".0" trimmed. Same round-then-
/// check-carry shape as [`format_speed_bits`]: a mantissa that rounds up
/// to 1000 carries into the next unit rather than printing "1000 MB".
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    if n < 1_000 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1_000.0;
    let mut unit = 1;
    loop {
        let rounded = if value >= 10.0 {
            value.round()
        } else {
            (value * 10.0).round() / 10.0
        };
        if rounded >= 1_000.0 && unit < UNITS.len() - 1 {
            value /= 1_000.0;
            unit += 1;
            continue;
        }
        let mantissa = if rounded >= 10.0 {
            format!("{rounded:.0}")
        } else {
            trim_trailing_zeros(&format!("{rounded:.1}"))
        };
        return format!("{mantissa} {}", UNITS[unit]);
    }
}

/// `bytes_done/bytes_total` as a floored integer percent; `None` while
/// `total` is `0` (the scan phase, before totals are computed - there is
/// nothing meaningful to divide by yet).
pub fn format_percent(done: u64, total: u64) -> Option<String> {
    if total == 0 {
        return None;
    }
    let pct = (u128::from(done) * 100) / u128::from(total);
    Some(format!("{pct}%"))
}

/// Joins the enabled metrics in DESIGN s2 order (percent, speed, files,
/// eta) with " · "; `None` when nothing is enabled, or nothing currently
/// has data to render (e.g. speed is enabled but no rate sample exists
/// yet, or percent is enabled but totals aren't known yet). Files only
/// renders once `files_total > 0`. The 1 Hz engine tick calls this via
/// [`active_title`] each time it aggregates a fresh [`TitleMetrics`].
pub fn format_title(cfg: &MenuBarConfig, m: &TitleMetrics) -> Option<String> {
    let mut parts = Vec::new();
    if cfg.percent {
        if let Some(percent) = format_percent(m.bytes_done, m.bytes_total) {
            parts.push(percent);
        }
    }
    if cfg.speed {
        if let Some(rate) = m.rate_bps {
            parts.push(format_speed_bits(rate));
        }
    }
    if cfg.files && m.files_total > 0 {
        parts.push(format!(
            "{}/{}",
            format_compact_count(m.files_done),
            format_compact_count(m.files_total)
        ));
    }
    if cfg.eta {
        if let Some(eta) = m.eta_secs {
            parts.push(format_eta(eta));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" \u{b7} "))
    }
}

/// Strips a trailing `.0`/`.00`-style zero mantissa (e.g. "84.0" -> "84",
/// "2.10" -> "2.1"); strings with no decimal point pass through unchanged.
///
/// Shared by `format_speed_bits`, `format_compact_count`, and
/// [`format_bytes`].
fn trim_trailing_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// EMA smoothing time-constant for [`RateEstimator::sample`] (DESIGN s2:
/// the tray rate should track real throughput without jittering on every
/// per-tick sample instead of visibly jumping to each instantaneous rate).
/// See `sample` for the alpha derivation.
const EMA_TAU_SECS: f64 = 3.0;

/// Minimum elapsed time since an estimator's first sample before
/// [`RateEstimator::sample`] starts returning a rate. A brand-new estimator
/// (new sync cycle, or the first tick of the app) has only a single
/// interval of history, which is too noisy to show - DESIGN s2 withholds
/// the rate/ETA metrics until a few seconds of samples have accumulated.
const MIN_SAMPLE_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// EMA-smoothed bytes/sec estimator fed one `(total_bytes, now)` sample per
/// tick. `total_bytes` is expected to be a monotonically increasing
/// cumulative counter within one sync cycle; a decrease means a new cycle
/// started and the estimator fully resets rather than computing a bogus
/// negative-delta rate. The engine task owns one and samples it each tick
/// to build the `TitleMetrics.rate_bps` fed into [`format_title`];
/// [`tick`] replaces it with a fresh estimator whenever the aggregate goes
/// inactive, so the next sync cycle starts from a clean window.
#[derive(Debug, Default)]
pub struct RateEstimator {
    /// Most recent `(total_bytes, sample_time)`.
    last: Option<(u64, std::time::Instant)>,
    /// When the first sample landed - `sample` withholds a rate until
    /// `now - first_at >= MIN_SAMPLE_WINDOW`.
    first_at: Option<std::time::Instant>,
    /// Current smoothed rate. Seeded with the first interval's
    /// instantaneous rate (rather than 0) so the EMA doesn't spend its
    /// first few ticks climbing up from zero.
    ema: Option<f64>,
}

impl RateEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one sample. Returns the smoothed bytes/sec once
    /// `MIN_SAMPLE_WINDOW` of history exists, else `None`. A `total_bytes`
    /// decrease (new sync cycle) fully resets the estimator and returns
    /// `None`. A non-positive interval (`dt <= 0` - a duplicate or
    /// out-of-order timestamp) leaves the estimator's state untouched and
    /// just re-reports whatever it would already report.
    pub fn sample(&mut self, total_bytes: u64, now: std::time::Instant) -> Option<f64> {
        let Some((last_bytes, last_time)) = self.last else {
            self.last = Some((total_bytes, now));
            self.first_at = Some(now);
            return None;
        };
        if total_bytes < last_bytes {
            *self = Self::new();
            return None;
        }
        let dt = match now.checked_duration_since(last_time) {
            Some(d) => d.as_secs_f64(),
            None => 0.0,
        };
        if dt <= 0.0 {
            return self.current();
        }
        let inst = (total_bytes - last_bytes) as f64 / dt;
        let alpha = (dt / EMA_TAU_SECS).clamp(0.0, 1.0);
        self.ema = Some(match self.ema {
            Some(prev) => prev * (1.0 - alpha) + inst * alpha,
            None => inst,
        });
        self.last = Some((total_bytes, now));
        self.current()
    }

    /// `Some(ema)` once the sample window has elapsed, else `None` - shared
    /// by the normal path above and the `dt <= 0` guard, so both return
    /// exactly what the estimator currently knows without duplicating the
    /// window check.
    fn current(&self) -> Option<f64> {
        let (_, now) = self.last?;
        let elapsed = now.checked_duration_since(self.first_at?)?;
        if elapsed >= MIN_SAMPLE_WINDOW {
            self.ema
        } else {
            None
        }
    }
}

/// Remaining-time estimate in whole seconds from a smoothed rate and the
/// remaining byte count; `None` until the rate is at least 1 byte/s (below
/// that there isn't a meaningful rate yet - could be idle, could be a
/// near-zero blip). Caps at 99h so a stalled-but-technically-positive rate
/// can't render an absurd tray string like "~4000h". [`active_title`]
/// feeds this the `RateEstimator` output each tick to build
/// `TitleMetrics.eta_secs`.
pub fn eta_secs(rate_bps_bytes: f64, remaining_bytes: u64) -> Option<u64> {
    if rate_bps_bytes < 1.0 {
        return None;
    }
    const CAP_SECS: u64 = 99 * 3600;
    let secs = (remaining_bytes as f64 / rate_bps_bytes).round() as u64;
    Some(secs.min(CAP_SECS))
}

/// One account's live sync counters for the cross-account tray aggregate
/// (DESIGN s2: the tray shows a single combined percent/files/ETA across
/// every currently-running account, not one per account). `active` marks
/// whether this account currently has a sync in progress; `aggregate` sums
/// only active accounts so a finished or paused account's stale totals
/// don't dilute the combined picture. [`record_progress`] / [`note_state`]
/// maintain one per account in [`METRICS`]; each tick folds them via
/// [`aggregate`].
#[derive(Debug, Clone, Copy, Default)]
pub struct AccountProgress {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
    pub active: bool,
}

/// Sums the four counters over active accounts only; the result's `active`
/// is true if any input account was active. An all-inactive input (or an
/// empty iterator) returns `AccountProgress::default()` (all zero,
/// inactive). Called once per tick to build the single [`TitleMetrics`]
/// fed into [`format_title`].
pub fn aggregate(accounts: impl Iterator<Item = AccountProgress>) -> AccountProgress {
    accounts
        .filter(|a| a.active)
        .fold(AccountProgress::default(), |mut acc, a| {
            acc.bytes_done += a.bytes_done;
            acc.bytes_total += a.bytes_total;
            acc.files_done += a.files_done;
            acc.files_total += a.files_total;
            acc.active = true;
            acc
        })
}

// -----------------------------------------------------------------------------
// engine: shared state, recorders, and the 1 Hz title task
// -----------------------------------------------------------------------------

/// Live per-account sync counters, written by the orchestrator event bridge
/// ([`record_progress`] / [`note_state`]) and read once per tick. A plain
/// `std::sync::Mutex` rather than a tokio one: every critical section is a
/// synchronous map edit, and the tick copies the aggregate out before it
/// `.await`s, so the guard is never held across a suspend point.
static METRICS: LazyLock<Mutex<HashMap<AccountId, AccountProgress>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Config cache, loaded at [`start`] and on [`config_changed`]; read each
/// tick. Seeded with the DTO defaults so a tick that lands before the
/// (async) settings read completes renders the default title rather than
/// nothing at all.
static CONFIG: LazyLock<Mutex<MenuBarConfig>> =
    LazyLock::new(|| Mutex::new(MenuBarConfig::from_settings(&MenuBarSettings::default())));

/// Bumped by [`load_config_from_store`] once a freshly-read config has been
/// written to [`CONFIG`], so the engine re-renders immediately-ish (next 1 s
/// tick) and the idle cache invalidates instead of showing a stale value for
/// up to a minute after a settings change.
static CONFIG_GEN: AtomicU64 = AtomicU64::new(0);

/// How long an idle title is reused before it is re-queried. The idle
/// values (last-backup age, bytes uploaded today) move on a scale of
/// minutes, so a per-second repo query would be pure waste.
const IDLE_REFRESH: Duration = Duration::from_secs(60);

/// How often the engine re-renders the tray title (DESIGN s2: 1 Hz).
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Records one `SourceProgress` tick for an account. The counters are
/// absolute (not deltas), so this is a straight overwrite; seeing progress
/// at all means the account is mid-execution, hence `active: true`.
pub fn record_progress(account_id: AccountId, p: &ExecProgress) {
    let mut metrics = METRICS.lock().unwrap_or_else(|e| e.into_inner());
    let entry = metrics.entry(account_id).or_default();
    entry.bytes_done = p.bytes_done;
    entry.bytes_total = p.bytes_total;
    entry.files_done = p.files_done;
    entry.files_total = p.files_total;
    entry.active = true;
}

/// What an [`OrchestratorState`] transition does to an account's entry in
/// [`METRICS`]. Split out as a pure decision so the state-to-effect mapping
/// is unit-testable without a live map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetricsUpdate {
    /// The account is mid-cycle: mark (or create) its entry active.
    Activate,
    /// The cycle stalled but has not finished (paused, backing off, in the
    /// power-check or verify phase, or errored). Keep the counters - the
    /// last-known progress is still the truth - but stop counting the
    /// account towards the live aggregate.
    Deactivate,
    /// The cycle finished. Drop the entry entirely so its stale totals
    /// cannot resurface if the account later reappears mid-cycle.
    Remove,
}

/// SPEC s2: only the scan / plan / execute phases count as "syncing" for
/// the tray title. `Idle` retires the account's entry; every other state
/// (paused, backoff, verifying, power-check, error) parks it.
fn classify_state(state: &OrchestratorState) -> MetricsUpdate {
    match state {
        OrchestratorState::Scanning { .. }
        | OrchestratorState::Planning { .. }
        | OrchestratorState::Executing { .. } => MetricsUpdate::Activate,
        OrchestratorState::Idle { .. } => MetricsUpdate::Remove,
        _ => MetricsUpdate::Deactivate,
    }
}

/// Records an orchestrator state transition for an account.
pub fn note_state(account_id: AccountId, state: &OrchestratorState) {
    let mut metrics = METRICS.lock().unwrap_or_else(|e| e.into_inner());
    match classify_state(state) {
        MetricsUpdate::Activate => {
            metrics.entry(account_id).or_default().active = true;
        }
        MetricsUpdate::Deactivate => {
            // Only parks an entry that already exists - an inactive entry
            // contributes nothing to `aggregate`, so creating one here would
            // be pure bookkeeping noise.
            if let Some(entry) = metrics.get_mut(&account_id) {
                entry.active = false;
            }
        }
        MetricsUpdate::Remove => {
            metrics.remove(&account_id);
        }
    }
}

/// Starts the 1 Hz tray-title engine. No-op off macOS: `set_title` is a
/// no-op on Windows and Linux tray implementations, so the whole task
/// (and its per-minute idle queries) would be wasted work there.
///
/// Safe to call before any sync starts - the first ticks simply render the
/// idle title (or nothing).
pub fn start(app: &AppHandle) {
    if !cfg!(target_os = "macos") {
        return;
    }
    // Kicks off an async settings read; until it lands, ticks use the
    // defaults `CONFIG` was seeded with.
    load_config_from_store(app);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(TICK_INTERVAL);
        // A missed tick (busy runtime) is skipped rather than burst-fired:
        // catching up would just repaint the same title several times.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut rate = RateEstimator::new();
        let mut idle = IdleCache::default();
        // `None` = nothing painted yet, so the first tick always writes
        // (including writing `None` to clear a title left by a previous run).
        let mut painted: Option<Option<String>> = None;
        loop {
            ticker.tick().await;
            tick(&app, &mut rate, &mut idle, &mut painted).await;
        }
    });
}

/// Re-reads the persisted menu bar settings into [`CONFIG`] and invalidates
/// the idle cache, so a settings change shows up on the next tick instead of
/// waiting out the idle refresh window (spec s2: settings apply immediately,
/// no restart). Called from `update_settings` whenever the patch touched
/// `macos.menu_bar`.
pub fn config_changed(app: &AppHandle) {
    load_config_from_store(app);
}

/// Reads `macos.menu_bar` from the settings store into [`CONFIG`].
///
/// Fire-and-forget: the read is async but neither caller can await, and a
/// failure to read is not worth surfacing - the engine keeps rendering with
/// whatever config it already has (the defaults, at worst). The `AppState`
/// is resolved on the calling thread and only the `Arc<dyn StateRepo>` is
/// moved into the task, so no `State` borrow crosses the spawn.
fn load_config_from_store(app: &AppHandle) {
    let Some(repo) = state_repo(app) else {
        tracing::debug!(
            target: TARGET,
            "app state unavailable; keeping the current menu bar config"
        );
        return;
    };
    tauri::async_runtime::spawn(async move {
        let settings = crate::commands::settings::load_menubar_settings(repo.as_ref()).await;
        let cfg = MenuBarConfig::from_settings(&settings);
        // Lock taken only after the await completes, and released at the end
        // of this statement - nothing is awaited under it.
        *CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
        // Bumped AFTER the write, so the generation only advances once the
        // new config is actually observable by a tick. Bumping before the
        // (async) read would let a tick stamp the OLD config's idle result
        // with the NEW generation and then consider it fresh.
        CONFIG_GEN.fetch_add(1, Ordering::Relaxed);
    });
}

/// Clones the managed state repo out of the app handle, or `None` when the
/// app state is not managed yet (very early startup) - callers degrade to
/// "no idle title" / "keep the current config" rather than panicking, which
/// is what `Manager::state` would do.
fn state_repo(app: &AppHandle) -> Option<std::sync::Arc<dyn StateRepo>> {
    app.try_state::<AppState>().map(|s| s.state().clone())
}

/// The idle title plus the freshness metadata that decides when to re-query
/// it. Owned by the engine task, so no locking is involved.
#[derive(Debug, Default)]
struct IdleCache {
    /// Last rendered idle title (`None` = icon only).
    value: Option<String>,
    /// When `value` was computed; `None` before the first query.
    fetched_at: Option<Instant>,
    /// The [`CONFIG_GEN`] `value` was computed under.
    generation: u64,
    /// The mode `value` was computed under. Tracked separately from
    /// `generation` because [`CONFIG`] is written by an async task while the
    /// engine ticks: at startup the first tick renders under the seeded
    /// defaults (`IdleMode::None`) and caches that result, and without this
    /// field the real mode's title would not appear until the TTL expired.
    mode: IdleMode,
}

/// Whether a cached idle title must be re-queried: never fetched, older
/// than `ttl`, computed under a different idle mode, or computed under a
/// superseded config generation. Pure, so the staleness rule is unit-tested
/// without a clock or an `AppHandle`.
///
/// The mode check is what makes this order-independent: it holds whether the
/// config landed before or after the tick that cached the value, so neither
/// the startup race nor a settings change can strand a stale title.
fn idle_cache_is_stale(
    fetched_at: Option<Instant>,
    cached_generation: u64,
    current_generation: u64,
    cached_mode: IdleMode,
    current_mode: IdleMode,
    now: Instant,
    ttl: Duration,
) -> bool {
    if cached_generation != current_generation || cached_mode != current_mode {
        return true;
    }
    match fetched_at {
        None => true,
        Some(at) => now.saturating_duration_since(at) >= ttl,
    }
}

impl IdleCache {
    /// The idle title for `cfg`, re-querying the state repo at most once per
    /// [`IDLE_REFRESH`] (or immediately after a config change).
    async fn title(&mut self, app: &AppHandle, cfg: &MenuBarConfig) -> Option<String> {
        let current_generation = CONFIG_GEN.load(Ordering::Relaxed);
        if idle_cache_is_stale(
            self.fetched_at,
            self.generation,
            current_generation,
            self.mode,
            cfg.idle,
            Instant::now(),
            IDLE_REFRESH,
        ) {
            self.value = fetch_idle_title(app, cfg.idle).await;
            // Stamped AFTER the query so the TTL measures time since the
            // value was known-good, not since the query started.
            self.fetched_at = Some(Instant::now());
            self.generation = current_generation;
            self.mode = cfg.idle;
        }
        self.value.clone()
    }
}

/// Queries the state repo for the idle title. Every failure path (missing
/// app state, repo error, no account ever synced, an unrepresentable local
/// midnight) renders `None` - the tray shows the icon alone rather than a
/// stale or invented value, and nothing here can panic.
async fn fetch_idle_title(app: &AppHandle, mode: IdleMode) -> Option<String> {
    if mode == IdleMode::None {
        return None;
    }
    let repo = state_repo(app)?;
    match mode {
        IdleMode::None => None,
        IdleMode::LastBackupAge => {
            let rows = match repo.list_accounts().await {
                Ok(rows) => rows,
                Err(err) => {
                    tracing::debug!(target: TARGET, %err, "idle title: list_accounts failed");
                    return None;
                }
            };
            // The most recent successful sync across ALL accounts - the tray
            // shows one age, and "last time anything was backed up" is the
            // question the user is asking.
            let latest = rows.iter().filter_map(|r| r.last_synced_at).max()?;
            Some(format_age(SystemClock.now_ms() - latest))
        }
        IdleMode::UploadedToday => {
            let day_start_ms = local_day_start_ms()?;
            // Only `bytes_today` is used, so the week bound collapses onto the
            // day bound and the throughput window is the smallest legal value.
            match repo
                .activity_summary(day_start_ms, day_start_ms, day_start_ms, 1)
                .await
            {
                Ok(summary) => Some(format!("{} today", format_bytes(summary.bytes_today))),
                Err(err) => {
                    tracing::debug!(target: TARGET, %err, "idle title: activity_summary failed");
                    None
                }
            }
        }
    }
}

/// Start of the current local day as Unix ms.
///
/// Local midnight does not exist on the (rare) day a timezone jumps its DST
/// forward at 00:00 - Cuba and Lord Howe do exactly this - so walk forward
/// an hour at a time to the first local wall-clock time that does resolve,
/// which is the true start of that day. `None` only if none of the first
/// four hours resolve, which no real timezone does.
fn local_day_start_ms() -> Option<i64> {
    let today = chrono::Local::now().date_naive();
    for hour in 0..4 {
        let Some(naive) = today.and_hms_opt(hour, 0, 0) else {
            continue;
        };
        // `earliest` rather than `single`: on a DST fall-back the hour repeats
        // and the FIRST occurrence is the real start of the day.
        if let Some(dt) = naive.and_local_timezone(chrono::Local).earliest() {
            return Some(dt.timestamp_millis());
        }
    }
    tracing::debug!(target: TARGET, "no resolvable local day start; skipping idle title");
    None
}

/// Builds the live (syncing) title from the aggregate and the current
/// smoothed rate. `rate_bytes` is BYTES/s as the estimator reports it; the
/// title renders bits, so it is multiplied by 8 on the way into
/// [`TitleMetrics`]. Pure, so the ETA gating and unit conversion are
/// unit-tested without a runtime.
fn active_title(
    cfg: &MenuBarConfig,
    agg: &AccountProgress,
    rate_bytes: Option<f64>,
) -> Option<String> {
    let eta_secs_value = if cfg.eta {
        rate_bytes.and_then(|r| eta_secs(r, agg.bytes_total.saturating_sub(agg.bytes_done)))
    } else {
        None
    };
    let metrics = TitleMetrics {
        bytes_done: agg.bytes_done,
        bytes_total: agg.bytes_total,
        files_done: agg.files_done,
        files_total: agg.files_total,
        rate_bps: rate_bytes.map(|r| r * 8.0),
        eta_secs: eta_secs_value,
    };
    format_title(cfg, &metrics)
}

/// Whether the newly rendered title differs from what is on the tray.
/// `last` is `None` before anything has been painted (so the first tick
/// always paints, including painting `None` to clear a leftover title).
/// Keeps the OS off the hook for a repaint every second while the title is
/// unchanged - which, idling, is essentially always.
fn should_paint(last: &Option<Option<String>>, next: &Option<String>) -> bool {
    match last {
        None => true,
        Some(previous) => previous != next,
    }
}

/// One engine tick: snapshot the shared state, render a title, and write it
/// to the tray if it changed.
async fn tick(
    app: &AppHandle,
    rate: &mut RateEstimator,
    idle: &mut IdleCache,
    painted: &mut Option<Option<String>>,
) {
    // Copy both statics out and drop the guards BEFORE the idle branch's
    // `.await` below - a std Mutex guard must never cross a suspend point.
    let cfg = *CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    let agg = {
        let metrics = METRICS.lock().unwrap_or_else(|e| e.into_inner());
        aggregate(metrics.values().copied())
    };

    let title = if agg.active {
        let rate_bytes = rate.sample(agg.bytes_done, Instant::now());
        active_title(&cfg, &agg, rate_bytes)
    } else {
        // Reset the estimator so the next sync cycle starts from a clean
        // sample window instead of inheriting the previous run's EMA.
        *rate = RateEstimator::new();
        idle.title(app, &cfg).await
    };

    if !should_paint(painted, &title) {
        return;
    }
    let Some(tray) = app.tray_by_id(tray::TRAY_ID) else {
        // Tray not built yet / mid-rebuild. Leave `painted` untouched so the
        // next tick retries this same title against the rebuilt tray.
        tracing::trace!(target: TARGET, "tray absent during title tick; skipping");
        return;
    };
    // `set_title(None)` clears the title, leaving the icon alone.
    if let Err(err) = tray.set_title(title.as_deref()) {
        tracing::debug!(target: TARGET, %err, "set tray title failed");
        return;
    }
    *painted = Some(title);
}

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

    // Review fix (round-1): the mantissa is rounded to display precision
    // AFTER the unit is picked, so a value just under a unit's threshold
    // can round UP to that threshold and needs to carry into the next
    // unit rather than rendering an overflowed mantissa in the old unit.
    #[test]
    fn speed_and_count_round_carry_into_next_unit() {
        assert_eq!(format_speed_bits(999_600.0), "1 Mbps");
        assert_eq!(format_compact_count(999_950), "1M");
    }

    // Review fix (round-1): same carry family as above, applied to the
    // minutes/hours split - a remainder that rounds up to 60 minutes must
    // carry into the next hour, and a sub-hour total that rounds up to 60
    // minutes must render as a full hour, not "~60m"/"~Xh 60m".
    #[test]
    fn eta_rounds_carry_into_next_unit() {
        assert_eq!(format_eta(3599), "~1h");
        assert_eq!(format_eta(7199), "~2h");
    }

    #[test]
    fn eta_formats_by_magnitude() {
        assert_eq!(format_eta(40), "~40s");
        assert_eq!(format_eta(4 * 60), "~4m");
        assert_eq!(format_eta(60 * 60 + 20 * 60), "~1h 20m");
    }

    #[test]
    fn title_joins_enabled_metrics_in_spec_order() {
        let cfg = MenuBarConfig {
            speed: true,
            percent: true,
            files: false,
            eta: false,
            idle: IdleMode::None,
        };
        let m = TitleMetrics {
            bytes_done: 62,
            bytes_total: 100,
            files_done: 341,
            files_total: 2148,
            rate_bps: Some(84_000_000.0),
            eta_secs: Some(240),
        };
        assert_eq!(format_title(&cfg, &m).unwrap(), "62% · 84 Mbps");
        let all = MenuBarConfig {
            speed: true,
            percent: true,
            files: true,
            eta: true,
            idle: IdleMode::None,
        };
        assert_eq!(
            format_title(&all, &m).unwrap(),
            "62% · 84 Mbps · 341/2.1k · ~4m"
        );
        let none = MenuBarConfig {
            speed: false,
            percent: false,
            files: false,
            eta: false,
            idle: IdleMode::None,
        };
        assert_eq!(format_title(&none, &m), None);
    }

    #[test]
    fn title_omits_unavailable_metrics() {
        // no totals yet (scan phase): percent+eta render nothing; speed absent
        // until a rate sample exists -> whole title is None
        let cfg = MenuBarConfig {
            speed: true,
            percent: true,
            files: false,
            eta: true,
            idle: IdleMode::None,
        };
        let m = TitleMetrics {
            bytes_done: 0,
            bytes_total: 0,
            files_done: 0,
            files_total: 0,
            rate_bps: None,
            eta_secs: None,
        };
        assert_eq!(format_title(&cfg, &m), None);
    }

    // Test body (including the mutate-after-default pattern) is verbatim
    // from the task-3 brief; suppress clippy's preferred struct-update-
    // syntax style nit rather than rewriting the brief's test.
    #[allow(clippy::field_reassign_with_default)]
    #[test]
    fn config_from_settings_maps_idle_and_tolerates_unknown() {
        use crate::commands::dtos::MenuBarSettings;
        let mut s = MenuBarSettings::default();
        s.idle = "uploadedToday".into();
        assert!(matches!(
            MenuBarConfig::from_settings(&s).idle,
            IdleMode::UploadedToday
        ));
        s.idle = "garbage".into();
        assert!(matches!(
            MenuBarConfig::from_settings(&s).idle,
            IdleMode::None
        ));
    }

    // Task 4 brief step 1 (verbatim): drives `RateEstimator::sample` with a
    // hand-built `Instant` sequence - `Instant::now()` once + `Duration`
    // additions - so there is no sleeping and no wall-clock flake.
    #[test]
    fn rate_needs_a_window_then_smooths() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let mut r = RateEstimator::new();
        assert_eq!(r.sample(0, t0), None); // first sample: no interval yet
        assert_eq!(r.sample(1_000_000, t0 + Duration::from_millis(1000)), None); // < 2s window
        let v = r
            .sample(2_000_000, t0 + Duration::from_millis(2000))
            .unwrap();
        assert!((v - 1_000_000.0).abs() < 50_000.0, "steady 1 MB/s, got {v}");
        // a burst decays toward the new rate rather than jumping (EMA)
        let v2 = r
            .sample(12_000_000, t0 + Duration::from_millis(3000))
            .unwrap();
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
        let a = AccountProgress {
            bytes_done: 10,
            bytes_total: 100,
            files_done: 1,
            files_total: 2,
            active: true,
        };
        let b = AccountProgress {
            bytes_done: 90,
            bytes_total: 900,
            files_done: 3,
            files_total: 4,
            active: false,
        };
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

    #[test]
    fn age_formats_by_magnitude() {
        assert_eq!(format_age(5 * 60 * 1_000), "5m");
        assert_eq!(format_age(2 * 3_600 * 1_000), "2h");
        assert_eq!(format_age(3 * 86_400 * 1_000), "3d");
    }

    // Same carry family as `format_eta`: a tier that rounds up to its own
    // ceiling must promote rather than render "60m" / "24h". A negative age
    // (the stored timestamp is ahead of the clock) clamps instead of
    // underflowing the unsigned conversion.
    #[test]
    fn age_carries_and_clamps() {
        assert_eq!(format_age(3_599_000), "1h"); // 59m59s
        assert_eq!(format_age(86_399_000), "1d"); // 23h59m59s
        assert_eq!(format_age(-5_000), "0s");
    }

    #[test]
    fn bytes_format_in_decimal_units() {
        assert_eq!(format_bytes(1_200_000_000), "1.2 GB");
        assert_eq!(format_bytes(840_000_000), "840 MB");
        assert_eq!(format_bytes(12_000), "12 kB");
    }

    #[test]
    fn bytes_handle_small_values_and_unit_carry() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        // 999.96 MB rounds to 1000 at this tier and must carry into GB.
        assert_eq!(format_bytes(999_960_000), "1 GB");
    }

    #[test]
    fn state_classification_matches_the_active_phases() {
        use driven_core::types::{ExecProgress, PauseReason, PlanSummary, SourceId};
        assert_eq!(
            classify_state(&OrchestratorState::Scanning {
                source_id: SourceId::new_v4(),
                scanned: 3,
            }),
            MetricsUpdate::Activate
        );
        assert_eq!(
            classify_state(&OrchestratorState::Planning {
                plan: PlanSummary::default(),
            }),
            MetricsUpdate::Activate
        );
        assert_eq!(
            classify_state(&OrchestratorState::Executing {
                progress: ExecProgress::default(),
            }),
            MetricsUpdate::Activate
        );
        assert_eq!(
            classify_state(&OrchestratorState::Idle { last_run_at: None }),
            MetricsUpdate::Remove
        );
        assert_eq!(
            classify_state(&OrchestratorState::Paused {
                reason: PauseReason::Manual,
            }),
            MetricsUpdate::Deactivate
        );
        assert_eq!(
            classify_state(&OrchestratorState::PowerCheck),
            MetricsUpdate::Deactivate
        );
    }

    #[test]
    fn idle_cache_staleness_rules() {
        use std::time::Duration;
        let now = Instant::now();
        let ttl = Duration::from_secs(60);
        let none = IdleMode::None;
        // never fetched
        assert!(idle_cache_is_stale(None, 0, 0, none, none, now, ttl));
        // fresh enough, same generation and mode
        assert!(!idle_cache_is_stale(
            Some(now - Duration::from_secs(30)),
            0,
            0,
            none,
            none,
            now,
            ttl
        ));
        // past the TTL
        assert!(idle_cache_is_stale(
            Some(now - Duration::from_secs(61)),
            0,
            0,
            none,
            none,
            now,
            ttl
        ));
        // fresh, but the config generation moved under it
        assert!(idle_cache_is_stale(
            Some(now - Duration::from_secs(1)),
            0,
            1,
            none,
            none,
            now,
            ttl
        ));
    }

    // The startup race: `CONFIG` is seeded with the defaults and overwritten
    // by an async read, so tick 1 can cache an `IdleMode::None` result (no
    // title) before the user's real mode lands. Staleness must key on the
    // MODE too, or the real idle title would not appear until the 60 s TTL
    // expired - and the generation alone cannot cover it, since the cache is
    // stamped with whatever generation was current at the time.
    #[test]
    fn idle_cache_invalidates_when_only_the_mode_changed() {
        use std::time::Duration;
        let now = Instant::now();
        let ttl = Duration::from_secs(60);
        let fresh = Some(now - Duration::from_secs(1));
        assert!(idle_cache_is_stale(
            fresh,
            0,
            0,
            IdleMode::None,
            IdleMode::LastBackupAge,
            now,
            ttl
        ));
        assert!(idle_cache_is_stale(
            fresh,
            0,
            0,
            IdleMode::LastBackupAge,
            IdleMode::UploadedToday,
            now,
            ttl
        ));
    }

    #[test]
    fn active_title_converts_bytes_to_bits_and_gates_eta() {
        let agg = AccountProgress {
            bytes_done: 50,
            bytes_total: 100,
            files_done: 1,
            files_total: 2,
            active: true,
        };
        let cfg = MenuBarConfig {
            speed: true,
            percent: false,
            files: false,
            eta: true,
            idle: IdleMode::None,
        };
        // 10 bytes/s renders as 80 bps, and 50 remaining bytes at 10 B/s is ~5s.
        assert_eq!(
            active_title(&cfg, &agg, Some(10.0)).unwrap(),
            "80 bps · ~5s"
        );
        // eta disabled: the remaining-time part disappears, speed stays.
        let no_eta = MenuBarConfig { eta: false, ..cfg };
        assert_eq!(active_title(&no_eta, &agg, Some(10.0)).unwrap(), "80 bps");
        // no rate sample yet: neither speed nor eta can render.
        assert_eq!(active_title(&cfg, &agg, None), None);
    }

    #[test]
    fn paint_only_on_change() {
        let a = Some("62%".to_string());
        let b = Some("63%".to_string());
        // nothing painted yet: always paint, even a `None` title (which
        // clears a title left over from a previous run)
        assert!(should_paint(&None, &None));
        assert!(should_paint(&None, &a));
        assert!(!should_paint(&Some(a.clone()), &a));
        assert!(should_paint(&Some(a.clone()), &b));
        assert!(should_paint(&Some(a), &None));
    }

    #[test]
    fn record_progress_and_note_state_track_one_account() {
        use driven_core::types::AccountId;
        // Operates on the process-wide METRICS map, so use a fresh account id
        // and clean up afterwards - other tests in this binary share it.
        let id = AccountId::new_v4();
        record_progress(
            id,
            &driven_core::types::ExecProgress {
                files_done: 3,
                files_total: 10,
                bytes_done: 30,
                bytes_total: 100,
                ..Default::default()
            },
        );
        {
            let m = METRICS.lock().unwrap_or_else(|e| e.into_inner());
            let entry = m.get(&id).copied().expect("entry recorded");
            assert_eq!(entry.bytes_done, 30);
            assert_eq!(entry.files_total, 10);
            assert!(entry.active);
        }
        // A pause parks the entry but keeps the counters.
        note_state(
            id,
            &OrchestratorState::Paused {
                reason: driven_core::types::PauseReason::Manual,
            },
        );
        {
            let m = METRICS.lock().unwrap_or_else(|e| e.into_inner());
            let entry = m.get(&id).copied().expect("entry parked, not dropped");
            assert!(!entry.active);
            assert_eq!(entry.bytes_done, 30);
        }
        // Idle retires it entirely.
        note_state(id, &OrchestratorState::Idle { last_run_at: None });
        {
            let m = METRICS.lock().unwrap_or_else(|e| e.into_inner());
            assert!(m.get(&id).is_none(), "idle removes the entry");
        }
    }
}
