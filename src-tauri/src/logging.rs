//! Persistent rolling file logs (issue: "the installed app writes no logs").
//!
//! Until this module existed the app called `tracing_subscriber::fmt::init()`,
//! which writes to STDOUT only. An installed Windows/macOS build has no console
//! attached, so every `tracing::` line was discarded - `<config_dir>/app.driven/logs/`
//! stayed empty since install and the SPEC s18 diagnostic bundle shipped with no
//! `logs/` at all, leaving field bugs undiagnosable.
//!
//! [`init`] now installs a LAYERED subscriber: the original stdout `fmt` layer
//! (unchanged, so `cargo tauri dev` still prints) plus a DAILY-rolling file layer
//! writing `driven.YYYY-MM-DD.log` into [`log_dir`]. [`prune`] bounds that
//! directory (14 days / 25 MB, widened to 250 MB while issue #309 debug
//! logging mode is on) so an always-on background daemon cannot fill the
//! user's disk.
//!
//! Issue #309 (debug logging mode): the filter layer is wrapped in a
//! [`tracing_subscriber::reload`] layer, so [`set_filter`] can raise/restore
//! the LIVE process's verbosity at runtime (no restart) when the user flips
//! the Settings toggle. `debug_mode.rs` owns the policy (persisted expiry,
//! 24h auto-off watchdog); this module owns the mechanism.
//!
//! This module is also the ONE place the log directory is resolved. The panic
//! hook (crash dumps, SPEC s17) and the diagnostic-bundle collector (SPEC s18)
//! both call [`log_dir`] rather than re-deriving the path - they previously
//! disagreed (`<config_dir>/app.driven/logs` vs `<config_dir>/app.driven/driven/logs`),
//! which is why the bundle collected nothing even when crash dumps existed.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{fmt, reload, EnvFilter, Layer as _, Registry};

const TARGET: &str = "driven::logging";

/// Default level filter when `RUST_LOG` is unset. `info` keeps the file useful
/// for field diagnosis without the per-file debug spam a backup run produces.
pub const DEFAULT_FILTER: &str = "info";

/// Issue #309 (debug logging mode): the directive applied to the LIVE filter
/// while debug logging is on. Driven's own crates go to `trace` (per-file
/// activity, IPC command traces, state transitions, reconcile/queue
/// decisions - the exact detail issue #309 asks for); everything else
/// (reqwest, hyper, boa's PAC engine, ...) stays at `info` so the log is still
/// dominated by Driven's own signal rather than drowned in dependency noise.
///
/// Two directive families are needed to cover every `tracing::` call site in
/// this workspace: most calls pass an explicit `target: TARGET` string that
/// is a hand-written `"driven::..."` / `"driven_tls::..."` path (covered by
/// the `driven=trace` / `driven_tls=trace` directives below); calls with no
/// explicit target default to `module_path!()`, i.e. the crate's actual
/// (underscored) library name (the `driven_core=trace`, `driven_s3=trace`,
/// ... directives).
pub const DEBUG_MODE_FILTER: &str = "info,\
driven=trace,\
driven_app_lib=trace,\
driven_core=trace,\
driven_s3=trace,\
driven_sftp=trace,\
driven_drive=trace,\
driven_tls=trace,\
driven_vss=trace,\
driven_backend=trace,\
driven_net=trace,\
driven_crypto=trace,\
driven_remote=trace,\
driven_localfs=trace,\
driven_power=trace,\
driven_diskstat=trace,\
driven_apfs=trace,\
driven_rclone=trace";

/// Default rolling-log total-byte cap (see [`MAX_LOG_TOTAL_BYTES`] doc). Kept
/// as the fallback [`effective_max_log_total_bytes`] restores on debug-mode
/// off.
const DEFAULT_MAX_LOG_TOTAL_BYTES: u64 = 25 * 1024 * 1024;

