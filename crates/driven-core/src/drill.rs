//! Scheduled RESTORE DRILLS: periodically prove that a random sample of
//! backed-up files can actually be restored.
//!
//! # Why this is not covered by anything else
//!
//! Every existing check verifies the backup from the WRITE side. The upload
//! path compares md5 after each upload; the deep-verify re-hashes local files;
//! the remote-existence audit proves objects still exist; the integrity scrub
//! ([`crate::scrub`]) proves their stored bytes still match what was recorded.
//! All of them ask "is what we wrote still there?" - none of them ever exercise
//! the READ side.
//!
//! That gap matters because restore is the only operation that ever runs the
//! full reverse pipeline: download, stream-decrypt with the source's key,
//! extract a bundle member, verify the plaintext hash, write the file out. A
//! bug or a lost key anywhere in that chain is invisible to every write-side
//! check and shows up exactly once - the day the user actually needs their
//! data. A drill runs that pipeline on purpose, on a schedule, while it is
//! still cheap to find out.
//!
//! # Shape
//!
//! Pick N files deterministically from the source's restorable set, restore
//! them through the REAL restore path into a temporary directory, let that path
//! verify the plaintext hash exactly as a user-initiated restore would, delete
//! the output, and persist a counts-only report.
//!
//! # Sampling
//!
//! [`sample_offsets`] is a PURE function of its seed: the same seed over the
//! same population always picks the same rows, with no RNG state to thread
//! through. The orchestrator seeds each run with the source id plus the current
//! wall clock, so successive drills cover DIFFERENT files - a sampler that kept
//! re-testing the same three would "pass" forever while the rest of the backup
//! rotted.
//!
//! That seed is not persisted, so a run is NOT reproducible from its stored
//! report; the report is a counts-and-codes summary, not a way to re-run the
//! same sample. Reproducing a specific failure means calling [`sample_offsets`]
//! with a known seed, which is what the unit tests do. Sampling reuses
//! [`crate::scrub::deterministic_sample`] rather than growing a second
//! implementation.
//!
//! # I/O-free
//!
//! Selection, classification, and the report shape are pure. The restore itself
//! is a side effect behind the [`RestoreProbe`] seam, implemented in the app
//! shell (where the real restore path and its destination-confinement machinery
//! live) and left `None` in tests + the chaos harness.
//!
//! # Privacy
//!
//! A [`DrillReport`] is COUNTS plus stable SPEC s24 ERROR CODES - never a path,
//! a remote id, or a filename. It is persisted verbatim and rendered by the UI
//! and CLI, so a drill report can no more leak an encrypted source's filenames
//! than a scrub report can (CONTRIBUTING.md house rules).

use crate::state::StateRepo;
use crate::types::{ErrorCode, RelativePath, SourceId, UnixMs};

/// Settings key: master on/off for restore drills (UI-surfaced).
pub const SETTING_DRILL_ENABLED: &str = "restore_drill_enabled";
/// Settings key: seconds between drills for one source.
pub const SETTING_DRILL_INTERVAL_SECS: &str = "restore_drill_interval_secs";
/// Settings key: how many files one drill restores.
pub const SETTING_DRILL_SAMPLE_SIZE: &str = "restore_drill_sample_size";

/// Default cadence: monthly. Deliberately slower than the weekly scrub - a
/// drill downloads and decrypts real bytes, and the failures it catches
/// (a broken restore path, an unusable key) are systemic rather than per-file,
/// so they do not need weekly sampling to be found.
pub const DEFAULT_DRILL_INTERVAL_SECS: u64 = 2_592_000;
/// Default sample: 3 files per drill. Enough to catch a systemic restore
/// failure on the first run; small enough that the bandwidth is unnoticeable.
pub const DEFAULT_DRILL_SAMPLE_SIZE: u64 = 3;

