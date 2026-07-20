//! In-memory latency reservoirs for the anonymous telemetry ping (DESIGN s13,
//! SPEC s16).
//!
//! DESIGN s13 lists "latency histograms (p50, p95) for scan and
//! upload-per-file" in the telemetry payload, but V1 shipped the wire keys
//! (`latency_p50_p95_ms.{scan,upload_per_mb}`) as ALWAYS-EMPTY arrays because
//! nothing captured per-op durations. This module is that capture: a small,
//! allocation-cheap, in-memory sampler the hot paths (the scanner's per-file
//! loop, the executor's per-upload op) feed at op completion, and the telemetry
//! ping reads at report-build time.
//!
//! SHAPE (mirrors the [`crate::executor::MemGauge`] instrumentation seam): a
//! single [`LatencyReservoir`] is created once per app, shared as an `Arc` into
//! every account's executor + orchestrator, and held on the app's telemetry
//! runtime. It is NEVER persisted - a restart starts empty (latency is a
//! best-effort signal, not durable state).
//!
//! PRIVACY / CONSENT (load-bearing, SPEC s16): capture is gated on the
//! telemetry-enabled pref. When telemetry is OFF, every `record_*` call is a
//! cheap no-op AND [`LatencyReservoir::set_enabled(false)`] drops any samples
//! already captured, so opting out cannot leave latency data lingering. A
//! latency sample is a bare millisecond duration - it carries no path, name, or
//! content, so it is privacy-safe by construction.
//!
//! WINDOWING: the reservoir is a bounded ring buffer per metric (most-recent
//! [`RESERVOIR_CAP`] samples), and [`LatencyReservoir::reset`] clears it. The
//! ping path takes a read-only [`LatencyReservoir::snapshot`] when it BUILDS the
//! payload, and resets ONLY after a SUCCESSFUL send - so a dropped/aborted ping
//! re-uses the same window's samples on the next attempt (matching how the
//! event-count aggregates re-send an un-checkpointed window).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Max samples retained per metric. A few thousand is plenty for a stable p50/
/// p95 over a 24h window while staying tiny in memory (`u64` * cap = ~32 KiB per
/// metric) and allocation-free on the hot path once warmed (the ring overwrites
/// in place). Most-recent-wins: once full, new samples overwrite the oldest.
const RESERVOIR_CAP: usize = 4096;

/// Bytes in one mebibyte - the normalizer for the upload-per-MB metric. Binary
/// MiB matches the byte-oriented accounting used elsewhere in the payload
/// (`bytes_uploaded` is raw bytes).
pub const BYTES_PER_MB: u64 = 1 << 20;

/// A single metric's bounded ring buffer of millisecond samples.
#[derive(Debug, Default)]
struct Reservoir {
    /// The retained samples (at most [`RESERVOIR_CAP`]).
    samples: Vec<u64>,
    /// Next overwrite index once at capacity (ring cursor).
    next: usize,
}

impl Reservoir {
    /// Record one sample, overwriting the oldest once at capacity.
    fn record(&mut self, v: u64) {
        if self.samples.len() < RESERVOIR_CAP {
            self.samples.push(v);
        } else {
            self.samples[self.next] = v;
            self.next += 1;
            if self.next >= RESERVOIR_CAP {
                self.next = 0;
            }
        }
    }

    /// Drop every sample (window reset).
    fn clear(&mut self) {
        self.samples.clear();
        self.next = 0;
    }

    /// `[p50, p95]` (nearest-rank) over the current samples, or an EMPTY vec
    /// when there are none - so the wire keeps emitting empty arrays until real
    /// data exists (the pre-existing V1 behaviour + what the Worker tolerates).
    fn percentiles(&self) -> Vec<u64> {
        percentiles_p50_p95(&self.samples)
    }
}

/// Compute `[p50, p95]` by the nearest-rank method, or an empty vec when
/// `samples` is empty. Split out (pure, over a slice) so the percentile math is
/// unit-tested directly against the edge cases (empty / single / even / odd).
#[must_use]
fn percentiles_p50_p95(samples: &[u64]) -> Vec<u64> {
    if samples.is_empty() {
        return Vec::new();
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    vec![nearest_rank(&sorted, 50), nearest_rank(&sorted, 95)]
}

/// Nearest-rank percentile of a NON-EMPTY sorted slice: `rank = ceil(p/100 * n)`
/// (1-indexed), clamped to `[1, n]`, returning `sorted[rank - 1]`. For `n == 1`
/// every percentile is the single sample; for the max percentile it lands on the
/// last element.
#[must_use]
fn nearest_rank(sorted: &[u64], p: u32) -> u64 {
    debug_assert!(
        !sorted.is_empty(),
        "nearest_rank requires a non-empty slice"
    );
    let n = sorted.len() as u64;
    // ceil(p * n / 100) via integer arithmetic.
    let rank = (u64::from(p) * n).div_ceil(100);
    let idx = rank.clamp(1, n) as usize - 1;
    sorted[idx]
}

/// The `[p50, p95]` pairs for both latency metrics, as read at ping-build time.
/// Each vec is either empty (no samples this window) or exactly `[p50, p95]`.
/// Mirrors the wire shape of the telemetry payload's `latency_p50_p95_ms`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LatencyPercentiles {
    /// `[p50, p95]` per-file scan-processing latency in ms (empty when none).
    pub scan: Vec<u64>,
    /// `[p50, p95]` upload latency normalized per MiB in ms (empty when none).
    pub upload_per_mb: Vec<u64>,
}