/// Issue #309: the rolling-log cap while debug logging mode is on. Debug mode
/// raises the filter to `trace` on every Driven crate, which produces far
/// more log volume per day than the steady-state 25 MB budget can hold - a
/// 250 MB budget (SPEC/issue #309 "rotating cap ~250 MB") keeps a full day of
/// verbose logs available for the diagnostic bundle without letting an
/// abandoned toggle fill the user's disk indefinitely (the 24h auto-off in
/// `debug_mode.rs` is the other half of that guarantee).
const DEBUG_MODE_MAX_LOG_TOTAL_BYTES: u64 = 250 * 1024 * 1024;

/// The live rolling-log total-byte cap [`prune`] enforces, switched between
/// [`DEFAULT_MAX_LOG_TOTAL_BYTES`] and [`DEBUG_MODE_MAX_LOG_TOTAL_BYTES`] by
/// [`set_debug_log_cap`]. An `AtomicU64` (not a plain const) because debug
/// logging mode is a runtime toggle, not a compile-time choice.
static MAX_LOG_TOTAL_BYTES: AtomicU64 = AtomicU64::new(DEFAULT_MAX_LOG_TOTAL_BYTES);

/// Widen or restore the rolling-log cap for debug logging mode (issue #309).
/// Best-effort/instant: the next [`prune`] pass (at boot, or from the
/// `debug_mode` watchdog) enforces the new budget; nothing is deleted here.
pub fn set_debug_log_cap(debug_enabled: bool) {
    let cap = if debug_enabled {
        DEBUG_MODE_MAX_LOG_TOTAL_BYTES
    } else {
        DEFAULT_MAX_LOG_TOTAL_BYTES
    };
    MAX_LOG_TOTAL_BYTES.store(cap, Ordering::Relaxed);
}

/// The live rolling-log total-byte cap (see [`MAX_LOG_TOTAL_BYTES`]).
fn effective_max_log_total_bytes() -> u64 {
    MAX_LOG_TOTAL_BYTES.load(Ordering::Relaxed)
}

/// The process's reloadable tracing filter handle (issue #309), installed by
/// [`init`]. `None` until `init` runs (or if a subscriber was already
/// installed by a double-init race / a test harness), in which case
/// [`set_filter`] is a documented no-op rather than a panic - the previous
/// filter keeps running.
static FILTER_HANDLE: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

/// Reload the LIVE tracing filter to `directive` (an `EnvFilter` directive
/// string, e.g. `"info"` or [`DEBUG_MODE_FILTER`]).
///
/// Returns `false` (and leaves the previous filter running) when the reload
/// handle is not installed yet, `directive` fails to parse, or the reload
/// itself errors (the subscriber was replaced from under us) - every case
/// fails closed to "still logging at the old level" rather than silently
/// going filter-less.
pub fn set_filter(directive: &str) -> bool {
    let Some(handle) = FILTER_HANDLE.get() else {
        return false;
    };
    let Ok(new_filter) = EnvFilter::try_new(directive) else {
        return false;
    };
    handle.reload(new_filter).is_ok()
}

/// Filename prefix of a rolling log file. `tracing-appender` joins prefix, the
/// rotation date, and the suffix with `.`, so files are `driven.2026-07-25.log`.
const LOG_PREFIX: &str = "driven";
/// Filename suffix (extension) of a rolling log file.
const LOG_SUFFIX: &str = "log";

