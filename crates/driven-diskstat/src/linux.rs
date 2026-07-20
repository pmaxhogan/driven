//! Linux [`DiskBusyProbe`] backend (DESIGN s18.2): `/proc/diskstats` field 10.
//!
//! `/proc/diskstats` exposes, per block device, the cumulative "time spent doing
//! I/Os" in milliseconds (the 13th whitespace token, a.k.a. field 10 of the
//! post-name stats - the same counter `iostat`'s `%util` is derived from). The
//! busy FRACTION over an interval is `busy_ms_delta / interval_ms`; DESIGN s18.2
//! flags `> 0.80` as saturated.
//!
//! The device is the one BACKING THE SOURCE ROOT: we `stat(2)` the root once at
//! construction, decompose its `st_dev` into `(major, minor)`, and match that
//! against the `/proc/diskstats` rows (partitions carry their own major:minor
//! and their own busy counter). A root whose device does not appear in
//! `/proc/diskstats` (device-mapper, overlay, a network mount) yields
//! [`DiskBusy::Unknown`] - fail-open, never a false "saturated".

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use crate::{DiskBusy, DiskBusyProbe};

/// A prior sample: the cumulative busy-ms counter and when it was read, so the
/// next sample can diff both against it.
#[derive(Clone, Copy)]
struct Baseline {
    busy_ms: u64,
    at: Instant,
}

/// Linux disk-busy reader over `/proc/diskstats` (DESIGN s18.2).
pub struct RealDiskBusyProbe {
    /// The `(major, minor)` of the device backing the source root, resolved once
    /// via `stat(2)`. `None` when the root could not be `stat`ed (then every
    /// sample is `Unknown`, fail-open).
    device: Option<(u64, u64)>,
    /// The previous `(busy_ms, Instant)`; `None` until the first sample sets the
    /// baseline.
    baseline: Mutex<Option<Baseline>>,
}

impl RealDiskBusyProbe {
    /// Build a reader for the device backing `root`. Resolution never fails hard:
    /// an un-`stat`able root just makes every [`sample`](DiskBusyProbe::sample)
    /// return [`DiskBusy::Unknown`].
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        let device = std::fs::metadata(&root)
            .ok()
            .map(|m| m.dev())
            .map(|dev| (gnu_dev_major(dev), gnu_dev_minor(dev)));
        Self {
            device,
            baseline: Mutex::new(None),
        }
    }
}

impl DiskBusyProbe for RealDiskBusyProbe {
    fn sample(&self) -> DiskBusy {
        let Some((major, minor)) = self.device else {
            return DiskBusy::Unknown;
        };
        let Ok(contents) = std::fs::read_to_string("/proc/diskstats") else {
            return DiskBusy::Unknown;
        };
        let Some(busy_ms) = parse_busy_ms(&contents, major, minor) else {
            return DiskBusy::Unknown;
        };

        let now = Instant::now();
        let mut guard = self.baseline.lock().unwrap_or_else(|e| e.into_inner());
        let prev = *guard;
        *guard = Some(Baseline { busy_ms, at: now });
        drop(guard);

        match prev {
            // First sample: establish the baseline, no fraction yet.
            None => DiskBusy::Unknown,
            Some(prev) => {
                let interval_ms = now.duration_since(prev.at).as_millis();
                let interval_ms = u64::try_from(interval_ms).unwrap_or(u64::MAX);
                // `saturating_sub`: the counter is monotonic, but a device
                // hot-swap / counter reset must never produce a huge bogus delta.
                let delta = busy_ms.saturating_sub(prev.busy_ms);
                crate::busy_fraction_from_delta(delta, interval_ms)
            }
        }
    }
}

/// Extract the cumulative "time spent doing I/Os" (ms) for `(major, minor)` from
/// `/proc/diskstats` contents. Pure over the file text so it is unit-testable
/// without a live `/proc`. Returns `None` when no row matches.
///
/// Row layout (`Documentation/admin-guide/iostats.rst`): `major minor name`
/// followed by the numeric fields; "time spent doing I/Os (ms)" is the 13th
/// whitespace token (index 12).
#[must_use]
fn parse_busy_ms(contents: &str, major: u64, minor: u64) -> Option<u64> {
    for line in contents.lines() {
        let mut it = line.split_whitespace();
        let row_major: u64 = it.next()?.parse().ok()?;
        let row_minor: u64 = it.next()?.parse().ok()?;
        if row_major != major || row_minor != minor {
            continue;
        }
        // Skip the device name, then advance to token index 12 overall. We have
        // already consumed indices 0 (major) and 1 (minor); the name is index 2,
        // so the 10 tokens after the name land us on index 12 (busy-ms).
        let mut it = it.skip(1); // device name (index 2)
                                 // indices 3..=11 (nine fields) then index 12 is next.
        for _ in 0..9 {
            it.next()?;
        }
        return it.next()?.parse().ok();
    }
    None
}

/// `major(3)` per the glibc `gnu_dev_major` encoding of a Linux `dev_t`.
#[must_use]
fn gnu_dev_major(dev: u64) -> u64 {
    ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)
}

/// `minor(3)` per the glibc `gnu_dev_minor` encoding of a Linux `dev_t`.
#[must_use]
fn gnu_dev_minor(dev: u64) -> u64 {
    (dev & 0xff) | ((dev >> 12) & !0xff)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A representative /proc/diskstats excerpt (sda + its partition sda1, plus
    // an unrelated nvme device). "Time spent doing I/Os (ms)" is field 10 of the
    // post-name stats == token index 12; each device carries a distinctive value
    // there (sda = 1500, sda1 = 123456, nvme0n1 = 90) so a miscount is obvious.
    const DISKSTATS: &str = "\
   8       0 sda 1000 20 40000 800 500 10 20000 400 0 1500 1200 0 0 0 0
   8       1 sda1 900 10 39000 700 400 5 19000 300 0 123456 1400 0 0 0 0
 259       0 nvme0n1 50 0 2000 30 60 0 3000 40 0 90 250 0 0 0 0
";

    #[test]
    fn parses_busy_ms_for_matching_device() {
        assert_eq!(parse_busy_ms(DISKSTATS, 8, 1), Some(123_456));
        assert_eq!(parse_busy_ms(DISKSTATS, 8, 0), Some(1_500));
        assert_eq!(parse_busy_ms(DISKSTATS, 259, 0), Some(90));
    }

    #[test]
    fn unmatched_device_is_none() {
        assert_eq!(parse_busy_ms(DISKSTATS, 253, 0), None);
    }

    #[test]
    fn malformed_row_is_skipped_not_panicked() {
        // A truncated row must not panic; a later well-formed row still matches.
        let text = "8 0 sda 1 2 3\n8 1 sda1 900 10 39000 700 400 5 19000 300 0 777 1400 0\n";
        assert_eq!(parse_busy_ms(text, 8, 0), None);
        assert_eq!(parse_busy_ms(text, 8, 1), Some(777));
    }

    #[test]
    fn dev_decompose_roundtrip() {
        // 8:1 encodes as the classic non-huge dev_t 0x0801.
        let dev: u64 = (8 << 8) | 1;
        assert_eq!(gnu_dev_major(dev), 8);
        assert_eq!(gnu_dev_minor(dev), 1);
    }
}
