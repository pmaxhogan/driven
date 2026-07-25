//! Runs a child process and measures it with OS accounting.
//!
//! Every benchmarked tool - including Driven itself - runs as a CHILD of the
//! harness, so all of them are measured the same way. That symmetry is the whole
//! point of this module: if Driven's engine ran in-process, its "CPU time" would
//! silently include fixture generation, report rendering and the harness's own
//! Drive calls, and would not be comparable to rclone's.
//!
//! Wall time is measured by the harness around spawn/wait. CPU time and peak
//! memory come from the OS:
//!
//! - Windows (the primary platform) reports both exactly, per process, via
//!   `GetProcessTimes` and `GetProcessMemoryInfo` on the child handle - which
//!   stay valid after exit for as long as the handle is open.
//! - Unix reads `getrusage(RUSAGE_CHILDREN)` around the child. CPU time is an
//!   exact delta because the harness never runs two children at once. Peak RSS
//!   is a high-water mark across ALL reaped children, so it is reported only
//!   when this child raised it; otherwise it is `None` rather than a number that
//!   belongs to an earlier child.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// What one measured child run produced.
#[derive(Debug, Clone)]
pub struct ProcMetrics {
    /// Wall-clock time from spawn to exit.
    pub wall: Duration,
    /// User + system CPU time, when the OS could attribute it.
    pub cpu: Option<Duration>,
    /// Peak resident set / working set in bytes, when the OS could attribute it.
    pub peak_rss_bytes: Option<u64>,
    /// The process exit code, when it exited normally.
    pub exit_code: Option<i32>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

impl ProcMetrics {
    /// Whether the child exited zero.
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Spawns `cmd`, captures its output, waits for it, and returns timings.
///
/// Output is drained on background threads so a chatty child can never deadlock
/// on a full pipe buffer.
pub fn run_measured(cmd: &mut Command) -> Result<ProcMetrics> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let before = rusage_children();
    let started = Instant::now();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {:?}", cmd.get_program()))?;

    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");

    let (stdout, stderr) = std::thread::scope(|scope| {
        let out = scope.spawn(move || {
            let mut buf = String::new();
            let _ = stdout_pipe.read_to_string(&mut buf);
            buf
        });
        let err = scope.spawn(move || {
            let mut buf = String::new();
            let _ = stderr_pipe.read_to_string(&mut buf);
            buf
        });
        (
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
        )
    });

    let status = child.wait().context("waiting for child")?;
    let wall = started.elapsed();

    // Query the handle BEFORE `child` drops (Windows closes it on drop).
    let (cpu, peak_rss_bytes) = child_resource_usage(&child, before);

    Ok(ProcMetrics {
        wall,
        cpu,
        peak_rss_bytes,
        exit_code: status.code(),
        stdout,
        stderr,
    })
}

#[cfg(windows)]
mod platform {
    use std::os::windows::io::AsRawHandle;
    use std::time::Duration;

    use windows::Win32::Foundation::{FILETIME, HANDLE};
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetProcessTimes;

    /// No baseline is needed on Windows: every counter is per-process. A unit
    /// STRUCT rather than `()` so the shared call site stays lint-clean.
    pub struct Baseline;

    pub fn baseline() -> Baseline {
        Baseline
    }

    /// Converts a `FILETIME` (100-nanosecond ticks) to a `Duration`.
    fn filetime_to_duration(ft: FILETIME) -> Duration {
        let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
        Duration::from_nanos(ticks.saturating_mul(100))
    }

    pub fn usage(
        child: &std::process::Child,
        _before: Baseline,
    ) -> (Option<Duration>, Option<u64>) {
        let handle = HANDLE(child.as_raw_handle());

        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let cpu =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }
                .ok()
                .map(|()| filetime_to_duration(kernel) + filetime_to_duration(user));

        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let peak = unsafe { GetProcessMemoryInfo(handle, &mut counters, size) }
            .ok()
            .map(|()| counters.PeakWorkingSetSize as u64);

        (cpu, peak)
    }
}

#[cfg(unix)]
mod platform {
    use std::time::Duration;

    /// The `(cpu, peak_rss)` reading taken before the child started.
    pub type Baseline = Option<(Duration, u64)>;

    fn read() -> Baseline {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: `usage` is a valid, fully-initialised rusage for the kernel to
        // write into; RUSAGE_CHILDREN is a documented constant.
        if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) } != 0 {
            return None;
        }
        let to_duration = |tv: libc::timeval| {
            Duration::new(tv.tv_sec as u64, (tv.tv_usec as u32).saturating_mul(1000))
        };
        // Linux reports ru_maxrss in kilobytes, macOS in bytes.
        let scale: u64 = if cfg!(target_os = "macos") { 1 } else { 1024 };
        Some((
            to_duration(usage.ru_utime) + to_duration(usage.ru_stime),
            (usage.ru_maxrss.max(0) as u64).saturating_mul(scale),
        ))
    }

    pub fn baseline() -> Baseline {
        read()
    }

    pub fn usage(
        _child: &std::process::Child,
        before: Baseline,
    ) -> (Option<Duration>, Option<u64>) {
        let (Some((cpu_before, rss_before)), Some((cpu_after, rss_after))) = (before, read())
        else {
            return (None, None);
        };
        // Children run one at a time, so the CPU delta is exactly this child's.
        let cpu = cpu_after.checked_sub(cpu_before);
        // ru_maxrss is a high-water mark over every child reaped so far; it only
        // tells us about THIS child when this child raised it.
        let peak = (rss_after > rss_before).then_some(rss_after);
        (cpu, peak)
    }
}

fn rusage_children() -> platform::Baseline {
    platform::baseline()
}

fn child_resource_usage(
    child: &std::process::Child,
    before: platform::Baseline,
) -> (Option<Duration>, Option<u64>) {
    platform::usage(child, before)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawns the test binary itself in a mode that just exits, which is
    /// portable in a way that `sleep` / `timeout` are not.
    fn trivial_child() -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        // A filter that matches no test: the harness starts, reports 0 tests and
        // exits 0 - cheap, and available on every platform.
        cmd.arg("--exact").arg("driven_bench_no_such_test");
        cmd
    }

    #[test]
    fn measures_a_successful_child() {
        let m = run_measured(&mut trivial_child()).unwrap();
        assert!(m.success(), "child failed: {}", m.stderr);
        assert!(m.wall > Duration::ZERO);
        // stdout of a libtest run always mentions how many tests ran.
        assert!(
            m.stdout.contains("test result") || m.stdout.contains("running"),
            "unexpected child stdout: {}",
            m.stdout
        );
    }

    #[test]
    fn reports_a_failing_child_without_erroring() {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.arg("--this-flag-does-not-exist");
        let m = run_measured(&mut cmd).unwrap();
        assert!(!m.success(), "expected a non-zero exit");
    }

    #[test]
    fn missing_binary_is_an_error_not_a_panic() {
        let mut cmd = Command::new("driven-bench-definitely-not-a-real-binary");
        assert!(run_measured(&mut cmd).is_err());
    }
}
