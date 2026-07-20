//! `driven-diskstat` - per-OS "is the local disk saturated?" reader for the
//! adaptive upload-parallelism controller (DESIGN s11.4.7, s18.2).
//!
//! # What it answers
//!
//! One question, sampled periodically: *are we bottlenecked by the disk right
//! now?* If yes, adding more in-flight uploads cannot raise throughput (the
//! reads that feed them are already disk-bound) and would only hurt, so the
//! controller must not grow the pool. DESIGN s18.2 fixes the signal per-OS:
//!
//! - **Linux:** `/proc/diskstats` field 10 ("time spent doing I/Os", ms) delta
//!   for the device backing the source root; `busy_ms / interval_ms > 0.80` =
//!   saturated.
//! - **macOS:** IOKit `IOBlockStorageDriver` `Statistics` dict, same ratio
//!   (best-effort - see [`RealDiskBusyProbe`] on macOS).
//! - **Windows:** PDH `\PhysicalDisk(_Total)\% Disk Time > 80 %` = saturated.
//!
//! # Shape (mirrors `driven-power`)
//!
//! A tiny [`DiskBusyProbe`] trait, a PURE classifier ([`DiskBusy::is_saturated`] +
//! [`busy_fraction_from_delta`]) unit-tested on every target, and exactly ONE
//! cfg-gated per-OS [`RealDiskBusyProbe`] compiled per target (re-exported
//! cfg-free so the caller wires `RealDiskBusyProbe::new(..)` with no `cfg` at the
//! call site). Tests use `FakeDiskBusyProbe` from `driven-test-fixtures`.
//!
//! # Fail-open (load-bearing, DESIGN s11.4.7)
//!
//! A disk reader that cannot produce a reading - no baseline yet, a parse/FFI
//! error, an unsupported platform - returns [`DiskBusy::Unknown`], and
//! [`DiskBusy::is_saturated`] maps `Unknown` to `false` ("not saturated"). A
//! broken or unavailable reader must NEVER strangle uploads: the worst it can do
//! is let the controller grow when it maybe should not have, which the
//! throughput signal then corrects on the next window. It must never do the
//! reverse (falsely report saturation and pin the pool small).

/// The busy-fraction above which the disk is considered "saturated" (DESIGN
/// s18.2: `> 80 %`). Exceeding this means reducing concurrency will not help and
/// raising it will hurt, so the controller must hold or shrink, never grow.
pub const SATURATION_THRESHOLD: f64 = 0.80;

/// A disk-busy reading (DESIGN s18.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DiskBusy {
    /// The measured busy fraction over the last sampling interval. Nominally in
    /// `0.0..=1.0`, but CAN exceed `1.0` on a device serving overlapping I/O
    /// (per-op "busy time" summed across a queue can exceed wall-clock); callers
    /// only ever compare it against [`SATURATION_THRESHOLD`], so an over-unity
    /// value simply reads as saturated, which is correct.
    Fraction(f64),
    /// The reader could not produce a reading (no baseline sample yet, a
    /// parse/FFI error, or an unsupported platform). Treated as NOT saturated
    /// (fail-open) - see the crate-level docs.
    Unknown,
}

impl DiskBusy {
    /// `true` iff the disk is saturated (DESIGN s18.2). [`DiskBusy::Unknown`] is
    /// NOT saturated (fail-open) - a broken reader must never pin the pool small.
    #[must_use]
    pub fn is_saturated(self) -> bool {
        match self {
            DiskBusy::Fraction(f) => f > SATURATION_THRESHOLD,
            DiskBusy::Unknown => false,
        }
    }
}

/// Compute a busy fraction from a raw "busy time" delta and the wall-clock
/// interval it accrued over, both in the SAME time unit (ms vs ms, ns vs ns).
///
/// Pure and unit-tested directly. A zero (or absent) interval yields
/// [`DiskBusy::Unknown`] rather than dividing by zero - the caller has no
/// baseline to diff against yet.
#[must_use]
pub fn busy_fraction_from_delta(busy_delta: u64, interval: u64) -> DiskBusy {
    if interval == 0 {
        return DiskBusy::Unknown;
    }
    DiskBusy::Fraction(busy_delta as f64 / interval as f64)
}

