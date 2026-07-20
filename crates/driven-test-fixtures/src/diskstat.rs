//! [`FakeDiskBusyProbe`] - a test double implementing
//! [`driven_diskstat::DiskBusyProbe`].
//!
//! Tests of the adaptive-parallelism controller drive the disk-saturation gate
//! deterministically via a fixed reading, or a scripted sequence of readings,
//! instead of touching the real per-OS backend (`/proc/diskstats`, PDH, IOKit).

use std::sync::Mutex;

use driven_diskstat::{DiskBusy, DiskBusyProbe};

/// A [`DiskBusyProbe`] that returns a caller-controlled reading.
///
/// Construct with [`not_saturated`](Self::not_saturated) /
/// [`saturated`](Self::saturated) for a constant reading, or
/// [`scripted`](Self::scripted) for a sequence consumed one-per-`sample`
/// (the last entry repeats once the script is exhausted).
#[derive(Debug)]
pub struct FakeDiskBusyProbe {
    /// The reading returned when the script is empty / exhausted.
    fixed: DiskBusy,
    /// Remaining scripted readings, consumed front-to-back.
    script: Mutex<std::collections::VecDeque<DiskBusy>>,
}

impl FakeDiskBusyProbe {
    /// A probe that always reports the disk as NOT saturated (0% busy) - the
    /// common case that lets the controller grow/shrink on throughput alone.
    #[must_use]
    pub fn not_saturated() -> Self {
        Self::constant(DiskBusy::Fraction(0.0))
    }

    /// A probe that always reports the disk as saturated (100% busy).
    #[must_use]
    pub fn saturated() -> Self {
        Self::constant(DiskBusy::Fraction(1.0))
    }

    /// A probe that always reports [`DiskBusy::Unknown`] (an unreadable device),
    /// exercising the fail-open path.
    #[must_use]
    pub fn unknown() -> Self {
        Self::constant(DiskBusy::Unknown)
    }

    /// A probe returning a constant reading.
    #[must_use]
    pub fn constant(reading: DiskBusy) -> Self {
        Self {
            fixed: reading,
            script: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// A probe returning each reading in turn; once the script is exhausted every
    /// further `sample` returns the final scripted reading.
    #[must_use]
    pub fn scripted(readings: impl IntoIterator<Item = DiskBusy>) -> Self {
        let script: std::collections::VecDeque<DiskBusy> = readings.into_iter().collect();
        let fixed = script.back().copied().unwrap_or(DiskBusy::Unknown);
        Self {
            fixed,
            script: Mutex::new(script),
        }
    }
}

impl DiskBusyProbe for FakeDiskBusyProbe {
    fn sample(&self) -> DiskBusy {
        let mut s = self.script.lock().unwrap_or_else(|e| e.into_inner());
        s.pop_front().unwrap_or(self.fixed)
    }
}
