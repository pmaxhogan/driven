//! macOS menu bar extra: pure config + formatting core (SPEC s22, DESIGN s2
//! `docs/superpowers/specs/2026-07-31-settings-redesign-menubar-design.md`).
//!
//! Everything here is pure and platform-independent - no `Instant::now()`,
//! no macOS APIs, no tray access - so it compiles and is unit-tested on all
//! three CI targets, matching DESIGN s2 "Testing": pure functions unit-
//! tested with injected time, no live clock in the logic under test.
//! [`MenuBarConfig`] mirrors the [`crate::commands::dtos::MenuBarSettings`]
//! wire DTO in enum form; the `format_*` functions turn a live
//! [`TitleMetrics`] snapshot into the "62% · 84 Mbps · 341/2.1k · ~4m"
//! tray-title string per the DESIGN s2 formatting rules. [`RateEstimator`]
//! and [`aggregate`] turn raw per-tick byte/file counters into the
//! `TitleMetrics` this feeds on; the engine + bridge wiring task drives
//! both with real per-tick sampling and owns the `set_title` call - this
//! module has no knowledge of the tray or the tokio task that ticks it.

use crate::commands::dtos::MenuBarSettings;

/// Idle-title mode (DESIGN s2 `macos.menuBar.idle`). Wire values are the
/// camelCase strings on [`MenuBarSettings::idle`]; an unrecognised value
/// degrades to [`IdleMode::None`] rather than failing settings load.
// Not yet constructed outside tests - the engine + bridge wiring task reads
// this to pick the idle-title branch (DESIGN s2 data-flow step 4).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleMode {
    /// Icon only, no idle title.
    None,
    /// "2h" since the most recent successful sync.
    LastBackupAge,
    /// "1.2 GB today" via the activity-summary window query.
    UploadedToday,
}

/// Which metrics render in the live tray title, plus the idle-title mode.
/// Built once per settings change via [`MenuBarConfig::from_settings`].
// Not yet constructed outside tests - the engine + bridge wiring task
// builds one from live settings each time they change and threads it into
// the per-tick title formatter.
#[allow(dead_code)]
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
    /// settings load must never fail on this field.
    // Not yet called outside tests - the engine + bridge wiring task calls
    // this from the settings-changed handler (DESIGN s2 data-flow step 5).
    #[allow(dead_code)]
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
// Not yet constructed outside tests - the engine + bridge wiring task
// builds one per tick from a [`RateEstimator`] and an [`aggregate`]d
// [`AccountProgress`] (DESIGN s2 data-flow steps 1-3) and hands it to
// `format_title`.
#[allow(dead_code)]
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
///
/// Only called from `format_speed_bits` below, so it goes dead alongside
/// it until the engine + bridge wiring task starts driving `format_title`.
#[allow(dead_code)]
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
///
/// Not yet called outside tests - `format_title` (below) calls this once
/// the engine + bridge wiring task starts feeding it real rate samples.
#[allow(dead_code)]
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
///
/// Not yet called outside tests - `format_title` (below) calls this once
/// the engine + bridge wiring task starts feeding it real file counts.
#[allow(dead_code)]
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
///
/// Not yet called outside tests - `format_title` (below) calls this once
/// the engine + bridge wiring task starts feeding it a real ETA estimate.
#[allow(dead_code)]
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

/// `bytes_done/bytes_total` as a floored integer percent; `None` while
/// `total` is `0` (the scan phase, before totals are computed - there is
/// nothing meaningful to divide by yet).
///
/// Not yet called outside tests - `format_title` (below) calls this once
/// the engine + bridge wiring task starts feeding it real byte totals.
#[allow(dead_code)]
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
/// renders once `files_total > 0`.
///
/// Not yet called outside tests - the engine + bridge wiring task's 1 Hz
/// tick calls this each time it aggregates a fresh `TitleMetrics` and
/// passes the result to `TrayIcon::set_title`.
#[allow(dead_code)]
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
/// Only called from `format_speed_bits`/`format_compact_count` above, so
/// it goes dead alongside them until the engine + bridge wiring task
/// starts driving `format_title`.
#[allow(dead_code)]
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
/// negative-delta rate.
// Not yet constructed outside tests - the engine + bridge wiring task
// (Task 5) owns one per running sync and samples it each tick to build the
// `TitleMetrics.rate_bps` fed into `format_title`.
#[allow(dead_code)]
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
    /// Not yet called outside tests - Task 5 constructs one per running sync.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one sample. Returns the smoothed bytes/sec once
    /// `MIN_SAMPLE_WINDOW` of history exists, else `None`. A `total_bytes`
    /// decrease (new sync cycle) fully resets the estimator and returns
    /// `None`. A non-positive interval (`dt <= 0` - a duplicate or
    /// out-of-order timestamp) leaves the estimator's state untouched and
    /// just re-reports whatever it would already report.
    ///
    /// Not yet called outside tests - Task 5's per-tick loop drives this.
    #[allow(dead_code)]
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
/// can't render an absurd tray string like "~4000h".
///
/// Not yet called outside tests - Task 5 feeds this the `RateEstimator`
/// output each tick to build `TitleMetrics.eta_secs`.
#[allow(dead_code)]
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
/// don't dilute the combined picture.
// Not yet constructed outside tests - Task 5 builds one per account from
// the shared per-account sync state each tick and folds them via
// `aggregate`.
#[allow(dead_code)]
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
/// inactive).
///
/// Not yet called outside tests - Task 5 calls this once per tick to build
/// the single `TitleMetrics` fed into `format_title`.
#[allow(dead_code)]
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
}