/// A periodically-sampled disk-busy source (DESIGN s18.2).
///
/// [`sample`](DiskBusyProbe::sample) is stateful: a delta-based backend
/// (Linux, macOS) stores the previous raw counters + timestamp internally and
/// returns the busy fraction accrued since the last call, so the FIRST call
/// after construction returns [`DiskBusy::Unknown`] (no baseline). It is called
/// on the controller's sampling cadence (DESIGN s18.2: every 5 s).
pub trait DiskBusyProbe: Send + Sync {
    /// Sample the disk-busy fraction accrued since the previous call. Cheap
    /// (a small file read on Linux, one PDH collect on Windows) and never
    /// blocks on I/O for a meaningful duration. Any failure returns
    /// [`DiskBusy::Unknown`] (fail-open).
    fn sample(&self) -> DiskBusy;
}

// Exactly one per-OS backend is compiled per target; each exports a
// `RealDiskBusyProbe` re-exported cfg-free below (mirrors driven-power).
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::RealDiskBusyProbe;
#[cfg(target_os = "macos")]
pub use macos::RealDiskBusyProbe;
#[cfg(target_os = "windows")]
pub use windows::RealDiskBusyProbe;

/// Fallback [`RealDiskBusyProbe`] for any target without a per-OS backend
/// (e.g. the BSDs). Always reports [`DiskBusy::Unknown`] so the adaptive
/// controller runs with the disk gate open (fail-open) rather than failing to
/// build. The three tier-1 desktop targets (Linux/macOS/Windows) all have a
/// real backend above.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod unsupported {
    use super::{DiskBusy, DiskBusyProbe};

    /// See the module docs: an always-`Unknown` probe for unsupported targets.
    #[derive(Debug, Default)]
    pub struct RealDiskBusyProbe;

    impl RealDiskBusyProbe {
        /// Construct the no-op probe. The `root` is accepted for signature
        /// parity with the real backends and ignored.
        #[must_use]
        pub fn new(_root: std::path::PathBuf) -> Self {
            Self
        }
    }

    impl DiskBusyProbe for RealDiskBusyProbe {
        fn sample(&self) -> DiskBusy {
            DiskBusy::Unknown
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub use unsupported::RealDiskBusyProbe;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_not_saturated_fail_open() {
        // The load-bearing invariant: an unreadable disk never pins the pool.
        assert!(!DiskBusy::Unknown.is_saturated());
    }

    #[test]
    fn threshold_is_strict_greater_than_80_percent() {
        assert!(!DiskBusy::Fraction(0.0).is_saturated());
        assert!(
            !DiskBusy::Fraction(0.80).is_saturated(),
            "exactly 80% is NOT saturated (strict >)"
        );
        assert!(DiskBusy::Fraction(0.8001).is_saturated());
        assert!(DiskBusy::Fraction(1.0).is_saturated());
        // Over-unity (overlapping I/O) reads as saturated, which is correct.
        assert!(DiskBusy::Fraction(3.5).is_saturated());
    }

    #[test]
    fn busy_fraction_zero_interval_is_unknown() {
        assert_eq!(busy_fraction_from_delta(100, 0), DiskBusy::Unknown);
    }

    #[test]
    fn busy_fraction_ratio() {
        assert_eq!(busy_fraction_from_delta(0, 5_000), DiskBusy::Fraction(0.0));
        assert_eq!(
            busy_fraction_from_delta(2_500, 5_000),
            DiskBusy::Fraction(0.5)
        );
        assert_eq!(
            busy_fraction_from_delta(5_000, 5_000),
            DiskBusy::Fraction(1.0)
        );
        // Delta exceeding the interval (overlapping I/O) -> over-unity fraction.
        assert_eq!(
            busy_fraction_from_delta(9_000, 5_000),
            DiskBusy::Fraction(1.8)
        );
    }
}
