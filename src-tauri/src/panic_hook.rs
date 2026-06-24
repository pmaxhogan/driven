//! Panic hook installing a crash-dump writer (SPEC s17).
//!
//! Installs a `std::panic` hook that writes the panic message + a forced
//! backtrace to `<config_dir>/driven/logs/crash-<timestamp>.txt`, then chains
//! the previous hook so the default abort/print behaviour is preserved. The
//! crash files surface in the diagnostic bundle (SPEC s18).
//!
//! Redaction note (SPEC s17 + s18): the crash file is written verbatim
//! (panic message + backtrace) so a developer reading the file on the user's
//! own machine sees the true failure. It is NOT redacted at rest - redaction
//! happens only when the user opts to `export_diagnostic_bundle` (SPEC s18),
//! where every `crash-*.txt` is run through the same token/path/email/file-id
//! redaction pipeline before it leaves the machine. A one-line banner in each
//! crash file records that contract so a recipient of a raw file understands
//! the threat model. The hook itself adds no extra context (no env dump, no
//! settings) beyond what the panic already carries, to keep incidental
//! sensitive data out of the dump.

use std::path::PathBuf;

/// Install the crash-dump panic hook (SPEC s17). Called once from `run()`
/// before the Tauri runtime starts so a panic anywhere - including during
/// `.setup()` / assembly - is captured.
///
/// Chains the previous hook (the default printer / `RUST_BACKTRACE` handler),
/// so installing this never suppresses the standard panic output or the
/// abort-on-panic behaviour; it only adds the on-disk crash dump.
pub fn install() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A crash dump must never itself panic: every fallible step here is
        // best-effort and its error is swallowed.
        write_crash_dump(info);
        prev(info);
    }));
}

/// Write one crash dump file. Best-effort: any error (no log dir, read-only
/// disk, full disk) is ignored so the hook can never panic re-entrantly.
///
/// The closure parameter is left un-annotated so this compiles across the
/// Rust `PanicInfo` -> `PanicHookInfo` type rename (the closure infers the
/// runtime's actual hook-info type).
fn write_crash_dump(info: &std::panic::PanicHookInfo<'_>) {
    let Some(dir) = log_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("crash-{}.txt", now_filename_safe()));
    // `Backtrace::force_capture` honours neither RUST_BACKTRACE nor the
    // panic=abort setting for the capture itself - it always resolves frames,
    // which is what a post-mortem dump needs.
    let backtrace = std::backtrace::Backtrace::force_capture();
    let body = format!(
        "Driven crash dump (SPEC s17)\n\
         NOTE: written verbatim, NOT redacted at rest. Redaction is applied \
         only by export_diagnostic_bundle (SPEC s18) before sharing.\n\
         time: {ts}\n\
         ----\n\
         {info}\n\
         {backtrace}\n",
        ts = now_iso8601(),
    );
    let _ = std::fs::write(&path, body);
}

/// `<config_dir>/driven/logs`, matching Tauri's `app_config_dir()`
/// (`config_dir() + identifier`) for identifier `app.driven`, so crash dumps
/// land next to the tracing logs the diagnostic bundle collects (SPEC s18).
///
/// Resolved from platform env conventions rather than the Tauri path resolver
/// because the hook is installed before the app handle exists (and must keep
/// working for panics that happen during startup, before `.setup()`).
/// `None` if the home / config dir cannot be determined.
fn log_dir() -> Option<PathBuf> {
    config_dir().map(|c| c.join("app.driven").join("logs"))
}

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

/// Seconds since the Unix epoch, formatted as a filename-safe sortable
/// stamp. `crash-<unix-seconds>-<nanos>.txt` is monotone-ish and never
/// contains a path separator or `:` (so it is valid on every platform).
/// Two panics in the same second still get distinct files via the nanos.
fn now_filename_safe() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("{}-{:09}", d.as_secs(), d.subsec_nanos()),
        // Clock before the epoch (or a broken clock): fall back to a constant
        // so we still write a uniquely-suffixed file rather than no file.
        Err(_) => "0-000000000".to_string(),
    }
}

/// Best-effort human-readable UTC timestamp for the dump header. Avoids a
/// chrono/time dependency: derived directly from the Unix epoch seconds.
fn now_iso8601() -> String {
    let secs = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => 0,
    };
    format_unix_utc(secs)
}

/// Format Unix epoch seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC), using a plain
/// civil-from-days algorithm (Howard Hinnant's `civil_from_days`). No
/// external crate, no panics.
fn format_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // civil_from_days: days since 1970-01-01 -> (year, month, day).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!(
        "{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z",
        year = year,
        m = m,
        d = d,
        hh = hh,
        mm = mm,
        ss = ss,
    )
}