/// Retention: delete rolling logs older than this. 14 days comfortably covers
/// "it broke sometime last week, send me a bundle".
const MAX_LOG_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// Keeps the `tracing-appender` non-blocking writer's worker thread alive.
///
/// `tracing_appender::non_blocking` hands back a [`WorkerGuard`] whose `Drop`
/// shuts the writer thread down and flushes. The guard therefore has to outlive
/// every `tracing::` call in the process - if it were dropped at the end of
/// [`init`], the file layer would go silent immediately and we would be back to
/// the empty-`logs/` bug this module exists to fix. Parking it in a process-
/// lifetime static is the deliberate leak: the guard is dropped only at process
/// teardown (in practice never, since `OnceLock` statics are not dropped), which
/// is exactly the lifetime we want.
static FILE_WRITER_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// Install the process-wide tracing subscriber: stdout + a daily rolling file.
///
/// Call ONCE, as early in `run()` as possible. Best-effort about the file half:
/// if the log directory cannot be resolved or created (locked-down profile,
/// read-only disk), the stdout layer is still installed and the app boots - a
/// backup daemon must never fail to start because it could not open a log file.
pub fn init() {
    // Retention runs BEFORE the appender opens today's file, so the size/age
    // sweep never races the writer for the file it is about to append to.
    if let Some(dir) = log_dir() {
        prune(&dir);
    }

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    // Issue #309: wrap the filter in a reload layer so `set_filter` can raise
    // / restore it at runtime with no restart. The handle is parked in
    // `FILTER_HANDLE` BEFORE `try_init` so it is available the instant the
    // subscriber is live; a failed `try_init` below leaves it set but unused
    // (harmless - `set_filter` would just reload a filter nothing reads).
    let (filter_layer, filter_handle) = reload::Layer::new(filter);
    let _ = FILTER_HANDLE.set(filter_handle);

    // The original stdout layer, unchanged: `cargo tauri dev` and any
    // console-attached launch keep printing exactly as before.
    let stdout_layer = fmt::layer();

    // `Option<Layer>` is itself a `Layer` (a `None` is a no-op), so a failed
    // file-appender setup degrades to stdout-only with no branching on the
    // subscriber type.
    let file_layer = file_appender().map(|writer| {
        fmt::layer()
            // A log FILE must never carry terminal escape sequences: they make
            // the file unreadable in a text editor and survive into the
            // diagnostic bundle.
            .with_ansi(false)
            .with_target(true)
            .with_level(true)
            .with_writer(writer)
            .boxed()
    });

    // An `Err` here means a subscriber is already installed (only reachable if
    // `init` is called twice, or under a test harness). Nothing to do and
    // nothing to report - reporting would need the subscriber that did not
    // install.
    if tracing_subscriber::registry()
        .with(filter_layer)
        .with(stdout_layer)
        .with(file_layer)
        .try_init()
        .is_err()
    {
        return;
    }

    match log_dir() {
        Some(dir) => {
            tracing::info!(target: TARGET, dir = %dir.display(), "rolling file logs active")
        }
        None => tracing::warn!(
            target: TARGET,
            "no log directory could be resolved; logging to stdout only"
        ),
    }
}

/// Build the non-blocking daily-rolling file writer, parking its [`WorkerGuard`]
/// in [`FILE_WRITER_GUARD`]. `None` when the directory is unavailable or the
/// appender cannot be constructed.
fn file_appender() -> Option<tracing_appender::non_blocking::NonBlocking> {
    let dir = log_dir()?;
    // No subscriber is installed yet, so a failure here cannot be `tracing::`d.
    // Degrade to stdout-only; `init` emits the warn once the subscriber exists.
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(LOG_PREFIX)
        .filename_suffix(LOG_SUFFIX)
        .build(&dir)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    // `set` fails only if `init` ran twice; the second guard is then dropped
    // here, shutting down the second (unused) writer thread. Correct either way.
    if FILE_WRITER_GUARD.set(guard).is_err() {
        return None;
    }
    Some(writer)
}

/// `<config_dir>/app.driven/logs` - the single source of truth for where
/// Driven's on-disk logs and crash dumps live.
///
/// Equivalent to Tauri's `app_config_dir()` (`config_dir() + identifier`, where
/// `app.driven` is the `tauri.conf.json` identifier) plus `logs`, but resolved
/// from platform env conventions rather than the Tauri path resolver: the panic
/// hook installs before the app handle exists and must keep working for panics
/// during startup. `None` if the home / config dir cannot be determined.
pub fn log_dir() -> Option<PathBuf> {
    if let Some(dir) = data_dir_override() {
        return Some(dir.join("logs"));
    }
    config_dir().map(|c| c.join("app.driven").join("logs"))
}

