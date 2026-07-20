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

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::time::Clock;
use crate::types::UnixMs;

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

    /// Set the effective bandwidth cap at runtime, in Mbps (`None` =
    /// unlimited). Drives the V2 metered-network throttle (DESIGN s17): the
    /// orchestrator lowers the cap on a metered network and lifts it off one.
    /// The default is a no-op so simple test/fake pacers need not implement it;
    /// [`AimdPacer`] overrides it.
    fn set_bandwidth_cap(&self, _mbps: Option<f64>) {}

    /// Wall-clock ms of the most recent throttle response (rate-limit or daily
    /// quota), or `i64::MIN` if the pacer has never throttled. The adaptive
    /// upload-parallelism controller (DESIGN s11.4.7) reads this to answer "did
    /// the pacer throttle at any point during the last throughput window?" - a
    /// throttle explains a throughput drop, so a shrink must be suppressed even
    /// if the backoff has since cleared. The default `i64::MIN` (never) makes a
    /// simple/fake pacer read as "not throttling"; [`AimdPacer`] overrides it.
    fn last_throttle_ms(&self) -> i64 {
        i64::MIN
    }
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

// ---------------------------------------------------------------------------
// Concrete implementation
// ---------------------------------------------------------------------------

/// How long the additive-increase window must stay clean before a raise
/// (DESIGN s18.1: every 10 minutes of zero-throttle window).
const CLEAN_WINDOW_MS: i64 = 10 * 60 * 1_000;

/// Additive-increase step for the transaction budget (DESIGN s18.1: +5
/// qps per clean window).
const QPS_INCREASE_STEP: u32 = 5;

/// Additive-increase step for the file-create budget (DESIGN s18.1: +1/s
/// per clean window).
const FILE_INCREASE_STEP: u32 = 1;

/// Pacific-standard-time offset from UTC, in milliseconds (UTC-8).
///
/// Pacific time is UTC-8 (PST) or UTC-7 (PDT). `driven-core` is held to
/// no new dependencies for this module, so we cannot pull a `tz` database
/// (chrono-tz) to resolve the exact DST rule for the `now` instant. We
/// therefore approximate the "midnight Pacific" quota-reset boundary
/// (DESIGN s18.1) with the fixed PST offset. The consequence is bounded:
/// during PDT the computed resume instant can be off by at most one hour,
/// which is acceptable for a daily-quota pause (Drive's reset is itself
/// not exposed to the minute). A precise DST-aware boundary is a later
/// refinement if it ever matters.
const PACIFIC_OFFSET_MS: i64 = 8 * 60 * 60 * 1_000;

/// Milliseconds in one day.
const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

/// How long [`TokenBucket::acquire`] sleeps between refill polls when a
/// bucket is empty.
///
/// The injected [`Clock`] (a `FakeClock` in tests) is decoupled from the
/// tokio timer, so `acquire` cannot compute one exact sleep and trust it:
/// it must re-consult the wall clock after each yield. A short real sleep
/// lets a concurrently-running test body advance the `FakeClock` between
/// polls (deterministic refill) while keeping the production busy-wait
/// negligible. Refill granularity is therefore ~1 ms, far finer than any
/// per-second budget.
const ACQUIRE_POLL: Duration = Duration::from_millis(1);

/// Locks a [`Mutex`], recovering the guard if a previous holder panicked
/// while the lock was held.
///
/// The pacer's guarded state is plain numeric AIMD bookkeeping (token
/// counts, refill rates, ceilings); a poisoned lock cannot leave it in a
/// logically corrupt state, so recovering the guard is safe and lets us
/// honour the no-`unwrap`/no-`panic` house rule on the hot path. We
/// deliberately do not propagate the poison.
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// A wall-clock-refilled token bucket (SPEC s9: "buckets refill on a
/// wall-clock schedule, not after-N-requests timer").
///
/// Tokens are held as a fixed-point fraction (scaled by [`Self::SCALE`])
/// so a sub-token-per-millisecond refill rate (e.g. 10 file-creates/sec =
/// 0.01 tokens/ms) does not truncate to zero. The bucket reads the
/// injected clock on every operation; it never schedules its own timer.
struct TokenBucket {
    /// Shared mutable state behind a short-lived lock. The lock is held
    /// only for arithmetic, so contention is a non-issue at the per-second
    /// rates involved.
    inner: Mutex<BucketInner>,
    clock: Arc<dyn Clock>,
}

// `Clock` is the Phase-1 canonical seam and intentionally carries no `Debug`
// supertrait (that would force `Debug` onto every impl, including the real
// OS clock). We format the observable state and elide the injected clock.
impl std::fmt::Debug for TokenBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenBucket")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct BucketInner {
    /// Available tokens, scaled by [`TokenBucket::SCALE`].
    tokens_scaled: u128,
    /// Refill rate in tokens/second.
    rate_per_sec: f64,
    /// Burst ceiling in tokens (the bucket never refills above this),
    /// scaled by [`TokenBucket::SCALE`].
    capacity_scaled: u128,
    /// Wall-clock reading (ms) of the last refill, so elapsed time is
    /// measured against the injected clock.
    last_refill_ms: UnixMs,
}