/// Lower bound for the cadence (1 day). Tighter than the scrub's 1-hour floor
/// because each drill spends real bandwidth.
pub const DRILL_INTERVAL_MIN: u32 = 86_400;
/// Upper bound for the cadence (1 year), matching the scrub.
pub const DRILL_INTERVAL_MAX: u32 = 31_536_000;
/// Lower bound for the sample. Zero would be a second, redundant kill-switch;
/// use `enabled = false` to turn drills off.
pub const DRILL_SAMPLE_MIN: u32 = 1;
/// Upper bound for the sample. Past this a "drill" is a bulk restore.
pub const DRILL_SAMPLE_MAX: u32 = 50;

/// Files larger than this are never drilled.
///
/// A drill downloads the whole object. Restoring a 5 GB video proves nothing a
/// 5 MB document does not - the pipeline under test is the same - while costing
/// the user real bandwidth and taking the cycle out of service for minutes.
pub const DRILL_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Resolved restore-drill configuration for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrillConfig {
    /// Master switch. When `false` the orchestrator never dispatches a drill.
    pub enabled: bool,
    /// Seconds between drills for one source.
    pub interval_secs: u32,
    /// How many files one drill restores.
    pub sample_size: u32,
}

impl Default for DrillConfig {
    fn default() -> Self {
        DrillConfig {
            enabled: true,
            interval_secs: DEFAULT_DRILL_INTERVAL_SECS as u32,
            sample_size: DEFAULT_DRILL_SAMPLE_SIZE as u32,
        }
    }
}

impl DrillConfig {
    /// The cadence as milliseconds, saturating rather than wrapping.
    #[must_use]
    pub fn interval_ms(&self) -> i64 {
        i64::from(self.interval_secs).saturating_mul(1_000)
    }
}

/// Reads the persisted restore-drill configuration.
///
/// Fails closed to the compile-time defaults and CLAMPS an out-of-range stored
/// value, exactly like [`crate::scrub::load_scrub_config`] - see its docs for
/// why clamping rather than rejecting is the right behaviour here.
pub async fn load_drill_config(state: &dyn StateRepo) -> DrillConfig {
    let defaults = DrillConfig::default();
    let enabled = match state.get_setting(SETTING_DRILL_ENABLED).await {
        Ok(Some(serde_json::Value::Bool(b))) => b,
        _ => defaults.enabled,
    };
    let interval_secs = read_u64(
        state,
        SETTING_DRILL_INTERVAL_SECS,
        u64::from(defaults.interval_secs),
    )
    .await;
    let sample_size = read_u64(
        state,
        SETTING_DRILL_SAMPLE_SIZE,
        u64::from(defaults.sample_size),
    )
    .await;
    DrillConfig {
        enabled,
        interval_secs: clamp(interval_secs, DRILL_INTERVAL_MIN, DRILL_INTERVAL_MAX),
        sample_size: clamp(sample_size, DRILL_SAMPLE_MIN, DRILL_SAMPLE_MAX),
    }
}

async fn read_u64(state: &dyn StateRepo, key: &str, default: u64) -> u64 {
    match state.get_setting(key).await {
        Ok(Some(v)) => v.as_u64().unwrap_or(default),
        _ => default,
    }
}

/// Clamp a persisted `u64` into the validated `u32` range with CHECKED
/// narrowing - a raw `as` would wrap `2^32` to `0`.
fn clamp(value: u64, min: u32, max: u32) -> u32 {
    let capped = value.min(u64::from(max));
    u32::try_from(capped).unwrap_or(max).max(min)
}

/// Is a source due for a restore drill?
///
/// Wall-clock only, delegating to the scrub's predicate so both scheduled jobs
/// share ONE definition of "due" and cannot drift apart (and inherit the same
/// documented backwards-wall-jump tradeoff: the failure mode is a delayed
/// drill, never a missed backup).
#[must_use]
pub fn drill_due(last_drill_at: Option<UnixMs>, now_ms: UnixMs, interval_ms: i64) -> bool {
    crate::scrub::scrub_due(last_drill_at, now_ms, interval_ms)
}

/// Where the per-source drill schedule stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrillCursor {
    /// When this source was last drilled, or `None` before its first drill.
    pub last_drill_at: Option<UnixMs>,
}

