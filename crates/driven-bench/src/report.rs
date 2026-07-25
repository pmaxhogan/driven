//! Report rendering: a markdown table for humans, raw JSON for trending.
//!
//! Both files are written per run under `bench/results/`, named by UTC
//! timestamp. The markdown is what goes in a PR or a release note; the JSON
//! keeps every field (including the ones the table omits for width) so a later
//! run can be diffed against an earlier one without re-running anything.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::fixture::{human_bytes, FixtureSpec};
use crate::tools::{PhaseResult, Tool};

/// One fixture's worth of measurements.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioReport {
    pub spec: FixtureSpec,
    pub results: Vec<PhaseResult>,
}

/// The whole run.
#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    /// UTC timestamp, `YYYY-MM-DDTHH:MM:SSZ`.
    pub started_at: String,
    /// The scale name the run was invoked with.
    pub scale: String,
    /// The fixture seed, so a re-run can reproduce the same trees.
    pub seed: u64,
    /// OS + architecture the numbers were produced on. Comparing across hosts
    /// is meaningless, so the report always says which host it was.
    pub host: String,
    /// Logical CPU count, for context on the concurrency columns.
    pub cpus: usize,
    /// The Driven version under test.
    pub driven_version: String,
    /// The rclone build, when rclone took part.
    pub rclone_version: Option<String>,
    /// Which tools ran.
    pub tools: Vec<Tool>,
    pub scenarios: Vec<ScenarioReport>,
}

impl RunReport {
    /// Whether every measured phase succeeded.
    pub fn all_ok(&self) -> bool {
        self.scenarios
            .iter()
            .flat_map(|s| s.results.iter())
            .all(|r| r.ok)
    }

    /// Renders the human-facing markdown report.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Driven benchmark run\n\n");
        out.push_str(&format!("- **When (UTC):** {}\n", self.started_at));
        out.push_str(&format!(
            "- **Scale:** {} (seed {})\n",
            self.scale, self.seed
        ));
        out.push_str(&format!(
            "- **Host:** {} ({} logical CPUs)\n",
            self.host, self.cpus
        ));
        out.push_str(&format!("- **Driven:** {}\n", self.driven_version));
        if let Some(version) = &self.rclone_version {
            out.push_str(&format!("- **rclone:** {version}\n"));
        }
        out.push('\n');

        for scenario in &self.scenarios {
            let spec = &scenario.spec;
            out.push_str(&format!("## {} fixture\n\n", spec.shape));
            out.push_str(&format!(
                "{} files, {} total{}\n\n",
                spec.files,
                human_bytes(spec.total_bytes()),
                match spec.shape {
                    crate::fixture::Shape::TinyDeep =>
                        format!(", nested {} directories deep", spec.depth),
                    crate::fixture::Shape::Huge => String::new(),
                }
            ));
            out.push_str(
                "| Tool | Phase | Wall s | Scan s | MiB/s | files/s | Files | Bytes | API calls | CPU s | Peak RSS | Conc | Notes |\n",
            );
            out.push_str(
                "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |\n",
            );
            for r in &scenario.results {
                out.push_str(&format!(
                    "| {} | {} | {:.1} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                    r.tool,
                    r.phase,
                    r.wall_secs,
                    opt_f(r.scan_secs, 1),
                    // Two places: on the tiny-files shape the byte rate is a
                    // small fraction of a MiB/s and one place rounds it to 0.0.
                    opt_f(r.mib_per_sec(), 2),
                    opt_f(r.files_per_sec(), 1),
                    opt_u(r.files_transferred),
                    r.bytes_transferred.map(human_bytes).unwrap_or_else(dash),
                    opt_u(r.api_calls),
                    opt_f(r.cpu_secs, 1),
                    r.peak_rss_bytes.map(human_bytes).unwrap_or_else(dash),
                    opt_u(r.concurrency),
                    r.detail.clone().unwrap_or_else(|| {
                        if r.ok {
                            String::new()
                        } else {
                            "FAILED".to_string()
                        }
                    }),
                ));
            }
            out.push('\n');
        }

        out.push_str("## Reading these numbers\n\n");
        out.push_str(
            "The two tools do different amounts of work, on purpose - see `bench/README.md`, \
             \"What is and is not apples-to-apples\". In short: Driven maintains a local state \
             database and hashes file content, which costs it time on the cold phase and buys it \
             precision on the incremental phase; rclone compares size and modification time and \
             keeps no database. `API calls` is instrumented inside Driven's Drive client and has \
             no rclone equivalent, so a blank cell there means \"not measurable\", not zero.\n\n\
             `Scan s` is the time Driven spent walking and hashing before the first upload \
             started, so `Wall s - Scan s` is the upload half. That split is what says WHERE a \
             slow run went: a large scan share means the local walk is the constraint, a small \
             one means Drive round-trips are. rclone interleaves listing with transferring and \
             exposes no such boundary, so its cell is blank.\n",
        );
        out
    }

    /// Writes `<dir>/<timestamp>.md` and `<dir>/<timestamp>.json`, returning
    /// both paths.
    pub fn write_to(&self, dir: &Path) -> Result<(PathBuf, PathBuf)> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating the results directory {}", dir.display()))?;
        let stem = self.started_at.replace([':', '-'], "");
        let md = dir.join(format!("{stem}.md"));
        let json = dir.join(format!("{stem}.json"));
        std::fs::write(&md, self.to_markdown())
            .with_context(|| format!("writing {}", md.display()))?;
        std::fs::write(&json, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", json.display()))?;
        Ok((md, json))
    }
}

