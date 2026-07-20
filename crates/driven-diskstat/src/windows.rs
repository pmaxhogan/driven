//! Windows [`DiskBusyProbe`] backend (DESIGN s18.2): PDH `\PhysicalDisk(_Total)
//! \% Disk Time`.
//!
//! `% Disk Time` is the fraction of the interval the disk spent servicing
//! requests - exactly the "busy fraction" DESIGN s18.2 wants. PDH computes the
//! rate itself over the interval between two `PdhCollectQueryData` calls, so this
//! reader holds one query + counter handle for its lifetime, primes the query
//! with a collect at construction, and each [`sample`](DiskBusyProbe::sample)
//! does one more collect + a formatted read. The value is a PERCENT (0..~100+,
//! and it CAN exceed 100 across a busy multi-spindle `_Total`); we divide by 100
//! and, consistent with the Linux backend, let an over-unity fraction read as
//! saturated rather than clamping.
//!
//! `PdhAddEnglishCounterW` (not the localized `PdhAddCounterW`) makes the counter
//! path locale-independent, so it resolves on a non-English Windows install.
//!
//! Fail-open: ANY non-zero PDH status - open/add/collect/format failure - makes
//! `sample` return [`DiskBusy::Unknown`], and a failed construction leaves the
//! handles `None` so every later sample is `Unknown` too.

use std::path::PathBuf;
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterValue,
    PdhOpenQueryW, PDH_FMT, PDH_FMT_COUNTERVALUE, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
};

use crate::{DiskBusy, DiskBusyProbe};

/// `ERROR_SUCCESS` for the PDH status codes (a `u32` returned by every PDH call).
const PDH_SUCCESS: u32 = 0;

/// `PDH_FMT_NOCAP100` (winperf.h `0x00008000`): report a genuinely-over-100%
/// `_Total` honestly instead of clamping to 100. Not re-exported as a named
/// constant by the `windows` crate, so defined here.
const PDH_FMT_NOCAP100: u32 = 0x0000_8000;

/// The English (locale-independent) counter path for aggregate disk-busy.
const COUNTER_PATH: &str = r"\PhysicalDisk(_Total)\% Disk Time";

/// Owns the PDH query + counter handles and closes the query on drop.
struct PdhQuery {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
}

// PDH handles are process-global kernel-ish handles; the query is only ever
// touched behind the outer `Mutex`, so it is safe to move across threads.
unsafe impl Send for PdhQuery {}

impl Drop for PdhQuery {
    fn drop(&mut self) {
        // Best-effort close; nothing actionable on failure at teardown.
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}

/// Windows disk-busy reader over PDH (DESIGN s18.2).
pub struct RealDiskBusyProbe {
    /// `None` when the PDH query could not be opened / the counter added; then
    /// every sample is [`DiskBusy::Unknown`] (fail-open).
    query: Mutex<Option<PdhQuery>>,
}

impl RealDiskBusyProbe {
    /// Open a PDH query for `\PhysicalDisk(_Total)\% Disk Time` and prime it with
    /// one collect so the first [`sample`](DiskBusyProbe::sample) already has a
    /// baseline interval. The `root` is unused on Windows (the `_Total` instance
    /// already spans every physical disk) but accepted for signature parity with
    /// the other backends. Never fails hard - a PDH error leaves the handle
    /// `None`.
    #[must_use]
    pub fn new(_root: PathBuf) -> Self {
        Self {
            query: Mutex::new(open_query()),
        }
    }
}

impl DiskBusyProbe for RealDiskBusyProbe {
    fn sample(&self) -> DiskBusy {
        let guard = self.query.lock().unwrap_or_else(|e| e.into_inner());
        let Some(q) = guard.as_ref() else {
            return DiskBusy::Unknown;
        };
        unsafe {
            if PdhCollectQueryData(q.query) != PDH_SUCCESS {
                return DiskBusy::Unknown;
            }
            let mut value = PDH_FMT_COUNTERVALUE::default();
            // NOCAP100 so a genuinely-over-100% `_Total` is reported honestly
            // (it reads as saturated) rather than being clamped to 100.
            let fmt = PDH_FMT(PDH_FMT_DOUBLE.0 | PDH_FMT_NOCAP100);
            let status = PdhGetFormattedCounterValue(q.counter, fmt, None, &mut value);
            if status != PDH_SUCCESS {
                return DiskBusy::Unknown;
            }
            // SAFETY: we requested PDH_FMT_DOUBLE, so the union's doubleValue is
            // the initialized member.
            let percent = value.Anonymous.doubleValue;
            if !percent.is_finite() || percent < 0.0 {
                return DiskBusy::Unknown;
            }
            DiskBusy::Fraction(percent / 100.0)
        }
    }
}

/// Open the PDH query, add the English counter, and prime it with one collect.
/// Returns `None` on any PDH failure (fail-open).
fn open_query() -> Option<PdhQuery> {
    unsafe {
        let mut query = PDH_HQUERY::default();
        if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != PDH_SUCCESS {
            return None;
        }
        let path: Vec<u16> = COUNTER_PATH
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut counter = PDH_HCOUNTER::default();
        let added = PdhAddEnglishCounterW(query, PCWSTR(path.as_ptr()), 0, &mut counter);
        if added != PDH_SUCCESS {
            let _ = PdhCloseQuery(query);
            return None;
        }
        // Prime: a rate counter needs a first collect to establish the baseline
        // interval; the next collect (first `sample`) then yields a real rate.
        let _ = PdhCollectQueryData(query);
        Some(PdhQuery { query, counter })
    }
}
