//! Pathological-filename scenarios (STRESS_HARNESS s3.4).
//!
//! Control chars, RLO/ZWJ/ZWNJ/BOM, IDN homographs, NFC-vs-NFD, Hangul
//! jamo, 255-byte leaves, 4 KiB paths, Windows-reserved names, trailing
//! space/dot, separator look-alikes, unpaired surrogates, case-only and
//! normalisation-only differences. The unpaired-surrogate row expects the
//! new `local.invalid_filename` code (SPEC s24 / STRESS_HARNESS s10).
//!
//! Phase-2 fills these and returns them from [`scenarios`].

use crate::scenario::Scenario;

/// Every pathological-filename scenario (STRESS_HARNESS s3.4). Empty until Phase-2.
pub fn scenarios() -> Vec<Box<dyn Scenario>> {
    Vec::new()
}