/// App-global latency sampler shared (as an `Arc`) into every account's executor
/// and orchestrator and held on the telemetry runtime. Cheap to `record_*` into
/// from the hot paths; snapshotted + reset by the telemetry ping.
#[derive(Debug)]
pub struct LatencyReservoir {
    /// Consent gate (mirrors the telemetry-enabled pref). When false, `record_*`
    /// is a no-op and the buffers are kept empty.
    enabled: AtomicBool,
    /// Per-file scan-processing durations (ms).
    scan: Mutex<Reservoir>,
    /// Per-upload latency normalized per MiB (ms).
    upload_per_mb: Mutex<Reservoir>,
}

impl Default for LatencyReservoir {
    /// A default-ON reservoir (telemetry is DEFAULT ON, SPEC s16). Boot replaces
    /// this with one initialized from the persisted `telemetry.enabled` pref
    /// before any capture happens; the default is only the pre-install placeholder
    /// and the no-orchestrator (quiesced) app state.
    fn default() -> Self {
        Self::new(true)
    }
}

impl LatencyReservoir {
    /// Create a reservoir with the initial consent state (set from the persisted
    /// `telemetry.enabled` pref at boot, BEFORE any executor/scanner can capture,
    /// so a user who opted out gets no startup capture window).
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            scan: Mutex::new(Reservoir::default()),
            upload_per_mb: Mutex::new(Reservoir::default()),
        }
    }

    /// Whether capture is currently enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Flip the consent gate. Turning it OFF also DROPS any samples already
    /// captured (SPEC s16: opting out must not leave latency data lingering);
    /// turning it back ON starts from an empty window.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        if !enabled {
            self.lock_scan().clear();
            self.lock_upload().clear();
        }
    }

    /// Record one per-file scan-processing duration (ms). No-op when disabled.
    pub fn record_scan_ms(&self, ms: u64) {
        if !self.is_enabled() {
            return;
        }
        self.lock_scan().record(ms);
    }

    /// Record one upload's latency normalized per MiB (ms). No-op when disabled.
    /// The caller computes the per-MiB figure from the op's wall-clock duration
    /// and its byte size (see [`per_mb_ms`]).
    pub fn record_upload_per_mb_ms(&self, ms: u64) {
        if !self.is_enabled() {
            return;
        }
        self.lock_upload().record(ms);
    }

    /// Read-only `[p50, p95]` for each metric (empty when no samples). Does NOT
    /// reset - the caller resets via [`Self::reset`] only after a SUCCESSFUL
    /// send, so a dropped ping re-uses the same window.
    #[must_use]
    pub fn snapshot(&self) -> LatencyPercentiles {
        LatencyPercentiles {
            scan: self.lock_scan().percentiles(),
            upload_per_mb: self.lock_upload().percentiles(),
        }
    }

    /// Clear both reservoirs (called after a successful telemetry send so the
    /// next reporting window starts fresh).
    pub fn reset(&self) {
        self.lock_scan().clear();
        self.lock_upload().clear();
    }

    fn lock_scan(&self) -> std::sync::MutexGuard<'_, Reservoir> {
        self.scan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_upload(&self) -> std::sync::MutexGuard<'_, Reservoir> {
        self.upload_per_mb
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Normalize an upload op's wall-clock duration to milliseconds-per-MiB.
/// Returns `None` when `bytes == 0` (nothing to normalize against - a zero-byte
/// object's per-MB latency is meaningless). Uses `u128` intermediate arithmetic
/// so a large `elapsed_ms * BYTES_PER_MB` cannot overflow.
#[must_use]
pub fn per_mb_ms(elapsed_ms: u64, bytes: u64) -> Option<u64> {
    if bytes == 0 {
        return None;
    }
    let per_mb = u128::from(elapsed_ms) * u128::from(BYTES_PER_MB) / u128::from(bytes);
    Some(per_mb.min(u128::from(u64::MAX)) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_empty_is_empty() {
        // No samples -> empty vec (the wire keeps emitting empty arrays).
        assert!(percentiles_p50_p95(&[]).is_empty());
    }

    #[test]
    fn percentiles_single_sample_repeats() {
        // One sample: p50 == p95 == that sample.
        assert_eq!(percentiles_p50_p95(&[42]), vec![42, 42]);
    }

    #[test]
    fn percentiles_odd_count() {
        // n = 5 sorted [1,2,3,4,5]: p50 rank ceil(2.5)=3 -> idx2 -> 3;
        // p95 rank ceil(4.75)=5 -> idx4 -> 5.
        assert_eq!(percentiles_p50_p95(&[5, 3, 1, 4, 2]), vec![3, 5]);
    }

    #[test]
    fn percentiles_even_count() {
        // n = 4 sorted [10,20,30,40]: p50 rank ceil(2.0)=2 -> idx1 -> 20;
        // p95 rank ceil(3.8)=4 -> idx3 -> 40.
        assert_eq!(percentiles_p50_p95(&[40, 10, 30, 20]), vec![20, 40]);
    }

    #[test]
    fn percentiles_large_uniform() {
        // 1..=100: p50 -> 50, p95 -> 95 (nearest-rank on a dense range).
        let samples: Vec<u64> = (1..=100).collect();
        assert_eq!(percentiles_p50_p95(&samples), vec![50, 95]);
    }

    #[test]
    fn record_no_op_when_disabled() {
        // SPEC s16: no capture when telemetry is off.
        let r = LatencyReservoir::new(false);
        r.record_scan_ms(5);
        r.record_upload_per_mb_ms(7);
        let snap = r.snapshot();
        assert!(snap.scan.is_empty());
        assert!(snap.upload_per_mb.is_empty());
    }

    #[test]
    fn record_and_snapshot_when_enabled() {
        let r = LatencyReservoir::new(true);
        for v in [10u64, 20, 30] {
            r.record_scan_ms(v);
        }
        r.record_upload_per_mb_ms(100);
        let snap = r.snapshot();
        // [10,20,30]: p50 rank ceil(1.5)=2 -> idx1 -> 20; p95 rank ceil(2.85)=3 -> idx2 -> 30.
        assert_eq!(snap.scan, vec![20, 30]);
        assert_eq!(snap.upload_per_mb, vec![100, 100]);
        // A read-only snapshot does NOT drain: a second snapshot is identical.
        assert_eq!(r.snapshot().scan, vec![20, 30]);
    }

    #[test]
    fn reset_clears_the_window() {
        let r = LatencyReservoir::new(true);
        r.record_scan_ms(1);
        r.record_upload_per_mb_ms(2);
        assert!(!r.snapshot().scan.is_empty());
        r.reset();
        let snap = r.snapshot();
        assert!(snap.scan.is_empty());
        assert!(snap.upload_per_mb.is_empty());
    }

    #[test]
    fn disable_drops_captured_samples() {
        // Turning capture off must drop anything already captured (consent).
        let r = LatencyReservoir::new(true);
        r.record_scan_ms(9);
        assert!(!r.snapshot().scan.is_empty());
        r.set_enabled(false);
        assert!(r.snapshot().scan.is_empty());
        // Re-enabling starts from an empty window.
        r.set_enabled(true);
        assert!(r.snapshot().scan.is_empty());
        r.record_scan_ms(3);
        assert_eq!(r.snapshot().scan, vec![3, 3]);
    }

    #[test]
    fn ring_buffer_caps_at_capacity() {
        // More than CAP samples: only the most-recent CAP are retained. Push
        // CAP zeros then CAP hundreds; the window should be all hundreds.
        let r = LatencyReservoir::new(true);
        for _ in 0..RESERVOIR_CAP {
            r.record_scan_ms(0);
        }
        for _ in 0..RESERVOIR_CAP {
            r.record_scan_ms(100);
        }
        assert_eq!(r.snapshot().scan, vec![100, 100]);
    }

    #[test]
    fn per_mb_ms_normalizes_and_guards_zero() {
        // 1 MiB in 200 ms -> 200 ms/MiB.
        assert_eq!(per_mb_ms(200, BYTES_PER_MB), Some(200));
        // 2 MiB in 200 ms -> 100 ms/MiB.
        assert_eq!(per_mb_ms(200, 2 * BYTES_PER_MB), Some(100));
        // Half a MiB in 50 ms -> 100 ms/MiB.
        assert_eq!(per_mb_ms(50, BYTES_PER_MB / 2), Some(100));
        // Zero bytes -> no sample (nothing to normalize against).
        assert_eq!(per_mb_ms(200, 0), None);
    }
}