/// One file selected for a drill.
///
/// Carries only what the probe needs to find and re-verify it; the plaintext
/// hash lives in `file_state` and is checked by the restore path itself, so it
/// is deliberately NOT duplicated here (two copies of a hash is two chances to
/// compare the wrong one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrillCandidate {
    /// The source the file belongs to.
    pub source_id: SourceId,
    /// The file's path within that source.
    pub path: RelativePath,
    /// Plaintext size, used only for the size cap and the report.
    pub size: u64,
}

/// What one restore attempt concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrillAttempt {
    /// Restored and its plaintext hash matched what `file_state` records.
    Verified,
    /// Not attempted, for a reason that is NOT evidence of a broken backup -
    /// most importantly an account whose crypto suite is unavailable right now
    /// (locked keychain, account pending re-auth).
    ///
    /// The distinction is the difference between a useful feature and one users
    /// turn off: raising a data-loss alert every time a keychain happens to be
    /// locked would train people to ignore the alert that matters.
    Skipped {
        /// The stable SPEC s24 code explaining why.
        code: ErrorCode,
    },
    /// Attempted and FAILED - the file could not be restored, or the bytes that
    /// came back did not match. This is the outcome the whole feature exists to
    /// surface.
    Failed {
        /// The stable SPEC s24 code from the real restore path.
        code: ErrorCode,
    },
}

/// The side-effecting half of a drill: restore one file and verify it.
///
/// Lives behind a trait because the real restore path (destination
/// confinement, stream decryption, bundle extraction, plaintext-hash
/// verification) lives in the app shell, while the scheduling and reporting
/// belong in this I/O-free crate. The implementation is expected to restore
/// into a TEMPORARY directory and delete the output afterwards - a drill must
/// never leave decrypted plaintext lying around.
///
/// Implementations must never panic and never return an error type: every
/// outcome is one of the three [`DrillAttempt`] variants, so a drill can always
/// produce a complete report.
#[async_trait::async_trait]
pub trait RestoreProbe: Send + Sync {
    /// Restore `candidate` to a temp location, verify it, and clean up.
    async fn restore_and_verify(&self, candidate: &DrillCandidate) -> DrillAttempt;
}

/// Why a drill run ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DrillOutcome {
    /// Every attempted restore verified.
    #[default]
    Passed,
    /// At least one restore failed.
    Failed,
    /// Nothing could be attempted (no restorable files, or every candidate was
    /// skipped). Distinct from `Passed` on purpose: "we restored nothing"
    /// must never read as "we restored everything successfully".
    Inconclusive,
}

impl DrillOutcome {
    /// The stable snake_case label persisted in `drill_runs.outcome`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            DrillOutcome::Passed => "passed",
            DrillOutcome::Failed => "failed",
            DrillOutcome::Inconclusive => "inconclusive",
        }
    }

    /// Parse the persisted label back. An unknown value (a row written by a
    /// newer build) reads as `Inconclusive` - "cannot trust it", the safe
    /// direction.
    #[must_use]
    pub fn from_label(s: &str) -> Self {
        match s {
            "passed" => DrillOutcome::Passed,
            "failed" => DrillOutcome::Failed,
            _ => DrillOutcome::Inconclusive,
        }
    }
}

/// What one drill run found, in COUNTS plus stable error CODES.
///
/// No paths, no remote ids, no filenames - see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrillReport {
    /// Files selected for this drill.
    pub sampled: u64,
    /// Files restored AND verified against the recorded plaintext hash.
    pub verified: u64,
    /// Files not attempted for a benign reason (see [`DrillAttempt::Skipped`]).
    pub skipped: u64,
    /// Files that could not be restored, or whose bytes did not verify.
    pub failed: u64,
    /// Stable SPEC s24 codes for the failures, with a count each, sorted by
    /// code so the report is deterministic. Codes are a closed, non-user
    /// vocabulary, so this carries no user data.
    pub failure_codes: Vec<(String, u64)>,
    /// Why the run ended.
    pub outcome: DrillOutcome,
}