impl TokenBucket {
    /// Fixed-point scale for fractional tokens (1 token = `SCALE` units).
    const SCALE: u128 = 1_000_000;

    /// Builds a bucket starting full (one burst of `capacity`) at the
    /// clock's current reading.
    fn new(rate_per_sec: f64, capacity: f64, clock: Arc<dyn Clock>) -> Self {
        let now = clock.now_ms();
        let capacity_scaled = Self::to_scaled(capacity);
        Self {
            inner: Mutex::new(BucketInner {
                tokens_scaled: capacity_scaled,
                rate_per_sec,
                capacity_scaled,
                last_refill_ms: now,
            }),
            clock,
        }
    }

    /// Converts a token count to fixed-point, clamping negatives to zero.
    fn to_scaled(tokens: f64) -> u128 {
        if tokens <= 0.0 {
            0
        } else {
            (tokens * Self::SCALE as f64) as u128
        }
    }

    /// Replays the wall-clock refill up to `now`, mutating `inner` in
    /// place. Uses `max(0, now - last)` so a backwards wall jump (DESIGN
    /// s18.7) never *removes* tokens; it just records the new reading.
    fn refill_locked(inner: &mut BucketInner, now: UnixMs) {
        let elapsed_ms = now.saturating_sub(inner.last_refill_ms);
        inner.last_refill_ms = now;
        if elapsed_ms <= 0 {
            return;
        }
        // tokens = rate_per_sec * elapsed_ms / 1000, in scaled units.
        let added = (inner.rate_per_sec * elapsed_ms as f64 / 1000.0) * Self::SCALE as f64;
        if added <= 0.0 {
            return;
        }
        let added = added as u128;
        inner.tokens_scaled = inner
            .tokens_scaled
            .saturating_add(added)
            .min(inner.capacity_scaled);
    }

    /// Tries to take `n` whole tokens without blocking. Returns `true` on
    /// success; `false` (with no tokens consumed) when the bucket is short.
    fn try_acquire(&self, n: u64) -> bool {
        let need = (n as u128).saturating_mul(Self::SCALE);
        let now = self.clock.now_ms();
        let mut inner = lock_recover(&self.inner);
        Self::refill_locked(&mut inner, now);
        if inner.tokens_scaled >= need {
            inner.tokens_scaled -= need;
            true
        } else {
            false
        }
    }

    /// Acquires `n` tokens, polling the wall clock and yielding until the
    /// refill has produced enough simultaneously-available tokens (SPEC s9
    /// blocking acquire).
    ///
    /// Acquisition is all-or-nothing per poll: the loop never partially
    /// drains the bucket, it waits until the full `n` is available in one
    /// shot. An `n` that exceeds the burst capacity could therefore never
    /// be satisfied, so the demand is clamped to the capacity below; the
    /// caller is still back-pressured (it waits a full refill) but the
    /// acquire can never wait forever. Per-chunk byte counts are bounded by
    /// the pipeline chunk size, which is sized below the byte-bucket burst,
    /// so the clamp is defensive rather than routine.
    async fn acquire(&self, n: u64) {
        let capped = {
            let inner = lock_recover(&self.inner);
            let cap_tokens = (inner.capacity_scaled / Self::SCALE).max(1) as u64;
            n.min(cap_tokens)
        };
        loop {
            if self.try_acquire(capped) {
                return;
            }
            tokio::time::sleep(ACQUIRE_POLL).await;
        }
    }

    /// Resets the bucket to a new refill rate and burst capacity, starting
    /// full at the clock's current reading. Used by AIMD halve / raise /
    /// daily re-init so the live rate tracks the ceiling.
    fn reconfigure(&self, rate_per_sec: f64, capacity: f64) {
        let now = self.clock.now_ms();
        let capacity_scaled = Self::to_scaled(capacity);
        let mut inner = lock_recover(&self.inner);
        inner.rate_per_sec = rate_per_sec;
        inner.capacity_scaled = capacity_scaled;
        inner.tokens_scaled = inner.tokens_scaled.min(capacity_scaled);
        inner.last_refill_ms = now;
    }
}

/// The concrete AIMD pacer (SPEC s9 `Pacer` struct).
///
/// One instance per account. Holds the transaction + file-create buckets,
/// the optional bandwidth bucket, the AIMD ceilings, a backoff deadline,
/// and the clean-window timer. All time decisions read the injected
/// [`Clock`] so tests drive AIMD deterministically with a `FakeClock`.
/// The optional bandwidth gate, mutable at runtime (V2 metered throttle).
///
/// `rate` is the current effective refill rate (bytes/s), `None` = unlimited.
/// `bucket` is the live [`TokenBucket`] held behind an `Arc` so
/// [`AimdPacer::permit_bytes`] can clone it out under a brief lock and run the
/// async `acquire` WITHOUT holding the gate lock across the await.
/// [`Pacer::set_bandwidth_cap`] swaps both atomically when the cap changes.
struct ByteGate {
    rate: Option<f64>,
    bucket: Option<Arc<TokenBucket>>,
}

