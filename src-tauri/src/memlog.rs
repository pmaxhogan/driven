//! Process-memory watchdog (2026-08-14 OOM incident follow-up).
//!
//! Samples the process's resident-set size on a fixed cadence and logs it at
//! INFO whenever it has moved materially, so the SPEC s18 diagnostic bundle's
//! `logs/` shows WHEN memory started growing and how fast. The 2026-08-14
//! incident (an 88 GB file buffered wholesale during the reconcile resume)
//! grew >10 GB in ~30 s with ZERO log lines before the OS killed the app;
//! with this watchdog the bundle carries a line per sample during any such
//! runaway, and `memory.txt` (see `commands::settings`) stamps the
//! current + peak values at bundle-capture time.
//!
//! The sampler is a detached task, deliberately outside the app's no-orphan
//! quit drain (R3-P1-1): it holds no AppState, no DB handle, and no network
//! resource - it only reads a per-OS process counter and writes two atomics -
//! so process exit is its teardown.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const TARGET: &str = "driven::app::memlog";

/// How often the resident-set size is sampled. Frequent enough that a
/// disk-speed runaway (the incident sustained ~300 MB/s of growth) is caught
/// several times before the OS kills the process, cheap enough to be free in
/// steady state (one syscall per tick).
const SAMPLE_INTERVAL: Duration = Duration::from_secs(15);

/// Log a sample only when RSS moved at least this far from the last LOGGED
/// value, so a steady-state app logs roughly never and a runaway logs every
/// tick. 128 MiB is far above normal jitter and far below the gigabytes an
/// unbounded buffer accumulates between two samples.
const LOG_DELTA_BYTES: u64 = 128 * 1024 * 1024;

/// Last-sampled resident-set size, bytes. 0 = not sampled yet / unsupported.
static CURRENT_RSS: AtomicU64 = AtomicU64::new(0);
/// Peak sampled resident-set size, bytes.
static PEAK_RSS: AtomicU64 = AtomicU64::new(0);

/// Trailing window of `(unix_ms, rss_bytes)` samples for the diagnostic
/// bundle's `memory.txt`, so a bundle shows the TREND (flat vs runaway), not
/// just two numbers. 60 entries at the 15 s cadence = the last ~15 minutes.
/// A plain `Mutex<VecDeque>`: written once per sample tick, read once per
/// bundle export - nowhere near contention.
const RECENT_SAMPLES_CAP: usize = 60;
static RECENT_SAMPLES: std::sync::Mutex<std::collections::VecDeque<(i64, u64)>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

/// The trailing sample window, oldest first.
pub fn recent_samples() -> Vec<(i64, u64)> {
    RECENT_SAMPLES
        .lock()
        .map(|q| q.iter().copied().collect())
        .unwrap_or_default()
}

/// `(current, peak)` resident-set bytes for the diagnostic bundle's
/// `memory.txt`. Takes a FRESH sample first, so a bundle captured before the
/// first tick (or during a fast runaway) still reports live numbers.
pub fn snapshot() -> (u64, u64) {
    if let Some(rss) = process_rss_bytes() {
        record(rss);
    }
    (
        CURRENT_RSS.load(Ordering::Relaxed),
        PEAK_RSS.load(Ordering::Relaxed),
    )
}

fn record(rss: u64) {
    CURRENT_RSS.store(rss, Ordering::Relaxed);
    PEAK_RSS.fetch_max(rss, Ordering::Relaxed);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if let Ok(mut q) = RECENT_SAMPLES.lock() {
        if q.len() >= RECENT_SAMPLES_CAP {
            q.pop_front();
        }
        q.push_back((now_ms, rss));
    }
}

/// Spawn the sampling loop. Called once from setup; a platform where the
/// counter is unreadable logs one line and exits the task.
pub fn spawn_sampler() {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_logged: u64 = 0;
        loop {
            ticker.tick().await;
            let Some(rss) = process_rss_bytes() else {
                tracing::warn!(target: TARGET, "cannot read process RSS on this platform; memory watchdog exiting");
                return;
            };
            record(rss);
            if last_logged == 0 || rss.abs_diff(last_logged) >= LOG_DELTA_BYTES {
                tracing::info!(
                    target: TARGET,
                    rss_mb = rss / (1024 * 1024),
                    peak_mb = PEAK_RSS.load(Ordering::Relaxed) / (1024 * 1024),
                    "process memory sample"
                );
                last_logged = rss;
            }
        }
    });
}

/// Resident-set size of THIS process, in bytes.
#[cfg(target_os = "linux")]
fn process_rss_bytes() -> Option<u64> {
    // /proc/self/statm field 2 = resident pages.
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page <= 0 {
        return None;
    }
    Some(resident_pages * page as u64)
}

/// Resident-set size of THIS process, in bytes.
#[cfg(target_os = "macos")]
fn process_rss_bytes() -> Option<u64> {
    // libproc's proc_pidinfo(PROC_PIDTASKINFO): a stable, exported ABI.
    // Declared locally because `libc` does not expose the task-info flavor.
    #[repr(C)]
    struct ProcTaskInfo {
        pti_virtual_size: u64,
        pti_resident_size: u64,
        pti_total_user: u64,
        pti_total_system: u64,
        pti_threads_user: u64,
        pti_threads_system: u64,
        pti_policy: i32,
        pti_faults: i32,
        pti_pageins: i32,
        pti_cow_faults: i32,
        pti_messages_sent: i32,
        pti_messages_received: i32,
        pti_syscalls_mach: i32,
        pti_syscalls_unix: i32,
        pti_csw: i32,
        pti_threadnum: i32,
        pti_numrunning: i32,
        pti_priority: i32,
    }
    const PROC_PIDTASKINFO: libc::c_int = 4;
    extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }
    let mut info: ProcTaskInfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<ProcTaskInfo>() as libc::c_int;
    let got = unsafe {
        proc_pidinfo(
            std::process::id() as libc::c_int,
            PROC_PIDTASKINFO,
            0,
            std::ptr::from_mut(&mut info).cast::<libc::c_void>(),
            size,
        )
    };
    if got < size {
        return None;
    }
    Some(info.pti_resident_size)
}

/// Resident-set size (working set) of THIS process, in bytes.
#[cfg(windows)]
fn process_rss_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        return None;
    }
    // WorkingSetSize tracks what Task Manager attributes to the process; the
    // incident's runaway `Vec` growth moves it in lockstep with commit.
    Some(counters.WorkingSetSize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-OS counter must be readable on every OS we ship (a None here
    /// means the watchdog silently disarms itself on this platform), and a
    /// test process trivially occupies at least a megabyte.
    #[test]
    fn rss_is_readable_and_plausible() {
        let rss = process_rss_bytes().expect("process RSS must be readable on this platform");
        assert!(
            rss > 1024 * 1024,
            "a running test process holds more than 1 MiB resident; got {rss}"
        );
    }

    /// `snapshot()` self-samples so a diagnostic bundle captured before the
    /// first sampler tick still reports live numbers, and every sample lands
    /// in the trailing window the bundle prints as the trend.
    #[test]
    fn snapshot_self_samples_and_feeds_the_trailing_window() {
        let (current, peak) = snapshot();
        assert!(current > 0, "snapshot must take a fresh sample");
        assert!(peak >= current || peak > 0);
        let samples = recent_samples();
        assert!(
            !samples.is_empty(),
            "the snapshot's sample must land in the trailing window"
        );
        let (ts_ms, rss) = *samples.last().unwrap();
        assert!(ts_ms > 0, "samples carry a wall-clock timestamp");
        assert!(rss > 0, "samples carry the sampled RSS");
    }
}