impl DrillReport {
    /// Fold one attempt into the counters.
    pub fn record(&mut self, attempt: &DrillAttempt) {
        self.sampled = self.sampled.saturating_add(1);
        match attempt {
            DrillAttempt::Verified => self.verified = self.verified.saturating_add(1),
            DrillAttempt::Skipped { .. } => self.skipped = self.skipped.saturating_add(1),
            DrillAttempt::Failed { code } => {
                self.failed = self.failed.saturating_add(1);
                let key = code.code().to_string();
                match self.failure_codes.iter_mut().find(|(c, _)| *c == key) {
                    Some((_, n)) => *n = n.saturating_add(1),
                    None => self.failure_codes.push((key, 1)),
                }
            }
        }
    }

    /// Whether this run found anything the user should act on.
    ///
    /// A passing drill writes NO activity row - the same silent-green rule the
    /// scrub and the remote-existence audit follow.
    #[must_use]
    pub fn found_anything(&self) -> bool {
        self.failed > 0
    }

    /// Recompute [`Self::outcome`] from the counters, and sort the failure
    /// codes so two runs with the same failures serialize identically.
    pub fn finish(&mut self) {
        self.failure_codes.sort_by(|a, b| a.0.cmp(&b.0));
        self.outcome = if self.failed > 0 {
            DrillOutcome::Failed
        } else if self.verified > 0 {
            DrillOutcome::Passed
        } else {
            // Nothing verified and nothing failed: either there was nothing to
            // restore or everything was skipped. Reporting that as a pass would
            // be a lie of exactly the kind this feature exists to prevent.
            DrillOutcome::Inconclusive
        };
    }
}