/// Test/dev seam (agent QA harness): `DRIVEN_DATA_DIR=<absolute dir>` relocates
/// EVERYTHING Driven persists under the platform config dir - the state DB
/// (`<dir>/state.db`) and the logs (`<dir>/logs`) - so an isolated instance can
/// run against a scratch directory without touching (or being blocked by) the
/// real install's `state.db` lock. Unset / empty / relative = no override
/// (relative is rejected for the same reason the XDG spec rejects it: the
/// process cwd is not a stable anchor).
///
/// This is deliberately NOT gated behind another env var: pointing the app at a
/// different data dir is no more dangerous than running a different OS user,
/// and the fault-injection seams that DO change behaviour are separately gated.
pub fn data_dir_override() -> Option<PathBuf> {
    let p = PathBuf::from(non_empty_env(ENV_DATA_DIR)?);
    p.is_absolute().then_some(p)
}

/// Env var name for [`data_dir_override`].
pub const ENV_DATA_DIR: &str = "DRIVEN_DATA_DIR";

/// Platform config dir, equivalent to `dirs::config_dir()` (which Tauri's
/// `app_config_dir()` builds on), hand-resolved so the panic hook carries no
/// extra dependency and no app-handle requirement.
///
/// - Windows: `%APPDATA%` (Roaming AppData).
/// - macOS:   `$HOME/Library/Application Support`.
/// - other:   `$XDG_CONFIG_HOME`, else `$HOME/.config`.
fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        non_empty_env("APPDATA").map(PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        non_empty_env("HOME").map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(xdg) = non_empty_env("XDG_CONFIG_HOME") {
            // XDG spec: relative paths are invalid and must be ignored.
            let p = PathBuf::from(xdg);
            if p.is_absolute() {
                return Some(p);
            }
        }
        non_empty_env("HOME").map(|h| PathBuf::from(h).join(".config"))
    }
}

/// Read an env var, treating an absent OR empty value as "unset".
fn non_empty_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// One rolling log file as the pruner sees it. Deliberately free of `Path` and
/// `SystemTime` so [`plan_prune`] is a pure, exhaustively testable decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFile {
    /// File name only (no directory component).
    pub name: String,
    /// Size on disk, bytes.
    pub size: u64,
    /// How long ago the file was last modified, in seconds.
    pub age_secs: u64,
}

/// True for a name this module owns: `driven.<something>.log`.
///
/// Deliberately narrow. `crash-*.txt` dumps (SPEC s17) live in the same
/// directory and are small, high-value, and NOT ours to delete; nor is any file
/// a user dropped there.
fn is_rolling_log_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("driven.") else {
        return false;
    };
    let Some(date) = rest.strip_suffix(".log") else {
        return false;
    };
    !date.is_empty() && !date.contains('.')
}

/// Decide which rolling logs to delete. Pure: no filesystem access.
///
/// Two independent rules, applied in order:
/// 1. Anything older than `max_age_secs` goes.
/// 2. If the survivors still exceed `max_total_bytes`, delete OLDEST-first until
///    they fit.
///
/// The single newest survivor is never deleted by the size rule - it is the file
/// the appender is about to write to, and a cap smaller than one day of logs
/// must not turn into "delete the log we are currently producing". The age rule
/// has no such exemption: if every file is stale, the whole directory is stale.
///
/// Returns the names to delete, in the order they should be attempted.
pub fn plan_prune(files: &[LogFile], max_age_secs: u64, max_total_bytes: u64) -> Vec<String> {
    // Newest first. Name is the tie-break so the plan is deterministic when two
    // files share an age (same-second mtimes in a test, or a clock with coarse
    // granularity).
    let mut candidates: Vec<&LogFile> = files
        .iter()
        .filter(|f| is_rolling_log_name(&f.name))
        .collect();
    candidates.sort_by(|a, b| a.age_secs.cmp(&b.age_secs).then(b.name.cmp(&a.name)));

    let mut doomed: Vec<String> = Vec::new();
    let mut kept: Vec<&LogFile> = Vec::new();
    for f in candidates {
        if f.age_secs > max_age_secs {
            doomed.push(f.name.clone());
        } else {
            kept.push(f);
        }
    }

    let mut total: u64 = kept.iter().map(|f| f.size).sum();
    while total > max_total_bytes && kept.len() > 1 {
        // `kept` is newest-first, so the last element is the oldest survivor.
        if let Some(oldest) = kept.pop() {
            total = total.saturating_sub(oldest.size);
            doomed.push(oldest.name.clone());
        }
    }
    doomed
}

