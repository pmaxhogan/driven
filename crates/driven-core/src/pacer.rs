//! The AIMD per-account rate pacer (SPEC s9, DESIGN s5.4, s18.1).
//!
//! The pacer is the canonical *rate* limit (per-second budget); it is
//! independent of the inter-file concurrency cap (the `UploadPool`,
//! DESIGN s11.4.2), which is the *in-flight* limit. Both gates must be
//! open for a request to issue (SPEC s9).
//!
//! Three token buckets refill on a wall-clock schedule (SPEC s9):
//! - a transaction bucket (50 qps initial, AIMD-adjusted),
//! - a file-create bucket (10/s initial, AIMD-adjusted),
//! - an optional bytes bucket for `settings.bandwidth_cap_mbps`
//!   (refill = `bandwidth_cap_mbps * 1_000_000 / 8` bytes/s, burst 2x;
//!   `None` = unlimited / bypassed).
//!
//! AIMD (DESIGN s18.1): halve the per-second budget on any 429 /
//! `403 rateLimitExceeded` / `403 userRateLimitExceeded`; every 10 minutes
//! of a zero-throttle window, `+5` qps and `+1/s` file-create, capped at
//! the configurable hard cap (default 200 qps, 50 files/s). Any throttle
//! resets the 10-minute window. `403 dailyLimitExceeded` pauses the
//! account until midnight Pacific and re-initialises the buckets to the
//! optimistic start.
//!
//! This module is the I/O-free contract the M3 pacer implementer fills.
//! The bucket type and the wall-clock refill read the injected
//! [`Clock`](crate::time::Clock) so tests drive AIMD deterministically.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How the pacer classifies a Drive response so AIMD can react (SPEC s9
/// `note_response(ResponseClass)`).
///
/// This is the pacer's own minimal view. It overlaps in intent with
/// [`DriveErrorClassification`](../../driven_drive/remote_store/enum.DriveErrorClassification.html)
/// (which the Drive layer produces) but is deliberately narrower: the
/// pacer only needs "did this response throttle me, and for how long".
/// The M3 executor maps a `DriveErrorClassification` onto a
/// [`ResponseClass`] when it calls [`Pacer::note_response`]; see the M3
/// phase-1 finding on not minting a third overlapping enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "class")]
pub enum ResponseClass {
    /// The request succeeded (2xx). Feeds the zero-throttle window that
    /// drives the additive increase (DESIGN s18.1).
    Ok,
    /// The account was rate-limited (429 / `userRateLimitExceeded` /
    /// `403 rateLimitExceeded`). Triggers the multiplicative decrease and
    /// a backoff for `retry_after`.
    RateLimited {
        /// Drive's recommended retry-after delay before the next request.
        #[serde(with = "duration_millis")]
        retry_after: Duration,
    },
    /// `403 dailyLimitExceeded`: the daily Drive quota is exhausted. The
    /// account pauses until the quota window resets (midnight Pacific) and
    /// the buckets re-initialise to the optimistic start (DESIGN s18.1).
    DailyQuota,
    /// A non-throttle error (5xx, network, other). Does not move the AIMD
    /// ceiling; the executor handles retry/backoff for these elsewhere.
    OtherError,
}

/// Alias for [`ResponseClass`] under the longer name the M3 task surface
/// also refers to. Identical type; provided only so call sites may use
/// whichever name reads better. [`Pacer::note_response`] takes
/// [`ResponseClass`].
pub type ResponseClassification = ResponseClass;

/// The AIMD ceilings the pacer raises toward and the hard cap it never
/// exceeds (SPEC s9 `ceilings: RwLock<PacerCeilings>`, DESIGN s18.1).
///
/// `qps` / `file_creates_per_sec` are the *current* per-second budgets
/// (the live bucket refill rates, AIMD-adjusted). `max_*` are the
/// user-configurable hard caps the additive increase is clamped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacerCeilings {
    /// Current transaction budget, transactions/sec. Starts at 50.
    pub qps: u32,
    /// Current file-create budget, creates/sec. Starts at 10.
    pub file_creates_per_sec: u32,
    /// Hard cap on `qps` (default 200, user-configurable). Additive
    /// increase never raises `qps` above this.
    pub max_qps: u32,
    /// Hard cap on `file_creates_per_sec` (default 50, user-configurable).
    pub max_file_creates_per_sec: u32,
}

impl Default for PacerCeilings {
    /// The optimistic starting values + default hard caps (SPEC s9,
    /// DESIGN s18.1): 50/10 current, 200/50 caps.
    fn default() -> Self {
        Self {
            qps: 50,
            file_creates_per_sec: 10,
            max_qps: 200,
            max_file_creates_per_sec: 50,
        }
    }
}

/// The per-account rate pacer (SPEC s9).
///
/// Declared as a trait (the SPEC s9 `Pacer` struct shape is the
/// implementer's; the *contract* the executor codes against is these
/// methods). Acquiring a permit may sleep until the relevant bucket has a
/// token (or until a backoff window lifts), so the permit methods are
/// async; the response/feedback methods are sync.
#[async_trait::async_trait]
pub trait Pacer: Send + Sync {
    /// Acquires one transaction-bucket token, first sleeping out any
    /// active backoff window (SPEC s9 `permit_request`). Every Drive API
    /// call goes through this gate.
    async fn permit_request(&self);

    /// Acquires a transaction token *and* a file-create token (SPEC s9
    /// `permit_file_create`). Used specifically for `create` calls, which
    /// Drive meters more strictly than reads/updates.
    async fn permit_file_create(&self);

    /// Acquires `n` bytes from the optional bandwidth bucket, called in
    /// the reader loop before reading each chunk from disk so the
    /// bandwidth cap back-pressures the whole pipeline (SPEC s9
    /// bandwidth-cap enforcement). A no-op when
    /// `settings.bandwidth_cap_mbps` is unset.
    async fn permit_bytes(&self, n: u64);

    /// Feeds a response classification back to the AIMD controller (SPEC
    /// s9 `note_response`): a throttle halves the budget and sets a
    /// backoff; a clean window advances the additive-increase timer.
    fn note_response(&self, classification: ResponseClass);

    /// Returns the current ceilings snapshot (for the diagnostics bundle
    /// and the "current rate" status read-out).
    fn ceilings(&self) -> PacerCeilings;
}

/// `serde` helper: (de)serialise a [`Duration`] as integer milliseconds so
/// [`ResponseClass`] crosses the IPC / activity-log JSON boundary as a
/// plain number rather than a `{secs, nanos}` struct.
mod duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_millis() as u64)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let ms = u64::deserialize(d)?;
        Ok(Duration::from_millis(ms))
    }
}