pub struct AimdPacer {
    qps_bucket: TokenBucket,
    file_bucket: TokenBucket,
    /// The optional bandwidth gate (SPEC s9), swappable at runtime so the
    /// metered-network throttle (V2, DESIGN s17) can lower / lift the cap
    /// without rebuilding the pacer. `None` rate bypasses the byte gate.
    bytes: Mutex<ByteGate>,
    /// Wall-clock deadline (ms) before which `permit_request` sleeps
    /// (SPEC s9 `backoff_until`). `<= now` means no backoff.
    backoff_until_ms: AtomicI64,
    /// Wall-clock reading (ms) of the last throttle, i.e. the start of the
    /// current clean window (DESIGN s18.1). The additive increase fires
    /// once `CLEAN_WINDOW_MS` of clean time has accrued since this point.
    clean_window_start_ms: AtomicI64,
    /// Wall-clock ms of the most recent THROTTLE response (rate-limit or daily
    /// quota), or `i64::MIN` if the pacer has never throttled. Distinct from
    /// `clean_window_start_ms` (which is ALSO seeded to `now` at construction, so
    /// it cannot answer "has a throttle happened since time T"): this is set only
    /// by an actual throttle. Read by the adaptive-parallelism controller
    /// (DESIGN s11.4.7) via [`Pacer::last_throttle_ms`] to scope its "not
    /// throttling" gate to the throughput window.
    last_throttle_ms: AtomicI64,
    /// Bit-packed current ceilings, guarded for atomic snapshot/update.
    ceilings: Mutex<PacerCeilings>,
    clock: Arc<dyn Clock>,
}

// `Clock` is the Phase-1 canonical seam without a `Debug` supertrait; we
// format the pacer's observable state and elide the injected clock (see the
// `TokenBucket` Debug impl above).
impl std::fmt::Debug for AimdPacer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes_rate = lock_recover(&self.bytes).rate;
        f.debug_struct("AimdPacer")
            .field("qps_bucket", &self.qps_bucket)
            .field("file_bucket", &self.file_bucket)
            .field("bytes_rate_per_sec", &bytes_rate)
            .field("backoff_until_ms", &self.backoff_until_ms)
            .field("clean_window_start_ms", &self.clean_window_start_ms)
            .field("ceilings", &self.ceilings)
            .finish_non_exhaustive()
    }
}

impl AimdPacer {
    /// Builds a pacer at the optimistic starting budget (50 qps, 10
    /// file-creates/s; default caps 200 / 50) with an optional bandwidth
    /// cap (SPEC s9, DESIGN s18.1).
    ///
    /// `bandwidth_cap_mbps` is the user setting: `Some(mbps)` installs a
    /// byte bucket refilling at `mbps * 1_000_000 / 8` bytes/s with a
    /// burst of `2x` that rate; `None` bypasses the byte gate.
    pub fn new(clock: Arc<dyn Clock>, bandwidth_cap_mbps: Option<f64>) -> Self {
        Self::with_ceilings(clock, bandwidth_cap_mbps, PacerCeilings::default())
    }

    /// Builds a pacer with explicit starting ceilings (test seam / restored
    /// per-account state). The buckets start at `ceilings.qps` /
    /// `ceilings.file_creates_per_sec` with a `2x` burst.
    pub fn with_ceilings(
        clock: Arc<dyn Clock>,
        bandwidth_cap_mbps: Option<f64>,
        ceilings: PacerCeilings,
    ) -> Self {
        let qps_bucket = TokenBucket::new(
            ceilings.qps as f64,
            (ceilings.qps as f64) * 2.0,
            clock.clone(),
        );
        let file_bucket = TokenBucket::new(
            ceilings.file_creates_per_sec as f64,
            (ceilings.file_creates_per_sec as f64) * 2.0,
            clock.clone(),
        );
        let bytes_rate_per_sec = bandwidth_cap_mbps.map(|mbps| mbps * 1_000_000.0 / 8.0);
        let bytes_bucket = bytes_rate_per_sec.map(|rate| {
            // Burst = 2x refill (SPEC s9).
            Arc::new(TokenBucket::new(rate, rate * 2.0, clock.clone()))
        });
        let now = clock.now_ms();
        Self {
            qps_bucket,
            file_bucket,
            bytes: Mutex::new(ByteGate {
                rate: bytes_rate_per_sec,
                bucket: bytes_bucket,
            }),
            backoff_until_ms: AtomicI64::new(now),
            clean_window_start_ms: AtomicI64::new(now),
            // Never-throttled sentinel: a genuine throttle overwrites it.
            last_throttle_ms: AtomicI64::new(i64::MIN),
            ceilings: Mutex::new(ceilings),
            clock,
        }
    }

