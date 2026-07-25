//! The tools under test, and how each one is driven and measured.
//!
//! Both tools run as child processes measured by [`crate::procstat`], upload
//! into the same Drive folder tree under the same account, and are given the
//! same source directory. What differs is unavoidable and is reported rather
//! than hidden - see bench/README.md, "What is and is not apples-to-apples".

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::agent::{AgentMetrics, METRICS_PREFIX};
use crate::creds::BenchCreds;
use crate::procstat::run_measured;

/// A tool the suite can benchmark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Tool {
    /// Driven's real engine, run headlessly in a child process.
    Driven,
    /// `rclone copy` against a Drive remote built from the same credentials.
    Rclone,
}

impl Tool {
    /// The stable slug used in folder names, JSON and report tables.
    pub fn slug(self) -> &'static str {
        match self {
            Tool::Driven => "driven",
            Tool::Rclone => "rclone",
        }
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

/// Which half of a scenario a measurement belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// First upload of the tree into an empty destination.
    Cold,
    /// Re-run after a small deterministic change to the same tree.
    Incremental,
}

impl Phase {
    pub fn slug(self) -> &'static str {
        match self {
            Phase::Cold => "cold",
            Phase::Incremental => "incremental",
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.slug())
    }
}

/// One measured tool run.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseResult {
    pub tool: Tool,
    pub phase: Phase,
    /// Wall-clock seconds for the whole child process.
    pub wall_secs: f64,
    /// CPU seconds, when the OS attributed them.
    pub cpu_secs: Option<f64>,
    /// Peak working set in bytes, when the OS attributed it.
    pub peak_rss_bytes: Option<u64>,
    /// Files the tool reported transferring.
    pub files_transferred: Option<u64>,
    /// Bytes the tool reported transferring.
    pub bytes_transferred: Option<u64>,
    /// Drive requests, when the tool can be instrumented. Only Driven can be:
    /// rclone exposes no request counter, so this stays `None` for it rather
    /// than being guessed from transfer counts.
    pub api_calls: Option<u64>,
    /// Upload concurrency the tool ran at, for the report to show alongside the
    /// timings.
    pub concurrency: Option<u64>,
    /// Seconds spent walking and hashing before any upload began, when the tool
    /// can report it. Only Driven can: it is the number that says whether a slow
    /// run was bound by the local scan or by Drive round-trips. rclone
    /// interleaves listing with transferring and exposes no such boundary, so
    /// this stays `None` for it rather than being invented.
    pub scan_secs: Option<f64>,
    /// Whether the child exited zero.
    pub ok: bool,
    /// A short human-facing note - the failure reason when `ok` is false.
    pub detail: Option<String>,
}

impl PhaseResult {
    /// Throughput in mebibytes per second, or `None` when nothing moved.
    pub fn mib_per_sec(&self) -> Option<f64> {
        let bytes = self.bytes_transferred?;
        if bytes == 0 || self.wall_secs <= 0.0 {
            return None;
        }
        Some(bytes as f64 / 1_048_576.0 / self.wall_secs)
    }

    /// Files per second, or `None` when nothing moved.
    pub fn files_per_sec(&self) -> Option<f64> {
        let files = self.files_transferred?;
        if files == 0 || self.wall_secs <= 0.0 {
            return None;
        }
        Some(files as f64 / self.wall_secs)
    }
}

/// Builds a failed result carrying the reason, so one broken tool degrades to a
/// row that says why instead of aborting the whole suite.
fn failed(tool: Tool, phase: Phase, wall: Duration, detail: String) -> PhaseResult {
    PhaseResult {
        tool,
        phase,
        wall_secs: wall.as_secs_f64(),
        cpu_secs: None,
        peak_rss_bytes: None,
        files_transferred: None,
        bytes_transferred: None,
        api_calls: None,
        concurrency: None,
        scan_secs: None,
        ok: false,
        detail: Some(detail),
    }
}

/// Extracts the agent's metrics line from a child's stdout.
pub fn parse_agent_metrics(stdout: &str) -> Option<AgentMetrics> {
    stdout
        .lines()
        .filter_map(|line| line.trim().strip_prefix(METRICS_PREFIX))
        .next_back()
        .and_then(|json| serde_json::from_str(json).ok())
}