/// Apply [`plan_prune`] to a real directory. Best-effort in every step: an
/// unreadable directory, an unreadable entry, or a failed delete is logged at
/// debug and skipped. Startup NEVER fails because of log retention.
///
/// Note this runs before the subscriber is installed (see [`init`]), so its
/// `tracing::debug!` lines are themselves discarded - they exist for the case
/// where a future caller prunes at runtime, and to keep the failure path
/// explicit rather than a bare `let _ =`.
pub fn prune(dir: &std::path::Path) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(target: TARGET, dir = %dir.display(), error = %e, "log retention: directory unreadable; skipping");
            return;
        }
    };

    let now = std::time::SystemTime::now();
    let mut files: Vec<LogFile> = Vec::new();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // A file with a modified-time in the future (clock skew, a restored
        // backup) reads as age 0 and is treated as brand new - safer than
        // deleting it.
        let age_secs = metadata
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        files.push(LogFile {
            name,
            size: metadata.len(),
            age_secs,
        });
    }

    for name in plan_prune(
        &files,
        MAX_LOG_AGE.as_secs(),
        effective_max_log_total_bytes(),
    ) {
        let path = dir.join(&name);
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::debug!(target: TARGET, file = %path.display(), error = %e, "log retention: could not delete stale log");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(name: &str, size: u64, age_secs: u64) -> LogFile {
        LogFile {
            name: name.to_string(),
            size,
            age_secs,
        }
    }

    const DAY: u64 = 24 * 60 * 60;
    const AGE_CAP: u64 = 14 * DAY;
    const SIZE_CAP: u64 = 25 * 1024 * 1024;

    #[test]
    fn rolling_log_names_are_recognised() {
        assert!(is_rolling_log_name("driven.2026-07-25.log"));
        // Not ours: crash dumps, the state DB, an unrelated drop-in, and the
        // near-misses (no date segment, extra dots, wrong prefix/suffix).
        assert!(!is_rolling_log_name("crash-1750000000-000000001.txt"));
        assert!(!is_rolling_log_name("state.db"));
        assert!(!is_rolling_log_name("notes.txt"));
        assert!(!is_rolling_log_name("driven.log"));
        assert!(!is_rolling_log_name("driven..log"));
        assert!(!is_rolling_log_name("driven.2026-07-25.old.log"));
        assert!(!is_rolling_log_name("other.2026-07-25.log"));
    }

    #[test]
    fn nothing_is_pruned_when_within_both_budgets() {
        let files = vec![
            f("driven.2026-07-25.log", 1024, 0),
            f("driven.2026-07-24.log", 1024, DAY),
            f("driven.2026-07-11.log", 1024, 13 * DAY),
        ];
        assert!(plan_prune(&files, AGE_CAP, SIZE_CAP).is_empty());
    }

    #[test]
    fn files_older_than_the_age_cap_are_pruned() {
        let files = vec![
            f("driven.2026-07-25.log", 10, 0),
            f("driven.2026-07-10.log", 10, 15 * DAY),
            f("driven.2026-06-01.log", 10, 60 * DAY),
        ];
        let doomed = plan_prune(&files, AGE_CAP, SIZE_CAP);
        assert_eq!(
            doomed,
            vec!["driven.2026-07-10.log", "driven.2026-06-01.log"]
        );
    }

    #[test]
    fn the_age_rule_may_delete_every_file() {
        // The app has not run in months: the whole directory is stale and there
        // is no "keep the newest" exemption for the age rule.
        let files = vec![
            f("driven.2026-01-02.log", 10, 200 * DAY),
            f("driven.2026-01-01.log", 10, 201 * DAY),
        ];
        assert_eq!(plan_prune(&files, AGE_CAP, SIZE_CAP).len(), 2);
    }

    #[test]
    fn the_size_cap_deletes_oldest_first_until_it_fits() {
        // 4 x 10 MB = 40 MB against a 25 MB cap: the two oldest go, leaving 20 MB.
        let mb = 1024 * 1024;
        let files = vec![
            f("driven.2026-07-25.log", 10 * mb, 0),
            f("driven.2026-07-24.log", 10 * mb, DAY),
            f("driven.2026-07-23.log", 10 * mb, 2 * DAY),
            f("driven.2026-07-22.log", 10 * mb, 3 * DAY),
        ];
        let doomed = plan_prune(&files, AGE_CAP, SIZE_CAP);
        assert_eq!(
            doomed,
            vec!["driven.2026-07-22.log", "driven.2026-07-23.log"]
        );
    }

    #[test]
    fn the_newest_file_survives_a_cap_it_alone_exceeds() {
        // One 40 MB day against a 25 MB cap. Deleting it would delete the file
        // the appender is about to write to, so it is kept and the cap is
        // knowingly exceeded until tomorrow's rotation.
        let files = vec![
            f("driven.2026-07-25.log", 40 * 1024 * 1024, 0),
            f("driven.2026-07-24.log", 1024, DAY),
        ];
        assert_eq!(
            plan_prune(&files, AGE_CAP, SIZE_CAP),
            vec!["driven.2026-07-24.log"]
        );
    }

    #[test]
    fn crash_dumps_and_foreign_files_are_never_pruned() {
        // Both are far past the age cap and far past the size cap; neither is a
        // rolling log, so neither is ours to delete.
        let files = vec![
            f(
                "crash-1750000000-000000001.txt",
                50 * 1024 * 1024,
                400 * DAY,
            ),
            f(
                "something-the-user-put-here.txt",
                50 * 1024 * 1024,
                400 * DAY,
            ),
            f("driven.2026-07-25.log", 1024, 0),
        ];
        assert!(plan_prune(&files, AGE_CAP, SIZE_CAP).is_empty());
    }

    #[test]
    fn both_rules_compose() {
        let mb = 1024 * 1024;
        let files = vec![
            f("driven.2026-07-25.log", 20 * mb, 0),
            f("driven.2026-07-24.log", 20 * mb, DAY),
            // Stale: removed by the age rule, so its 20 MB never counts toward
            // the size budget.
            f("driven.2026-06-01.log", 20 * mb, 40 * DAY),
        ];
        let doomed = plan_prune(&files, AGE_CAP, SIZE_CAP);
        assert_eq!(
            doomed,
            vec!["driven.2026-06-01.log", "driven.2026-07-24.log"]
        );
    }

    #[test]
    fn prune_deletes_from_a_real_directory_and_spares_crash_dumps() {
        let dir = tempfile::tempdir().expect("temp dir");
        let stale = dir.path().join("driven.2026-01-01.log");
        let fresh = dir.path().join("driven.2026-07-25.log");
        let crash = dir.path().join("crash-1750000000-000000001.txt");
        std::fs::write(&stale, b"old").expect("write stale");
        std::fs::write(&fresh, b"new").expect("write fresh");
        std::fs::write(&crash, b"boom").expect("write crash");

        // Age the stale file past the cap. `set_modified` is the only way to
        // exercise the real mtime plumbing without sleeping for two weeks.
        let two_months_ago = std::time::SystemTime::now() - Duration::from_secs(60 * DAY);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .and_then(|fh| fh.set_modified(two_months_ago))
            .expect("backdate stale log");

        prune(dir.path());

        assert!(!stale.exists(), "stale rolling log should be deleted");
        assert!(fresh.exists(), "fresh rolling log should survive");
        assert!(crash.exists(), "crash dumps are never pruned");
    }

    #[test]
    fn prune_on_a_missing_directory_is_a_no_op() {
        // Startup must never fail because the log dir does not exist yet.
        let dir = tempfile::tempdir().expect("temp dir");
        prune(&dir.path().join("does-not-exist"));
    }

    #[test]
    fn log_dir_ends_in_the_app_identifier_and_logs() {
        // The exact prefix is platform-dependent (and CI may run without HOME on
        // no platform we support), but when it resolves it must be the shared
        // `<config>/app.driven/logs` the panic hook and the bundle both use.
        if let Some(dir) = log_dir() {
            assert!(dir.ends_with(std::path::Path::new("app.driven").join("logs")));
        }
    }
}