    /// The bytes/sec refill rate for a `bandwidth_cap_mbps` setting (SPEC s9).
    fn mbps_to_bytes_per_sec(mbps: f64) -> f64 {
        mbps * 1_000_000.0 / 8.0
    }

    /// Sleeps out any active backoff window, polling the wall clock so a
    /// `FakeClock` advance lifts it deterministically.
    async fn wait_out_backoff(&self) {
        loop {
            let until = self.backoff_until_ms.load(Ordering::Acquire);
            let now = self.clock.now_ms();
            if now >= until {
                return;
            }
            tokio::time::sleep(ACQUIRE_POLL).await;
        }
    }

    /// Applies the additive increase if a full clean window has accrued
    /// (DESIGN s18.1: +5 qps, +1/s file-create per 10 clean minutes, capped
    /// at the hard cap). Called on each clean (`Ok`) response. Multiple
    /// elapsed windows raise once per window passed.
    fn maybe_raise(&self, now: UnixMs) {
        loop {
            let start = self.clean_window_start_ms.load(Ordering::Acquire);
            let elapsed = now.saturating_sub(start);
            if elapsed < CLEAN_WINDOW_MS {
                return;
            }
            // Claim this window: advance the start by exactly one window so
            // a long clean stretch raises once per window, not all at once.
            let next_start = start.saturating_add(CLEAN_WINDOW_MS);
            if self
                .clean_window_start_ms
                .compare_exchange(start, next_start, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                // Another note_response moved the window; re-read and retry.
                continue;
            }
            self.raise_one_step();
        }
    }

    /// Raises both budgets by one additive step, clamped to the hard caps,
    /// and reconfigures the buckets to the new rate.
    fn raise_one_step(&self) {
        let mut c = lock_recover(&self.ceilings);
        let new_qps = (c.qps + QPS_INCREASE_STEP).min(c.max_qps);
        let new_file =
            (c.file_creates_per_sec + FILE_INCREASE_STEP).min(c.max_file_creates_per_sec);
        let changed = new_qps != c.qps || new_file != c.file_creates_per_sec;
        c.qps = new_qps;
        c.file_creates_per_sec = new_file;
        drop(c);
        if changed {
            self.qps_bucket
                .reconfigure(new_qps as f64, (new_qps as f64) * 2.0);
            self.file_bucket
                .reconfigure(new_file as f64, (new_file as f64) * 2.0);
        }
    }

    /// Halves both per-second budgets (multiplicative decrease, DESIGN
    /// s18.1), flooring each at 1/s so the account never wedges fully
    /// closed, and resets the clean-window timer to `now`.
    fn halve(&self, now: UnixMs) {
        let mut c = lock_recover(&self.ceilings);
        let new_qps = (c.qps / 2).max(1);
        let new_file = (c.file_creates_per_sec / 2).max(1);
        c.qps = new_qps;
        c.file_creates_per_sec = new_file;
        drop(c);
        self.qps_bucket
            .reconfigure(new_qps as f64, (new_qps as f64) * 2.0);
        self.file_bucket
            .reconfigure(new_file as f64, (new_file as f64) * 2.0);
        // Any throttle resets the additive-increase window (DESIGN s18.1).
        self.clean_window_start_ms.store(now, Ordering::Release);
    }