/// Runs one Driven phase by re-invoking this binary's hidden `agent-sync`
/// subcommand, so the engine is measured as a child exactly like rclone is.
pub fn run_driven(
    phase: Phase,
    source: &Path,
    dest_folder_id: &str,
    state_db: &Path,
) -> Result<PhaseResult> {
    let exe = std::env::current_exe().context("locating the bench binary")?;
    let mut cmd = Command::new(exe);
    cmd.arg("agent-sync")
        .arg("--source")
        .arg(source)
        .arg("--dest-folder-id")
        .arg(dest_folder_id)
        .arg("--state-db")
        .arg(state_db)
        // Keep the child's log volume bounded: at info level a million-file run
        // would spend real time formatting lines nobody reads.
        .env("RUST_LOG", "warn");

    let m = run_measured(&mut cmd)?;
    if !m.success() {
        return Ok(failed(
            Tool::Driven,
            phase,
            m.wall,
            format!(
                "driven agent exited {:?}: {}",
                m.exit_code,
                last_lines(&m.stderr, 3)
            ),
        ));
    }

    let Some(agent) = parse_agent_metrics(&m.stdout) else {
        return Ok(failed(
            Tool::Driven,
            phase,
            m.wall,
            "driven agent printed no metrics line".to_string(),
        ));
    };

    // Prefer the durable activity-row counts; fall back to the progress stream,
    // which can lag on a large run.
    let files = if agent.logged_files_uploaded > 0 {
        agent.logged_files_uploaded
    } else {
        agent.files_done
    };
    let bytes = if agent.logged_bytes_uploaded > 0 {
        agent.logged_bytes_uploaded
    } else {
        agent.bytes_done
    };

    Ok(PhaseResult {
        tool: Tool::Driven,
        phase,
        wall_secs: m.wall.as_secs_f64(),
        cpu_secs: m.cpu.map(|c| c.as_secs_f64()),
        peak_rss_bytes: m.peak_rss_bytes,
        files_transferred: Some(files),
        bytes_transferred: Some(bytes),
        api_calls: Some(agent.api.total),
        concurrency: Some(driven_core::adaptive::default_pool_size() as u64),
        scan_secs: agent.scan_ms.map(|ms| ms as f64 / 1000.0),
        ok: agent.errors == 0,
        detail: (agent.errors > 0).then(|| format!("{} executor error(s)", agent.errors)),
    })
}

/// Writes an rclone config for a Drive remote rooted at `folder_id`.
///
/// rclone will not accept a token whose `access_token` is empty (it treats the
/// whole token as unparseable and reports "there's no refresh token"), but it is
/// perfectly happy to refresh a NON-empty token that has already expired. So the
/// config carries a placeholder access token with an expiry in the past, and
/// rclone mints a real one from the refresh token on first use - no separate
/// token-minting request, and the same credential Driven uses.
///
/// The file lands in a caller-owned temp directory (rclone rewrites it with the
/// refreshed token) and is never logged.
pub fn write_rclone_config(path: &Path, creds: &BenchCreds, folder_id: &str) -> Result<()> {
    let token = format!(
        r#"{{"access_token":"driven-bench-placeholder","token_type":"Bearer","refresh_token":"{}","expiry":"2000-01-01T00:00:00Z"}}"#,
        creds.refresh_token
    );
    let config = format!(
        "[bench]\n\
         type = drive\n\
         client_id = {}\n\
         client_secret = {}\n\
         scope = drive\n\
         root_folder_id = {}\n\
         token = {}\n",
        creds.client_id, creds.client_secret, folder_id, token
    );
    std::fs::write(path, config)
        .with_context(|| format!("writing the rclone config to {}", path.display()))?;
    Ok(())
}

/// Parses the `stats` object out of rclone's JSON log.
///
/// rclone prints one final stats record at the end of a run; with
/// `--stats-log-level NOTICE` it is emitted even without `-v`, which matters
/// because the per-file `-v` lines would be a million lines long on the tiny
/// files shape. Returns `(bytes, transfers, errors)`.
pub fn parse_rclone_stats(stderr: &str) -> Option<(u64, u64, u64)> {
    stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        .filter_map(|value| value.get("stats").cloned())
        .rfind(|stats| stats.is_object())
        .map(|stats| {
            let get = |key: &str| stats.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
            (get("bytes"), get("transfers"), get("errors"))
        })
}

/// Runs one rclone phase.
pub fn run_rclone(
    phase: Phase,
    binary: &Path,
    config: &Path,
    source: &Path,
    transfers: u64,
) -> Result<PhaseResult> {
    let mut cmd = Command::new(binary);
    cmd.arg("--config")
        .arg(config)
        .arg("copy")
        .arg(source)
        .arg("bench:")
        .arg("--use-json-log")
        // A single final stats record instead of a per-file log.
        .arg("--stats")
        .arg("100000h")
        .arg("--stats-log-level")
        .arg("NOTICE")
        .arg("--transfers")
        .arg(transfers.to_string())
        .arg("--checkers")
        .arg(transfers.to_string());

    let m = run_measured(&mut cmd)?;
    let stats = parse_rclone_stats(&m.stderr);
    if !m.success() {
        return Ok(failed(
            Tool::Rclone,
            phase,
            m.wall,
            format!(
                "rclone exited {:?}: {}",
                m.exit_code,
                last_lines(&m.stderr, 3)
            ),
        ));
    }

    let (bytes, transfers_done, errors) = stats.unwrap_or((0, 0, 0));
    Ok(PhaseResult {
        tool: Tool::Rclone,
        phase,
        wall_secs: m.wall.as_secs_f64(),
        cpu_secs: m.cpu.map(|c| c.as_secs_f64()),
        peak_rss_bytes: m.peak_rss_bytes,
        files_transferred: Some(transfers_done),
        bytes_transferred: Some(bytes),
        // rclone exposes no request counter.
        api_calls: None,
        concurrency: Some(transfers),
        // rclone interleaves listing and transferring; there is no scan phase
        // to report, so the column stays empty instead of guessing.
        scan_secs: None,
        ok: errors == 0,
        detail: (errors > 0).then(|| format!("{errors} rclone error(s)")),
    })
}