fn dash() -> String {
    "-".to_string()
}

fn opt_f(value: Option<f64>, places: usize) -> String {
    value.map_or_else(dash, |v| format!("{v:.places$}"))
}

fn opt_u(value: Option<u64>) -> String {
    value.map_or_else(dash, |v| v.to_string())
}

/// Formats "now" as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Hand-rolled rather than pulling in a date library: the harness needs exactly
/// one timestamp and the crate is deliberately dependency-light.
pub fn utc_timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to (y, m, d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Shape;
    use crate::tools::Phase;

    fn spec() -> FixtureSpec {
        FixtureSpec {
            shape: Shape::TinyDeep,
            files: 100,
            huge_file_bytes: 0,
            depth: 4,
            seed: 3,
        }
    }

    fn result(tool: Tool, ok: bool) -> PhaseResult {
        PhaseResult {
            tool,
            phase: Phase::Cold,
            wall_secs: 10.0,
            cpu_secs: Some(2.5),
            peak_rss_bytes: Some(1 << 20),
            files_transferred: Some(100),
            bytes_transferred: Some(10 * 1_048_576),
            api_calls: (tool == Tool::Driven).then_some(120),
            concurrency: Some(8),
            scan_secs: (tool == Tool::Driven).then_some(3.0),
            ok,
            detail: None,
        }
    }

    fn report(results: Vec<PhaseResult>) -> RunReport {
        RunReport {
            started_at: "2026-07-25T16:00:00Z".into(),
            scale: "smoke".into(),
            seed: 3,
            host: "windows/x86_64".into(),
            cpus: 8,
            driven_version: "2.3.0".into(),
            rclone_version: Some("rclone v1.74.4".into()),
            tools: vec![Tool::Driven, Tool::Rclone],
            scenarios: vec![ScenarioReport {
                spec: spec(),
                results,
            }],
        }
    }

    #[test]
    fn markdown_has_a_row_per_result_and_names_both_tools() {
        let md = report(vec![result(Tool::Driven, true), result(Tool::Rclone, true)]).to_markdown();
        assert!(md.contains("| driven | cold |"));
        assert!(md.contains("| rclone | cold |"));
        assert!(md.contains("rclone v1.74.4"));
        assert!(md.contains("tiny-deep fixture"));
    }

    #[test]
    fn an_unmeasurable_cell_renders_as_a_dash_not_a_zero() {
        let md = report(vec![result(Tool::Rclone, true)]).to_markdown();
        let row = md
            .lines()
            .find(|l| l.starts_with("| rclone |"))
            .expect("rclone row");
        // rclone has no request counter; the API column must be a dash so the
        // table never claims it made zero requests.
        assert!(row.contains(" - |"), "expected a dash cell in: {row}");
    }

    #[test]
    fn the_scan_column_shows_drivens_split_and_stays_blank_for_rclone() {
        let md = report(vec![result(Tool::Driven, true), result(Tool::Rclone, true)]).to_markdown();
        assert!(md.contains("| Scan s |"), "the table must carry the column");
        let driven = md
            .lines()
            .find(|l| l.starts_with("| driven |"))
            .expect("driven row");
        assert!(
            driven.contains("| 3.0 |"),
            "driven must report its scan time, got: {driven}"
        );
        let rclone = md
            .lines()
            .find(|l| l.starts_with("| rclone |"))
            .expect("rclone row");
        // rclone interleaves listing with transferring; the cell must be a dash,
        // never a zero that would read as "no scan needed".
        assert!(
            !rclone.contains("| 0.0 |"),
            "rclone must not claim a zero scan, got: {rclone}"
        );
    }

    #[test]
    fn all_ok_is_false_when_any_phase_failed() {
        assert!(report(vec![result(Tool::Driven, true)]).all_ok());
        assert!(!report(vec![result(Tool::Driven, false)]).all_ok());
    }

    #[test]
    fn writes_both_a_markdown_and_a_json_file() {
        let dir = tempfile::tempdir().unwrap();
        let (md, json) = report(vec![result(Tool::Driven, true)])
            .write_to(dir.path())
            .unwrap();
        assert!(md.exists() && json.exists());
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        assert_eq!(parsed["scale"], "smoke");
        assert_eq!(parsed["scenarios"][0]["results"][0]["tool"], "driven");
    }

    #[test]
    fn timestamp_is_iso_8601_utc() {
        let ts = utc_timestamp();
        assert_eq!(ts.len(), 20, "unexpected timestamp {ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
        // A leap day and the day after it, the classic off-by-one.
        assert_eq!(civil_from_days(18_321), (2020, 2, 29));
        assert_eq!(civil_from_days(18_322), (2020, 3, 1));
    }
}