    /// Extends the backoff deadline to `now + retry_after_with_jitter`,
    /// never shortening an already-later deadline.
    fn set_backoff(&self, now: UnixMs, retry_after: Duration) {
        let wait_ms = retry_after_with_jitter(retry_after).as_millis() as i64;
        let candidate = now.saturating_add(wait_ms);
        // Keep the latest of any concurrent backoff requests.
        let mut cur = self.backoff_until_ms.load(Ordering::Acquire);
        while candidate > cur {
            match self.backoff_until_ms.compare_exchange(
                cur,
                candidate,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Pauses the account until the next midnight Pacific and re-initialises
    /// the buckets to the optimistic start (DESIGN s18.1
    /// `403 dailyLimitExceeded`).
    fn pause_until_midnight_pacific(&self, now: UnixMs) {
        let resume = next_midnight_pacific_ms(now);
        // The daily pause is a hard backoff: never shorten it.
        let mut cur = self.backoff_until_ms.load(Ordering::Acquire);
        while resume > cur {
            match self.backoff_until_ms.compare_exchange(
                cur,
                resume,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
        // Re-initialise the budgets to the optimistic start. The buckets
        // start full, but the backoff above gates them until the reset.
        let default = PacerCeilings::default();
        {
            let mut c = lock_recover(&self.ceilings);
            c.qps = default.qps;
            c.file_creates_per_sec = default.file_creates_per_sec;
        }
        self.qps_bucket
            .reconfigure(default.qps as f64, (default.qps as f64) * 2.0);
        self.file_bucket.reconfigure(
            default.file_creates_per_sec as f64,
            (default.file_creates_per_sec as f64) * 2.0,
        );
        {
            let gate = lock_recover(&self.bytes);
            if let (Some(b), Some(rate)) = (&gate.bucket, gate.rate) {
                b.reconfigure(rate, rate * 2.0);
            }
        }
        self.clean_window_start_ms.store(now, Ordering::Release);
    }
}

#[async_trait::async_trait]
impl Pacer for AimdPacer {
    async fn permit_request(&self) {
        self.wait_out_backoff().await;
        self.qps_bucket.acquire(1).await;
    }

    async fn permit_file_create(&self) {
        // Transaction gate first (SPEC s9: file-create implies a request),
        // then the stricter file-create gate.
        self.permit_request().await;
        self.file_bucket.acquire(1).await;
    }

    async fn permit_bytes(&self, n: u64) {
        // Clone the live bucket out under a BRIEF lock so the async `acquire`
        // never holds the gate lock across an await (and a concurrent
        // `set_bandwidth_cap` can swap it). A `None` bucket = unlimited no-op.
        let bucket = lock_recover(&self.bytes).bucket.clone();
        if let Some(bucket) = bucket {
            bucket.acquire(n).await;
        }
    }

    fn set_bandwidth_cap(&self, mbps: Option<f64>) {
        // V2 metered throttle (DESIGN s17): swap the effective cap at runtime.
        // Idempotent so the orchestrator may call it every cycle. A daily-quota
        // re-init rebuilds the bucket from `rate`, so updating `rate` keeps the
        // two consistent.
        let new_rate = mbps.map(Self::mbps_to_bytes_per_sec);
        let mut gate = lock_recover(&self.bytes);
        if gate.rate == new_rate {
            return;
        }
        gate.rate = new_rate;
        gate.bucket =
            new_rate.map(|rate| Arc::new(TokenBucket::new(rate, rate * 2.0, self.clock.clone())));
    }

    fn note_response(&self, classification: ResponseClass) {
        let now = self.clock.now_ms();
        match classification {
            ResponseClass::Ok => self.maybe_raise(now),
            ResponseClass::RateLimited { retry_after } => {
                self.last_throttle_ms.store(now, Ordering::Release);
                self.halve(now);
                self.set_backoff(now, retry_after);
            }
            ResponseClass::DailyQuota => {
                self.last_throttle_ms.store(now, Ordering::Release);
                self.pause_until_midnight_pacific(now);
            }
            ResponseClass::OtherError => {
                // Non-throttle: does not move the AIMD ceiling and does not
                // count as a clean window tick (DESIGN s18.1 / SPEC s9).
            }
        }
    }

    fn ceilings(&self) -> PacerCeilings {
        *lock_recover(&self.ceilings)
    }

    fn last_throttle_ms(&self) -> i64 {
        self.last_throttle_ms.load(Ordering::Acquire)
    }
}

/// Applies a backoff with jitter to Drive's `Retry-After` (DESIGN s5.4:
/// "exponential backoff with jitter ... honour `Retry-After`").
///
/// We honour the server hint as the floor and add up to 25% positive
/// jitter so a fleet of accounts (or retries) does not stampede the same
/// instant. The jitter is deterministic-free of any RNG dependency: it is
/// derived from the low bits of the retry-after itself, which is good
/// enough to decorrelate retries without adding the `rand` crate to
/// `driven-core`. A zero `Retry-After` floors at 1s so a throttle always
/// produces a real pause.
fn retry_after_with_jitter(retry_after: Duration) -> Duration {
    let base_ms = retry_after.as_millis().max(1_000) as u64;
    // Up to 25% jitter, derived from the value's own low bits (no RNG dep).
    let jitter = (base_ms / 4).max(1);
    let extra = base_ms.wrapping_mul(2_654_435_761) % (jitter + 1);
    Duration::from_millis(base_ms + extra)
}

/// Computes the next midnight Pacific after `now_ms`, in Unix epoch ms
/// (DESIGN s18.1 daily-quota reset boundary).
///
/// Uses the fixed PST offset (`UTC-8`, [`PACIFIC_OFFSET_MS`]) - see that
/// constant's doc for why a DST-aware boundary is out of scope here. The
/// computation shifts into Pacific local time, rounds *up* to the next
/// day boundary, then shifts back to UTC. If `now` is exactly midnight
/// Pacific the result is the following midnight (a daily pause should
/// always move forward at least to the next reset).
fn next_midnight_pacific_ms(now_ms: UnixMs) -> UnixMs {
    let pacific = now_ms.saturating_sub(PACIFIC_OFFSET_MS);
    // Floor to the current Pacific-day boundary. `rem_euclid` keeps the
    // remainder non-negative even for pre-epoch / negative readings.
    let into_day = pacific.rem_euclid(DAY_MS);
    let day_start = pacific - into_day;
    let next_day_start = day_start + DAY_MS;
    next_day_start + PACIFIC_OFFSET_MS
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FakeClock;
    use std::sync::Arc;
    use std::time::Duration;

    fn clock() -> (FakeClock, Arc<dyn Clock>) {
        let fake = FakeClock::new();
        let dyn_clock: Arc<dyn Clock> = Arc::new(fake.clone());
        (fake, dyn_clock)
    }

    /// Spawns `fut` and, while it is pending, repeatedly advances the
    /// FakeClock so a clock-polling acquire makes progress. Returns once the
    /// future resolves. This bridges the FakeClock<->tokio-timer gap: the
    /// pacer's acquire polls real `tokio::time::sleep(1ms)`, and this driver
    /// advances the fake wall clock each tick so refill happens.
    async fn drive<F>(fake: &FakeClock, step: Duration, fut: F)
    where
        F: std::future::Future<Output = ()>,
    {
        tokio::pin!(fut);
        loop {
            // Poll the future once.
            let done = futures::poll!(&mut fut).is_ready();
            if done {
                return;
            }
            fake.advance(step);
            // Yield so the acquire's sleep timer can fire and re-poll.
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn acquire_succeeds_immediately_when_full() {
        let (_fake, c) = clock();
        let pacer = AimdPacer::new(c, None);
        // Bucket starts full (burst 2x of 50 = 100), so 1 token is free.
        pacer.permit_request().await;
        assert_eq!(pacer.ceilings().qps, 50);
    }

    #[tokio::test]
    async fn acquire_blocks_then_refills_under_fake_clock() {
        let (fake, c) = clock();
        // Start with a tiny budget so the burst is small and we can drain
        // it, then prove refill unblocks the next acquire.
        let ceilings = PacerCeilings {
            qps: 2,
            file_creates_per_sec: 1,
            max_qps: 200,
            max_file_creates_per_sec: 50,
        };
        let pacer = Arc::new(AimdPacer::with_ceilings(c, None, ceilings));
        // Burst capacity = 2x qps = 4 tokens. Drain all 4 immediately.
        for _ in 0..4 {
            pacer.permit_request().await;
        }
        // A 5th acquire must block until the clock advances enough to
        // refill one token. Rate = 2/s => 500ms per token.
        let p2 = pacer.clone();
        drive(&fake, Duration::from_millis(100), async move {
            p2.permit_request().await;
        })
        .await;
        // It resolved only because the driver advanced the clock; assert
        // the clock moved at least ~500ms worth of refill.
        assert!(fake.now_ms() >= 500, "clock advanced to {}", fake.now_ms());
    }

    #[tokio::test]
    async fn rate_limit_halves_budget_and_sets_backoff() {
        let (fake, c) = clock();
        let pacer = AimdPacer::new(c, None);
        assert_eq!(pacer.ceilings().qps, 50);
        pacer.note_response(ResponseClass::RateLimited {
            retry_after: Duration::from_secs(2),
        });
        assert_eq!(pacer.ceilings().qps, 25, "qps halved");
        assert_eq!(pacer.ceilings().file_creates_per_sec, 5, "file halved");
        // Backoff is active: a permit must block until the clock advances
        // past the (jittered) retry-after (>= 2s floor).
        let pacer = Arc::new(pacer);
        let p2 = pacer.clone();
        drive(&fake, Duration::from_millis(100), async move {
            p2.permit_request().await;
        })
        .await;
        assert!(
            fake.now_ms() >= 2_000,
            "backoff held until >= 2s, clock at {}",
            fake.now_ms()
        );
    }

    #[tokio::test]
    async fn repeated_rate_limits_floor_at_one() {
        let (_fake, c) = clock();
        let pacer = AimdPacer::new(c, None);
        for _ in 0..20 {
            pacer.note_response(ResponseClass::RateLimited {
                retry_after: Duration::from_millis(1),
            });
        }
        let cl = pacer.ceilings();
        assert_eq!(cl.qps, 1, "qps floors at 1");
        assert_eq!(cl.file_creates_per_sec, 1, "file floors at 1");
    }

    #[tokio::test]
    async fn clean_window_raises_budget() {
        let (fake, c) = clock();
        let pacer = AimdPacer::new(c, None);
        assert_eq!(pacer.ceilings().qps, 50);
        // No raise before a full window.
        fake.advance(Duration::from_secs(9 * 60));
        pacer.note_response(ResponseClass::Ok);
        assert_eq!(pacer.ceilings().qps, 50, "no raise before 10 min");
        // Cross the 10-minute clean window: +5 qps, +1/s file.
        fake.advance(Duration::from_secs(2 * 60));
        pacer.note_response(ResponseClass::Ok);
        assert_eq!(pacer.ceilings().qps, 55, "raised +5 after clean window");
        assert_eq!(pacer.ceilings().file_creates_per_sec, 11, "raised +1/s");
    }

    #[tokio::test]
    async fn throttle_resets_clean_window() {
        let (fake, c) = clock();
        let pacer = AimdPacer::new(c, None);
        fake.advance(Duration::from_secs(9 * 60));
        // A throttle resets the window timer to now.
        pacer.note_response(ResponseClass::RateLimited {
            retry_after: Duration::from_millis(1),
        });
        let qps_after_halve = pacer.ceilings().qps;
        // Only 2 more minutes pass: not a full clean window since the reset.
        fake.advance(Duration::from_secs(2 * 60));
        pacer.note_response(ResponseClass::Ok);
        assert_eq!(
            pacer.ceilings().qps,
            qps_after_halve,
            "no raise: window was reset by the throttle"
        );
    }

    #[tokio::test]
    async fn raise_caps_at_hard_cap() {
        let (fake, c) = clock();
        let ceilings = PacerCeilings {
            qps: 198,
            file_creates_per_sec: 49,
            max_qps: 200,
            max_file_creates_per_sec: 50,
        };
        let pacer = AimdPacer::with_ceilings(c, None, ceilings);
        fake.advance(Duration::from_secs(11 * 60));
        pacer.note_response(ResponseClass::Ok);
        assert_eq!(pacer.ceilings().qps, 200, "clamped to max_qps");
        assert_eq!(
            pacer.ceilings().file_creates_per_sec,
            50,
            "clamped to max_file_creates_per_sec"
        );
        // Further clean windows do not exceed the cap.
        fake.advance(Duration::from_secs(11 * 60));
        pacer.note_response(ResponseClass::Ok);
        assert_eq!(pacer.ceilings().qps, 200);
    }

    #[tokio::test]
    async fn bytes_bucket_throttles_when_capped() {
        let (fake, c) = clock();
        // 1 Mbps = 1_000_000 bits/s = 125_000 bytes/s; burst = 250_000.
        let pacer = Arc::new(AimdPacer::new(c, Some(1.0)));
        // Drain the full burst (250_000 bytes) in one acquire.
        pacer.permit_bytes(250_000).await;
        // The next 125_000-byte acquire must wait ~1s of refill.
        let p2 = pacer.clone();
        drive(&fake, Duration::from_millis(50), async move {
            p2.permit_bytes(125_000).await;
        })
        .await;
        assert!(
            fake.now_ms() >= 900,
            "bytes acquire waited for refill, clock at {}",
            fake.now_ms()
        );
    }

    #[tokio::test]
    async fn bytes_bucket_none_is_noop() {
        let (fake, c) = clock();
        let pacer = AimdPacer::new(c, None);
        // No cap: a huge byte request returns immediately without advancing
        // the clock (no driver needed).
        pacer.permit_bytes(u64::MAX).await;
        assert_eq!(fake.now_ms(), 0, "no-op acquire did not block");
    }

    #[tokio::test]
    async fn set_bandwidth_cap_installs_a_cap_at_runtime() {
        let (fake, c) = clock();
        let pacer = Arc::new(AimdPacer::new(c, None)); // starts unlimited
        pacer.permit_bytes(u64::MAX).await;
        assert_eq!(fake.now_ms(), 0, "no cap yet: instant");

        // Install a 1 Mbps cap (125_000 B/s, burst 250_000) at runtime.
        pacer.set_bandwidth_cap(Some(1.0));
        pacer.permit_bytes(250_000).await; // drains the fresh burst, still t=0
        let p2 = pacer.clone();
        drive(&fake, Duration::from_millis(50), async move {
            p2.permit_bytes(125_000).await;
        })
        .await;
        assert!(
            fake.now_ms() >= 900,
            "throttled once the cap was installed, clock at {}",
            fake.now_ms()
        );
    }

    #[tokio::test]
    async fn set_bandwidth_cap_lifts_the_cap() {
        let (fake, c) = clock();
        let pacer = Arc::new(AimdPacer::new(c, Some(1.0))); // starts capped
        pacer.set_bandwidth_cap(None); // lift the cap
        pacer.permit_bytes(u64::MAX).await;
        assert_eq!(fake.now_ms(), 0, "cap lifted: acquire is now a no-op");
    }

    #[tokio::test]
    async fn set_bandwidth_cap_is_idempotent_for_the_same_rate() {
        let (fake, c) = clock();
        let pacer = Arc::new(AimdPacer::new(c, Some(1.0)));
        pacer.permit_bytes(250_000).await; // drain the burst at t=0
                                           // Re-applying the SAME cap must NOT rebuild (and refill) the bucket.
        pacer.set_bandwidth_cap(Some(1.0));
        let p2 = pacer.clone();
        drive(&fake, Duration::from_millis(50), async move {
            p2.permit_bytes(125_000).await;
        })
        .await;
        assert!(
            fake.now_ms() >= 900,
            "idempotent re-apply kept the drained bucket, clock at {}",
            fake.now_ms()
        );
    }

    #[tokio::test]
    async fn daily_quota_pauses_until_midnight_and_reinits() {
        let (fake, c) = clock();
        // Set the wall clock to a known instant: 2026-01-01 12:00 UTC.
        // 2026-01-01T00:00:00Z = 1_767_225_600_000 ms.
        let noon_utc = 1_767_225_600_000 + 12 * 60 * 60 * 1_000;
        fake.now_set(noon_utc);
        // Start from a depressed budget so we can prove the daily re-init
        // restores the optimistic start (50 / 10).
        let pacer = AimdPacer::with_ceilings(
            c,
            None,
            PacerCeilings {
                qps: 3,
                file_creates_per_sec: 2,
                max_qps: 200,
                max_file_creates_per_sec: 50,
            },
        );
        pacer.note_response(ResponseClass::DailyQuota);
        // Buckets re-initialised to the optimistic start.
        assert_eq!(pacer.ceilings().qps, 50, "re-init qps");
        assert_eq!(pacer.ceilings().file_creates_per_sec, 10, "re-init file");
        // Backoff is set to the next midnight Pacific. Midnight Pacific on
        // 2026-01-01 (PST, UTC-8) is 2026-01-01T08:00:00Z. Noon UTC is
        // already past that, so the resume is 2026-01-02T08:00:00Z.
        let expected = next_midnight_pacific_ms(noon_utc);
        let backoff = pacer.backoff_until_ms.load(Ordering::Acquire);
        assert_eq!(backoff, expected, "paused to next midnight Pacific");
        assert!(backoff > noon_utc, "resume is in the future");
        // A permit blocks until the wall reaches the resume instant.
        let pacer = Arc::new(pacer);
        let p2 = pacer.clone();
        // Big steps so the test does not take real time per simulated hour.
        drive(&fake, Duration::from_secs(60 * 60), async move {
            p2.permit_request().await;
        })
        .await;
        assert!(
            fake.now_ms() >= expected,
            "released at/after midnight Pacific"
        );
    }

    #[test]
    fn next_midnight_pacific_rounds_forward() {
        // 2026-01-01T08:00:00Z is exactly midnight PST. The next boundary
        // must be the following midnight, not the same instant.
        let midnight_pst_utc = 1_767_225_600_000 + 8 * 60 * 60 * 1_000;
        let next = next_midnight_pacific_ms(midnight_pst_utc);
        assert_eq!(next, midnight_pst_utc + DAY_MS);
        // Just after midnight rounds up to the same next boundary.
        let just_after = midnight_pst_utc + 1;
        assert_eq!(
            next_midnight_pacific_ms(just_after),
            midnight_pst_utc + DAY_MS
        );
        // Just before midnight rounds up to this boundary.
        let just_before = midnight_pst_utc - 1;
        assert_eq!(next_midnight_pacific_ms(just_before), midnight_pst_utc);
    }

    #[test]
    fn retry_after_jitter_honours_floor() {
        // Below the 1s floor: result is at least the 1s base.
        let d = retry_after_with_jitter(Duration::from_millis(10));
        assert!(d >= Duration::from_secs(1), "1s floor honoured");
        // Above the floor: result is >= the server hint.
        let d = retry_after_with_jitter(Duration::from_secs(4));
        assert!(d >= Duration::from_secs(4), "honours Retry-After as floor");
        // Jitter is bounded to +25%.
        assert!(d <= Duration::from_millis(4_000 + 1_000), "jitter bounded");
    }

    #[tokio::test]
    async fn other_error_does_not_move_ceiling_or_window() {
        let (fake, c) = clock();
        let pacer = AimdPacer::new(c, None);
        fake.advance(Duration::from_secs(11 * 60));
        pacer.note_response(ResponseClass::OtherError);
        // No raise (OtherError is not a clean tick) and no halve.
        assert_eq!(pacer.ceilings().qps, 50);
        // A subsequent Ok after the same elapsed time DOES raise, proving
        // OtherError neither advanced nor reset the window.
        pacer.note_response(ResponseClass::Ok);
        assert_eq!(pacer.ceilings().qps, 55, "Ok still raises; window intact");
    }

    #[tokio::test]
    async fn file_create_consumes_both_buckets() {
        let (_fake, c) = clock();
        let ceilings = PacerCeilings {
            qps: 50,
            file_creates_per_sec: 1,
            max_qps: 200,
            max_file_creates_per_sec: 50,
        };
        let pacer = AimdPacer::with_ceilings(c, None, ceilings);
        // file burst = 2x of 1 = 2 tokens; two creates succeed immediately.
        pacer.permit_file_create().await;
        pacer.permit_file_create().await;
        // The file bucket is now empty; a non-blocking try on the inner
        // file bucket confirms it.
        assert!(
            !pacer.file_bucket.try_acquire(1),
            "file bucket drained by two creates"
        );
    }
}
