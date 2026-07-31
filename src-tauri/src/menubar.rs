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
//! tray-title string per the DESIGN s2 formatting rules. Later tasks (the
//! rate estimator/aggregation task and the engine + bridge wiring task)
//! drive this with real per-tick sampling and own the `set_title` call;
//! this module has no knowledge of the tray or the tokio task that ticks it.

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
// Not yet constructed outside tests - the rate estimator + aggregation
// task builds one per tick from the shared TrayMetrics map (DESIGN s2
// data-flow steps 1-3) and hands it to `format_title`.
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

/// Speedtest-style auto-scaled bit rate: "84 Mbps", <= 3 significant
/// digits. Units step at 1000 (bps -> kbps -> Mbps -> Gbps, capping at
/// Gbps); the mantissa is rendered with 3 significant digits (`.0` under
/// 1000, `.1` under 10000, `.2` below that) and a trailing `.0`/`.00` is
/// trimmed so a round number like 84 Mbps doesn't print "84.0 Mbps".
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
    let rendered = if value >= 100.0 {
        format!("{value:.0}")
    } else if value >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    };
    format!("{} {}", trim_trailing_zeros(&rendered), UNITS[unit])
}

/// Compact integer count: "341", "2.1k", "3.4M".
///
/// Not yet called outside tests - `format_title` (below) calls this once
/// the engine + bridge wiring task starts feeding it real file counts.
#[allow(dead_code)]
pub fn format_compact_count(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        let mantissa = trim_trailing_zeros(&format!("{:.1}", n as f64 / 1_000.0));
        format!("{mantissa}k")
    } else {
        let mantissa = trim_trailing_zeros(&format!("{:.1}", n as f64 / 1_000_000.0));
        format!("{mantissa}M")
    }
}

/// Remaining-time estimate: "~40s" under a minute, "~4m" (rounded to the
/// nearest minute) under an hour, else "~1h 20m" (the minutes are omitted
/// when they round to 0, e.g. "~1h").
///
/// Not yet called outside tests - `format_title` (below) calls this once
/// the engine + bridge wiring task starts feeding it a real ETA estimate.
#[allow(dead_code)]
pub fn format_eta(secs: u64) -> String {
    if secs < 60 {
        format!("~{secs}s")
    } else if secs < 3600 {
        let minutes = (secs as f64 / 60.0).round() as u64;
        format!("~{minutes}m")
    } else {
        let hours = secs / 3600;
        let minutes = ((secs % 3600) as f64 / 60.0).round() as u64;
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
}