/// The deterministic offsets a drill run samples from a source's restorable
/// set.
///
/// `total` is the number of restorable files; the returned values are distinct
/// row offsets in `0..total`, sorted ascending, at most `sample_size` of them.
/// Seeded by `run_seed`, so the SAME seed over the SAME population always picks
/// the same rows - which is what makes a failing drill reproducible.
///
/// Offsets rather than paths on purpose: reading every path of a
/// hundred-thousand-file source just to pick three of them would cost more than
/// the restores do. The caller resolves each offset with a single indexed
/// `LIMIT 1 OFFSET n` lookup.
#[must_use]
pub fn sample_offsets(run_seed: &str, total: u64, sample_size: u32) -> Vec<u64> {
    if total == 0 || sample_size == 0 {
        return Vec::new();
    }
    let want = u64::from(sample_size).min(total);
    // Hash `seed || offset` and take the `want` lowest scores. Over the whole
    // population this is a keyed shuffle; it stays exact (no rejection loop, no
    // duplicate offsets) and needs no RNG state or `rand` dependency, which
    // `driven-core` does not carry.
    //
    // For a large population, scoring every offset would be wasteful, so the
    // candidate window is capped: we score at most `SCORING_WINDOW` evenly
    // spaced offsets and pick from those.
    //
    // Be precise about what that costs. The draw is uniform over the STRATUM -
    // the roughly `SCORING_WINDOW` lattice offsets `0, stride, 2*stride, ...` -
    // not over every file. On a source larger than the window, a file whose
    // offset is not on the lattice is never drilled while the population stays
    // that size. That is an accepted trade: the drill exists to catch SYSTEMIC
    // failures (a broken restore path, an unusable key), which any sample
    // exposes, and the alternative is hashing a million keys to pick three. The
    // lattice also shifts whenever the population size changes, since `stride`
    // is derived from `total`. Deterministic and bounded regardless of source
    // size.
    const SCORING_WINDOW: u64 = 4_096;
    let stride = total.div_ceil(SCORING_WINDOW).max(1);
    let window: Vec<u64> = (0..total).step_by(stride as usize).collect();
    let keys: Vec<String> = window.iter().map(u64::to_string).collect();
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let picked = crate::scrub::deterministic_sample(run_seed, &refs, want as usize);
    let mut out: Vec<u64> = picked.into_iter().map(|i| window[i]).collect();
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(p: &str) -> RelativePath {
        RelativePath::try_from(p.to_string()).expect("valid relative path")
    }

    fn candidate(path: &str) -> DrillCandidate {
        DrillCandidate {
            source_id: SourceId::new_v4(),
            path: rel(path),
            size: 10,
        }
    }

    // --- sampling determinism ------------------------------------------------

    #[test]
    fn sampling_is_reproducible_for_the_same_run_seed() {
        // The property the whole feature's diagnosability rests on: a report
        // saying "2 of 3 failed" is only actionable if a re-run examines the
        // SAME two.
        let a = sample_offsets("run-7", 1_000, 3);
        let b = sample_offsets("run-7", 1_000, 3);
        assert_eq!(a, b);
        assert_eq!(a.len(), 3);
    }

    #[test]
    fn sampling_moves_between_run_seeds() {
        let a = sample_offsets("run-1", 1_000, 5);
        let b = sample_offsets("run-2", 1_000, 5);
        assert_ne!(
            a, b,
            "successive drills must not keep testing the same files"
        );
    }

    #[test]
    fn sampled_offsets_are_distinct_sorted_and_in_range() {
        let picked = sample_offsets("seed", 500, 10);
        assert_eq!(picked.len(), 10);
        for w in picked.windows(2) {
            assert!(w[0] < w[1], "offsets must be distinct and ascending");
        }
        assert!(picked.iter().all(|o| *o < 500));
    }

    #[test]
    fn sampling_caps_at_the_population_size() {
        assert_eq!(sample_offsets("seed", 2, 10), vec![0, 1]);
        assert_eq!(sample_offsets("seed", 1, 10), vec![0]);
    }

    #[test]
    fn sampling_an_empty_population_picks_nothing() {
        assert!(sample_offsets("seed", 0, 5).is_empty());
        assert!(sample_offsets("seed", 100, 0).is_empty());
    }

    #[test]
    fn sampling_stays_bounded_on_a_huge_source() {
        // A million-file source must not cost a million hashes to pick 3 files.
        let picked = sample_offsets("seed", 1_000_000, 3);
        assert_eq!(picked.len(), 3);
        assert!(picked.iter().all(|o| *o < 1_000_000));
        // Still reproducible at that size.
        assert_eq!(picked, sample_offsets("seed", 1_000_000, 3));
    }

    #[test]
    fn sampling_spreads_across_the_population_rather_than_clustering_at_the_start() {
        // A sampler that always picked the first N rows would "pass" forever
        // while the rest of the backup rotted.
        let picked = sample_offsets("seed", 10_000, 20);
        let max = picked.iter().copied().max().unwrap();
        assert!(
            max > 1_000,
            "the sample must reach past the head of the population, got max {max}"
        );
    }

    // --- report --------------------------------------------------------------

    #[test]
    fn a_fully_verified_drill_passes_silently() {
        let mut r = DrillReport::default();
        for _ in 0..3 {
            r.record(&DrillAttempt::Verified);
        }
        r.finish();
        assert_eq!(r.sampled, 3);
        assert_eq!(r.verified, 3);
        assert_eq!(r.outcome, DrillOutcome::Passed);
        assert!(!r.found_anything());
    }

    #[test]
    fn a_failing_drill_records_the_code_and_reports() {
        let mut r = DrillReport::default();
        r.record(&DrillAttempt::Verified);
        r.record(&DrillAttempt::Failed {
            code: ErrorCode::DriveUnreachable,
        });
        r.record(&DrillAttempt::Failed {
            code: ErrorCode::DriveUnreachable,
        });
        r.record(&DrillAttempt::Failed {
            code: ErrorCode::CryptoDecryptFailed,
        });
        r.finish();
        assert_eq!(r.failed, 3);
        assert_eq!(r.verified, 1);
        assert_eq!(r.outcome, DrillOutcome::Failed);
        assert!(r.found_anything());
        assert_eq!(
            r.failure_codes,
            vec![
                ("crypto.decrypt_failed".to_string(), 1),
                ("drive.unreachable".to_string(), 2),
            ],
            "codes are aggregated with counts and sorted for a deterministic report"
        );
    }

    #[test]
    fn a_skip_is_not_a_failure_and_never_raises_an_alert() {
        // A locked keychain is not evidence of a broken backup. Alerting on it
        // would train users to ignore the alert that matters.
        let mut r = DrillReport::default();
        r.record(&DrillAttempt::Skipped {
            code: ErrorCode::CryptoKeyMissing,
        });
        r.finish();
        assert_eq!(r.skipped, 1);
        assert_eq!(r.failed, 0);
        assert!(!r.found_anything());
        assert!(r.failure_codes.is_empty());
    }

    #[test]
    fn a_drill_that_verified_nothing_is_inconclusive_not_a_pass() {
        // The lie this guards against: "we restored nothing" must never render
        // as "we restored everything successfully".
        let mut all_skipped = DrillReport::default();
        all_skipped.record(&DrillAttempt::Skipped {
            code: ErrorCode::CryptoKeyMissing,
        });
        all_skipped.finish();
        assert_eq!(all_skipped.outcome, DrillOutcome::Inconclusive);

        let mut nothing = DrillReport::default();
        nothing.finish();
        assert_eq!(nothing.outcome, DrillOutcome::Inconclusive);
        assert!(!nothing.found_anything());
    }

    #[test]
    fn one_failure_among_skips_still_fails_the_run() {
        let mut r = DrillReport::default();
        r.record(&DrillAttempt::Skipped {
            code: ErrorCode::CryptoKeyMissing,
        });
        r.record(&DrillAttempt::Failed {
            code: ErrorCode::DriveChecksumMismatch,
        });
        r.finish();
        assert_eq!(r.outcome, DrillOutcome::Failed);
    }

    #[test]
    fn outcome_labels_round_trip_and_unknown_reads_as_inconclusive() {
        for o in [
            DrillOutcome::Passed,
            DrillOutcome::Failed,
            DrillOutcome::Inconclusive,
        ] {
            assert_eq!(DrillOutcome::from_label(o.label()), o);
        }
        assert_eq!(
            DrillOutcome::from_label("something-newer"),
            DrillOutcome::Inconclusive
        );
    }

    // --- config + cadence ----------------------------------------------------

    #[test]
    fn the_shipped_defaults_are_a_monthly_three_file_drill() {
        let d = DrillConfig::default();
        assert!(d.enabled);
        assert_eq!(d.interval_secs, 2_592_000);
        assert_eq!(d.sample_size, 3);
        assert_eq!(d.interval_ms(), 2_592_000_000);
    }

    #[test]
    fn out_of_range_persisted_values_clamp_instead_of_wedging_the_drill() {
        assert_eq!(clamp(0, DRILL_SAMPLE_MIN, DRILL_SAMPLE_MAX), 1);
        assert_eq!(clamp(u64::MAX, DRILL_SAMPLE_MIN, DRILL_SAMPLE_MAX), 50);
        // The cast trap: `u64::from(u32::MAX) + 1 as u32` is 0, which would
        // silently disable the feature.
        assert_eq!(
            clamp(
                u64::from(u32::MAX) + 1,
                DRILL_INTERVAL_MIN,
                DRILL_INTERVAL_MAX
            ),
            DRILL_INTERVAL_MAX
        );
        assert_eq!(clamp(5, DRILL_SAMPLE_MIN, DRILL_SAMPLE_MAX), 5);
    }

    #[test]
    fn a_never_drilled_source_is_due_and_a_just_drilled_one_is_not() {
        let interval = DrillConfig::default().interval_ms();
        assert!(drill_due(None, 0, interval));
        assert!(!drill_due(Some(1_000), 1_000 + interval - 1, interval));
        assert!(drill_due(Some(1_000), 1_000 + interval, interval));
    }

    #[test]
    fn candidate_carries_no_hash_so_there_is_only_one_source_of_truth() {
        // Guard on the shape, not behaviour: the restore path verifies against
        // `file_state`, and a second copy of the hash here would be a second
        // chance to compare the wrong one.
        let c = candidate("docs/a.txt");
        assert_eq!(c.path.as_str(), "docs/a.txt");
        assert_eq!(c.size, 10);
    }
}