/// Reports the rclone version string, for the report header.
pub fn rclone_version(binary: &Path) -> Option<String> {
    let out = Command::new(binary).arg("version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next().map(|l| l.trim().to_string())
}

/// Finds the rclone binary: an explicit path, then `PATH`.
pub fn find_rclone(explicit: Option<&Path>) -> Option<std::path::PathBuf> {
    if let Some(path) = explicit {
        return path.exists().then(|| path.to_path_buf());
    }
    let name = if cfg!(windows) {
        "rclone.exe"
    } else {
        "rclone"
    };
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

/// The last `n` non-empty lines of a captured stream, for error messages.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real final stats line captured from `rclone v1.74.4`.
    const RCLONE_JSON_LOG: &str = r#"{"time":"2026-07-25T16:34:13-05:00","level":"notice","msg":"there was a message"}
{"time":"2026-07-25T16:34:13-05:00","level":"info","msg":"\nTransferred: 292.969 KiB\n","stats":{"bytes":300000,"checks":0,"deletes":0,"elapsedTime":0.008,"errors":0,"listed":5,"speed":0,"totalBytes":300000,"totalTransfers":3,"transfers":3},"source":"accounting/stats.go:551"}"#;

    #[test]
    fn parses_rclone_stats_from_the_json_log() {
        let (bytes, transfers, errors) = parse_rclone_stats(RCLONE_JSON_LOG).expect("stats");
        assert_eq!(bytes, 300_000);
        assert_eq!(transfers, 3);
        assert_eq!(errors, 0);
    }

    #[test]
    fn rclone_stats_are_none_when_no_stats_record_was_printed() {
        assert!(parse_rclone_stats("not json at all\n{\"level\":\"info\"}").is_none());
    }

    #[test]
    fn rclone_errors_are_read_from_the_stats_record() {
        let log = r#"{"msg":"x","stats":{"bytes":1,"transfers":0,"errors":2}}"#;
        assert_eq!(parse_rclone_stats(log), Some((1, 0, 2)));
    }

    #[test]
    fn agent_metrics_are_taken_from_the_last_marker_line() {
        let stdout = format!(
            "some log noise\n{METRICS_PREFIX}{{\"engine_ms\":1}}\n{METRICS_PREFIX}{{\"engine_ms\":2}}\n"
        );
        assert_eq!(parse_agent_metrics(&stdout).unwrap().engine_ms, 2);
    }

    #[test]
    fn agent_metrics_are_none_without_a_marker() {
        assert!(parse_agent_metrics("nothing here").is_none());
    }

    #[test]
    fn rclone_config_embeds_an_expired_placeholder_so_rclone_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclone.conf");
        let creds = BenchCreds {
            client_id: "cid".into(),
            client_secret: "csec".into(),
            refresh_token: "rt-value".into(),
        };
        write_rclone_config(&path, &creds, "folder-1").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("root_folder_id = folder-1"));
        assert!(text.contains("\"refresh_token\":\"rt-value\""));
        assert!(
            text.contains("\"expiry\":\"2000-01-01T00:00:00Z\""),
            "the placeholder must already be expired so rclone refreshes it"
        );
        assert!(
            !text.contains("\"access_token\":\"\""),
            "rclone rejects an empty access_token outright"
        );
    }

    #[test]
    fn throughput_helpers_handle_the_nothing_moved_case() {
        let mut r = PhaseResult {
            tool: Tool::Driven,
            phase: Phase::Incremental,
            wall_secs: 2.0,
            cpu_secs: None,
            peak_rss_bytes: None,
            files_transferred: Some(0),
            bytes_transferred: Some(0),
            api_calls: None,
            concurrency: None,
            scan_secs: None,
            ok: true,
            detail: None,
        };
        assert!(r.mib_per_sec().is_none());
        assert!(r.files_per_sec().is_none());

        r.bytes_transferred = Some(2 * 1_048_576);
        r.files_transferred = Some(4);
        assert_eq!(r.mib_per_sec().unwrap(), 1.0);
        assert_eq!(r.files_per_sec().unwrap(), 2.0);
    }

    #[test]
    fn last_lines_trims_to_the_tail() {
        assert_eq!(last_lines("a\n\nb\nc\n", 2), "b | c");
    }

    #[test]
    fn find_rclone_rejects_a_missing_explicit_path() {
        assert!(find_rclone(Some(Path::new("no/such/rclone"))).is_none());
    }
}
