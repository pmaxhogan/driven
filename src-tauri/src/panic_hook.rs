//! Panic hook installing a crash-dump writer (SPEC s17).
//!
//! Installs a `std::panic` hook that writes the panic message + a forced
//! backtrace to `<config_dir>/driven/logs/crash-<timestamp>.txt`, then chains
//! the previous hook so the default abort/print behaviour is preserved. The
//! crash files surface in the diagnostic bundle (SPEC s18).

/// Install the crash-dump panic hook (SPEC s17). Idempotent enough to call
/// once from `.setup()` before the orchestrators spawn.
///
/// TODO(M5): per SPEC s17 -
/// ```ignore
/// let prev = std::panic::take_hook();
/// std::panic::set_hook(Box::new(move |info| {
///     write_crash_dump(info); // log_dir().join(format!("crash-{}.txt", now_filename_safe()))
///     prev(info);
/// }));
/// ```
/// `write_crash_dump` writes `format!("{info}\n{}", Backtrace::force_capture())`
/// (errors ignored - a crash dump must never itself panic).
pub fn install() {
    todo!("M5: install SPEC s17 panic hook -> write crash-<ts>.txt with forced backtrace, chain prev hook")
}
