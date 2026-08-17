//! Cumulative app-wide I/O byte counters behind the Activity dashboard's live
//! disk / network throughput graphs (2026-08-14 follow-up to the OOM
//! incident: an 88 GB resume pushed 140 Mbps of upload while every existing
//! throughput surface read zero).
//!
//! Deliberately SEPARATE from [`crate::adaptive::ThroughputProbe`]: that
//! probe is drain-based (`take_bytes`) and owned by the adaptive
//! pool-sizing controller - a second reader would steal its window. These
//! counters are CUMULATIVE and peek-only, so any number of samplers can diff
//! consecutive snapshots without disturbing each other.
//!
//! What counts where:
//! - `disk_read`: plaintext bytes Driven itself reads from local files for
//!   backup work - the upload pipeline's reader stage, the resume re-read,
//!   the reconcile re-hash paths, and the buffered small-file band. This is
//!   app-attributed I/O, not OS device throughput (a deliberate design
//!   choice: the graph answers "what is Driven doing", not "what is the
//!   disk doing").
//! - `net_wire`: bytes accepted by the destination - post-encryption wire
//!   bytes, credited on ack (per wire chunk for resumable sessions, on
//!   completion for single-request uploads). Each byte is credited exactly
//!   once; bundle members are covered by their bundle's wire push, never
//!   double-counted at completion.
//! - `hashed`: plaintext bytes blake3-hashed (issue #308 bottleneck
//!   classifier, 2026-08-17 follow-up). Credited from the two hot hashing
//!   paths - the upload pipeline's cpu stage (streamed and buffered) and the
//!   scanner's deep-verify re-hash - so the "cpu" bottleneck state has a real
//!   rate to compare against `disk_read` and `net_wire`. Deliberately its own
//!   counter rather than folded into `disk_read`: a deep-verify re-hash of an
//!   already-synced file hashes bytes without any corresponding upload, so
//!   conflating the two would make a hash-only scan look like disk activity.
//!
//! v1 scope notes: bundle ASSEMBLY reads (tar-ing members) are approximated
//! by the bundle's wire push rather than counted at read time.

use std::sync::atomic::{AtomicU64, Ordering};

/// The cumulative counters. One instance per app, shared by every account's
/// executor; the app shell's sampler diffs [`IoCounters::snapshot`] on a
/// fixed cadence to derive bytes/sec.
#[derive(Debug, Default)]
pub struct IoCounters {
    disk_read: AtomicU64,
    net_wire: AtomicU64,
    hashed: AtomicU64,
}

/// One peek of the cumulative totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoSnapshot {
    /// Total plaintext bytes read from local files for backup work.
    pub disk_read_bytes: u64,
    /// Total wire bytes accepted by the destination.
    pub net_wire_bytes: u64,
    /// Total plaintext bytes blake3-hashed (issue #308).
    pub hashed_bytes: u64,
}

impl IoCounters {
    /// Credit `n` plaintext bytes read from a local file.
    pub fn add_disk_read(&self, n: u64) {
        self.disk_read.fetch_add(n, Ordering::Relaxed);
    }

    /// Credit `n` wire bytes the destination accepted.
    pub fn add_net_wire(&self, n: u64) {
        self.net_wire.fetch_add(n, Ordering::Relaxed);
    }

    /// Credit `n` plaintext bytes blake3-hashed (issue #308 bottleneck
    /// classifier's cpu signal). A single relaxed atomic add on the same
    /// buffer the hashing path already owns - zero measurable overhead in
    /// the hot loop.
    pub fn add_hashed(&self, n: u64) {
        self.hashed.fetch_add(n, Ordering::Relaxed);
    }

    /// Peek all totals. Never resets - samplers diff consecutive snapshots.
    pub fn snapshot(&self) -> IoSnapshot {
        IoSnapshot {
            disk_read_bytes: self.disk_read.load(Ordering::Relaxed),
            net_wire_bytes: self.net_wire.load(Ordering::Relaxed),
            hashed_bytes: self.hashed.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_snapshot_does_not_drain() {
        let c = IoCounters::default();
        assert_eq!(
            c.snapshot(),
            IoSnapshot {
                disk_read_bytes: 0,
                net_wire_bytes: 0,
                hashed_bytes: 0,
            }
        );
        c.add_disk_read(100);
        c.add_net_wire(40);
        c.add_disk_read(1);
        c.add_hashed(7);
        let s1 = c.snapshot();
        assert_eq!(s1.disk_read_bytes, 101);
        assert_eq!(s1.net_wire_bytes, 40);
        assert_eq!(s1.hashed_bytes, 7);
        // Peek-only: a second reader sees the same cumulative totals.
        assert_eq!(c.snapshot(), s1);
    }
}
