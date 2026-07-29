//! Scheduled integrity scrub of remote objects (the remote half of DESIGN
//! s3.3's "periodic deep-verify").
//!
//! # What this adds over what already shipped
//!
//! Driven already re-checks two of the three things that can silently rot:
//!
//! - **Local bit-rot** - the deep-verify scan (`ScanMode::DeepVerify`,
//!   DESIGN s5.2 step 4) re-hashes every local file weekly and re-uploads a
//!   file whose plaintext BLAKE3 stopped matching `file_state`.
//! - **Remote EXISTENCE** - the remote-existence audit
//!   ([`crate::executor::Executor::audit_remote_existence`]) diffs every
//!   recorded `drive_file_id` against the ids Drive still reports and
//!   re-queues whatever vanished.
//!
//! Nothing re-checked the third: an object that still EXISTS but whose stored
//! bytes no longer match what `file_state` claims about them - a truncated
//! object, a re-uploaded-by-hand replacement, remote-side corruption. Drive
//! publishes `md5Checksum` for every object and Driven already records the
//! md5 of the exact bytes it sent (`file_state.drive_md5` /
//! `bundles.drive_md5`), so that comparison costs one metadata GET per object
//! and no download at all. This module is that comparison.
//!
//! # Rolling slice
//!
//! A source can hold hundreds of thousands of objects, so a run checks a
//! bounded SLICE and remembers where it stopped ([`ScrubCursor`]). The next
//! run resumes after that key and wraps to the beginning when it runs off the
//! end, so successive runs sweep the whole source without any single run
//! issuing an unbounded number of requests. Files and bundles are two
//! independent populations (a bundled member has `drive_file_id IS NULL` by
//! the migration-0007 invariant, and its bytes' md5 lives on `bundles`), so
//! each gets its own cursor and its own per-run cap.
//!
//! # I/O-free
//!
//! Everything here is pure or takes a [`StateRepo`] seam: slice cursor
//! arithmetic, drift classification, deterministic sampling, and the report
//! shape. The remote calls live in the executor
//! ([`crate::executor::Executor::scrub_slice`]), which owns the
//! `RemoteStore` + `Pacer` seams.
//!
//! # Privacy
//!
//! A [`ScrubReport`] is COUNTS ONLY - no relative paths, no Drive ids, no
//! object names. It is persisted verbatim into `scrub_runs` and rendered by
//! the UI and CLI, so keeping it counts-only means the scrub adds no new
//! egress surface for encrypted-source filenames (CONTRIBUTING.md house
//! rules).

use crate::state::StateRepo;
use crate::types::{RelativePath, UnixMs};
use driven_remote::remote_store::RemoteEntry;

/// Settings key: master on/off for the scrub (UI-surfaced).
pub const SETTING_SCRUB_ENABLED: &str = "scrub_enabled";
/// Settings key: seconds between scrub runs for one source.
pub const SETTING_SCRUB_INTERVAL_SECS: &str = "scrub_interval_secs";
/// Settings key: how many objects of EACH population (files, bundles) one run
/// checks.
pub const SETTING_SCRUB_SLICE_SIZE: &str = "scrub_slice_size";
/// Settings key: how many of the slice's objects one run additionally
/// DOWNLOADS and re-hashes end-to-end.
pub const SETTING_SCRUB_DEEP_SAMPLE: &str = "scrub_deep_sample";

/// Default cadence: weekly, matching `deep_verify_interval_secs` - the scrub
/// is the remote half of the same question, so it runs on the same clock.
pub const DEFAULT_SCRUB_INTERVAL_SECS: u64 = 604_800;
/// Default per-population slice. 500 metadata GETs per population is a
/// rounding error against Drive's per-minute query budget, and it is paced
/// like every other remote call.
pub const DEFAULT_SCRUB_SLICE_SIZE: u64 = 500;
/// Default deep sample: ZERO. A deep check DOWNLOADS the object; the metadata
/// comparison already catches remote-side corruption using Drive's own
/// checksum, so paying real bandwidth by default would be a poor trade. The
/// setting exists for users who do not trust the provider's checksum.
pub const DEFAULT_SCRUB_DEEP_SAMPLE: u64 = 0;

/// Lower bound for the cadence (1 hour), mirroring `DEEP_VERIFY_MIN`.
pub const SCRUB_INTERVAL_MIN: u32 = 3_600;
/// Upper bound for the cadence (1 year), mirroring `DEEP_VERIFY_MAX`.
pub const SCRUB_INTERVAL_MAX: u32 = 31_536_000;
/// Lower bound for the per-population slice.
pub const SCRUB_SLICE_MIN: u32 = 10;
/// Upper bound for the per-population slice. Past this a single run stops
/// being a "slice" and starts being an unbounded sweep.
pub const SCRUB_SLICE_MAX: u32 = 10_000;
/// Upper bound for the deep sample. Each one is a full object download.
pub const SCRUB_DEEP_SAMPLE_MAX: u32 = 100;

/// Objects larger than this are never chosen for a DEEP check.
///
/// A deep check streams the whole object back to re-hash it. The metadata
/// comparison already covers every object regardless of size, so declining to
/// download a multi-hundred-megabyte object costs almost no detection power
/// while keeping one scrub run from turning into an hours-long transfer on a
/// slow link.
pub const SCRUB_DEEP_MAX_OBJECT_BYTES: u64 = 256 * 1024 * 1024;

/// How many objects a deep sample may download before the run gives up on the
/// remaining budget, regardless of the configured `deep_sample`. Belt and
/// braces against a misconfigured setting; the validator already caps the
/// setting itself.
pub const SCRUB_DEEP_SAMPLE_HARD_CAP: usize = SCRUB_DEEP_SAMPLE_MAX as usize;

/// Resolved scrub configuration for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrubConfig {
    /// Master switch. When `false` the orchestrator never dispatches a run.
    pub enabled: bool,
    /// Seconds between runs for one source.
    pub interval_secs: u32,
    /// How many objects of EACH population one run checks.
    pub slice_size: u32,
    /// How many of the slice's objects one run additionally downloads and
    /// re-hashes end-to-end.
    pub deep_sample: u32,
}

impl Default for ScrubConfig {
    fn default() -> Self {
        ScrubConfig {
            enabled: true,
            interval_secs: DEFAULT_SCRUB_INTERVAL_SECS as u32,
            slice_size: DEFAULT_SCRUB_SLICE_SIZE as u32,
            deep_sample: DEFAULT_SCRUB_DEEP_SAMPLE as u32,
        }
    }
}

impl ScrubConfig {
    /// The cadence as milliseconds, saturating rather than wrapping.
    #[must_use]
    pub fn interval_ms(&self) -> i64 {
        i64::from(self.interval_secs).saturating_mul(1_000)
    }
}

/// Reads the persisted scrub configuration.
///
/// Every read FAILS CLOSED to the compile-time default (a missing key, an
/// unreadable settings table, or a value of the wrong JSON type all yield the
/// default) - the same discipline as
/// [`crate::planner::load_bundle_config`]. Values outside the validated
/// bounds are CLAMPED rather than rejected: the app-side validator already
/// refuses a bad write, so an out-of-range value here can only come from a
/// hand-edited DB, and clamping keeps the scrub running instead of wedging it.
pub async fn load_scrub_config(state: &dyn StateRepo) -> ScrubConfig {
    let defaults = ScrubConfig::default();
    let enabled = read_bool_setting(state, SETTING_SCRUB_ENABLED, defaults.enabled).await;
    let interval_secs = read_u64_setting(
        state,
        SETTING_SCRUB_INTERVAL_SECS,
        u64::from(defaults.interval_secs),
    )
    .await;
    let slice_size = read_u64_setting(
        state,
        SETTING_SCRUB_SLICE_SIZE,
        u64::from(defaults.slice_size),
    )
    .await;
    let deep_sample = read_u64_setting(
        state,
        SETTING_SCRUB_DEEP_SAMPLE,
        u64::from(defaults.deep_sample),
    )
    .await;

    ScrubConfig {
        enabled,
        interval_secs: clamp_u64_to_u32(interval_secs, SCRUB_INTERVAL_MIN, SCRUB_INTERVAL_MAX),
        slice_size: clamp_u64_to_u32(slice_size, SCRUB_SLICE_MIN, SCRUB_SLICE_MAX),
        deep_sample: clamp_u64_to_u32(deep_sample, 0, SCRUB_DEEP_SAMPLE_MAX),
    }
}

/// Clamps a persisted `u64` into the validated `u32` range. A checked
/// conversion, never `as` - a raw cast would wrap `2^32` to `0` and silently
/// disable the feature.
fn clamp_u64_to_u32(value: u64, min: u32, max: u32) -> u32 {
    let capped = value.min(u64::from(max));
    let narrowed = u32::try_from(capped).unwrap_or(max);
    narrowed.max(min)
}

/// Reads one boolean setting, failing closed to `default`.
async fn read_bool_setting(state: &dyn StateRepo, key: &str, default: bool) -> bool {
    match state.get_setting(key).await {
        Ok(Some(serde_json::Value::Bool(b))) => b,
        _ => default,
    }
}

/// Reads one unsigned-integer setting, failing closed to `default`.
async fn read_u64_setting(state: &dyn StateRepo, key: &str, default: u64) -> u64 {
    match state.get_setting(key).await {
        Ok(Some(v)) => v.as_u64().unwrap_or(default),
        _ => default,
    }
}

/// Is a source due for a scrub?
///
/// Wall-clock only, deliberately matching
/// [`crate::orchestrator::SyncOrchestrator::deep_verify_due`]'s documented
/// tradeoff: a backwards wall jump makes the next scrub look "not yet due"
/// until the clock catches up, and the failure mode of a DELAYED scrub is
/// nothing worse than a delayed report. A never-scrubbed source is always due.
#[must_use]
pub fn scrub_due(last_scrub_at: Option<UnixMs>, now_ms: UnixMs, interval_ms: i64) -> bool {
    match last_scrub_at {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= interval_ms,
    }
}

/// Where a rolling scrub stopped, per source.
///
/// Both cursors are EXCLUSIVE lower bounds for the next run's keyset page
/// (`WHERE key > cursor ORDER BY key LIMIT n`). `None` means "start from the
/// beginning", which is both the initial state and the wrapped state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubCursor {
    /// Last `file_state.relative_path` checked, or `None` at the start.
    pub file_cursor: Option<String>,
    /// Last `bundles.id` checked, or `None` at the start.
    pub bundle_cursor: Option<String>,
    /// When this source was last scrubbed to completion.
    pub last_scrub_at: Option<UnixMs>,
}

/// Which population a scrub candidate came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScrubTarget {
    /// A standalone per-file Drive object.
    File {
        /// The `file_state` row's path - the repair key.
        path: RelativePath,
    },
    /// A `.tar.gz` bundle object holding many members' bytes.
    Bundle {
        /// The `bundles` row id - the repair key.
        bundle_id: String,
    },
}

/// One recorded object the scrub will check against the remote.
///
/// Deliberately lean (four fields, no `FileStateRow`): a source can hold
/// hundreds of thousands of rows and the check needs exactly the recorded
/// claims about the STORED bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubCandidate {
    /// Which population, and the key used to repair it.
    pub target: ScrubTarget,
    /// The remote object id this candidate claims to own.
    pub drive_file_id: String,
    /// Byte size of the object AS STORED (ciphertext size for an encrypted
    /// source), so it is directly comparable to `RemoteEntry::size`.
    pub size: u64,
    /// md5 of the bytes as stored, when recorded. `None` for a row written
    /// before the md5 was captured, which reads as "cannot verify", never as
    /// "mismatch".
    pub md5: Option<[u8; 16]>,
}

impl ScrubCandidate {
    /// The keyset-pagination cursor key for this candidate: the relative path
    /// for a file, the bundle id for a bundle. Ordering by this key is what
    /// makes the rolling slice deterministic and gap-free.
    #[must_use]
    pub fn cursor_key(&self) -> &str {
        match &self.target {
            ScrubTarget::File { path } => path.as_str(),
            ScrubTarget::Bundle { bundle_id } => bundle_id.as_str(),
        }
    }
}

/// What one object's check concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drift {
    /// Exists, right size, and (where both sides publish one) matching md5.
    Ok,
    /// The remote object is not in the source's live-object set - deleted,
    /// hard-deleted, or trashed. Repairable: the same re-queue the
    /// remote-existence audit performs.
    Missing,
    /// Exists but its stored byte length differs from what was recorded. Not
    /// auto-repairable (see [`ScrubReport::unrecoverable`]).
    SizeMismatch,
    /// Exists at the right size but its md5 differs from what was recorded.
    /// Not auto-repairable.
    HashMismatch,
    /// Exists, but the comparison could not be made: one side publishes no
    /// checksum (a legacy row, or a backend that exposes none) or the metadata
    /// lookup itself failed. NEVER counted as damage.
    Unverifiable,
}

impl Drift {
    /// Whether this outcome describes actual damage (as opposed to "fine" or
    /// "could not tell").
    #[must_use]
    pub fn is_drift(self) -> bool {
        matches!(
            self,
            Drift::Missing | Drift::SizeMismatch | Drift::HashMismatch
        )
    }

    /// Whether the scrub can repair this itself by re-queuing the object for a
    /// fresh upload.
    ///
    /// ONLY [`Drift::Missing`] is repairable, and that is a deliberate,
    /// conservative limit rather than an oversight. The repair is
    /// [`StateRepo::requeue_file_state_for_reupload`], which NULLs
    /// `drive_file_id`; that is exactly right when the object is gone, and
    /// exactly wrong when the object is still there - it would orphan a live
    /// Drive object with nothing referencing it, silently consuming the user's
    /// quota forever. A size/hash mismatch is therefore REPORTED, not
    /// "repaired".
    #[must_use]
    pub fn is_repairable(self) -> bool {
        matches!(self, Drift::Missing)
    }

    /// The stable snake_case label used in reports and tests.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Drift::Ok => "ok",
            Drift::Missing => "missing",
            Drift::SizeMismatch => "size_mismatch",
            Drift::HashMismatch => "hash_mismatch",
            Drift::Unverifiable => "unverifiable",
        }
    }
}

/// Classifies one candidate against what the remote reports.
///
/// `live` answers "is this object still in the source's live-object set?" -
/// sourced from
/// [`driven_remote::remote_store::RemoteStore::list_source_object_ids`], the
/// SAME all-or-nothing enumeration the remote-existence audit infers deletion
/// from. Using the identical oracle is what keeps the two passes from
/// disagreeing about one object (a scrub that healed what the audit considers
/// live, or vice versa, would oscillate).
///
/// `observed` is the per-object metadata, `None` when the lookup failed or was
/// not attempted. A failed lookup is [`Drift::Unverifiable`], never damage:
/// the scrub must infer breakage from POSITIVE evidence, never from an error.
#[must_use]
pub fn classify(candidate: &ScrubCandidate, live: bool, observed: Option<&RemoteEntry>) -> Drift {
    if !live {
        return Drift::Missing;
    }
    let Some(entry) = observed else {
        return Drift::Unverifiable;
    };
    // A trashed object is not restorable-in-place and is exactly what the
    // audit's `trashed = false` listing filter already reads as gone, so
    // classify it identically rather than inventing a third state.
    if entry.trashed {
        return Drift::Missing;
    }
    match entry.size {
        Some(size) if size != candidate.size => return Drift::SizeMismatch,
        // A folder (or a backend that publishes no size) cannot be
        // size-checked; fall through to the checksum comparison.
        Some(_) | None => {}
    }
    match (candidate.md5, entry.md5) {
        (Some(recorded), Some(remote)) if recorded != remote => Drift::HashMismatch,
        (Some(_), Some(_)) => Drift::Ok,
        // Either side missing a checksum means the strongest available check
        // was the size comparison above, which passed. Report that honestly
        // rather than claiming a verification that did not happen.
        _ => Drift::Unverifiable,
    }
}

/// Why a run ended.
///
/// `Clean` is the [`Default`] so a freshly-built [`ScrubReport`] reads as "no
/// problems found yet" rather than needing the caller to remember to set it;
/// [`ScrubReport::finish`] recomputes it from the counters at the end of a run
/// that completed, and an aborted run sets [`ScrubOutcome::Incomplete`]
/// explicitly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ScrubOutcome {
    /// Every object in the slice checked out.
    #[default]
    Clean,
    /// At least one object drifted.
    Drift,
    /// The run could not complete (the live-object enumeration failed), so it
    /// wrote nothing and advanced no cursor.
    Incomplete,
}

impl ScrubOutcome {
    /// The stable snake_case label persisted in `scrub_runs.outcome`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ScrubOutcome::Clean => "clean",
            ScrubOutcome::Drift => "drift",
            ScrubOutcome::Incomplete => "incomplete",
        }
    }

    /// Parses the persisted label back, defaulting to
    /// [`ScrubOutcome::Incomplete`] for an unknown value (a forward-compatible
    /// read of a row written by a newer version reads as "cannot trust it",
    /// which is the safe direction).
    #[must_use]
    pub fn from_label(s: &str) -> Self {
        match s {
            "clean" => ScrubOutcome::Clean,
            "drift" => ScrubOutcome::Drift,
            _ => ScrubOutcome::Incomplete,
        }
    }
}

/// What one scrub run found, in COUNTS ONLY.
///
/// No paths, no ids, no names - see the module docs. The whole struct is
/// persisted into `scrub_runs` and rendered verbatim by the UI and CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScrubReport {
    /// Objects whose check completed this run (the slice size actually used).
    pub checked: u64,
    /// Objects that verified clean.
    pub ok: u64,
    /// Objects whose remote copy is gone.
    pub missing: u64,
    /// Objects whose stored byte length no longer matches.
    pub size_mismatch: u64,
    /// Objects whose stored md5 no longer matches.
    pub hash_mismatch: u64,
    /// Objects that could not be compared (no checksum on one side, or the
    /// metadata lookup failed).
    pub unverifiable: u64,
    /// Objects the scrub REPAIRED by re-queuing them for a fresh upload.
    pub healed: u64,
    /// Files re-queued as a consequence of a healed BUNDLE object. Counted
    /// separately because one dead bundle can strand hundreds of members.
    pub healed_bundle_members: u64,
    /// Drift the scrub found but could NOT repair - every size/hash mismatch,
    /// plus any repair attempt that itself failed. The number that means "a
    /// human needs to look at this".
    pub unrecoverable: u64,
    /// Objects additionally downloaded and re-hashed end-to-end this run.
    pub deep_checked: u64,
    /// Deep checks whose downloaded bytes did not re-hash to the recorded md5.
    pub deep_failed: u64,
    /// Whether a cursor wrapped past the end of its population this run - i.e.
    /// the rolling sweep completed a full lap.
    pub wrapped: bool,
    /// Why the run ended.
    pub outcome: ScrubOutcome,
}

impl ScrubReport {
    /// Records one classification against the counters.
    pub fn record(&mut self, drift: Drift) {
        self.checked = self.checked.saturating_add(1);
        match drift {
            Drift::Ok => self.ok = self.ok.saturating_add(1),
            Drift::Missing => self.missing = self.missing.saturating_add(1),
            Drift::SizeMismatch => self.size_mismatch = self.size_mismatch.saturating_add(1),
            Drift::HashMismatch => self.hash_mismatch = self.hash_mismatch.saturating_add(1),
            Drift::Unverifiable => self.unverifiable = self.unverifiable.saturating_add(1),
        }
    }

    /// Total drift found, repairable or not.
    #[must_use]
    pub fn drift_total(&self) -> u64 {
        self.missing
            .saturating_add(self.size_mismatch)
            .saturating_add(self.hash_mismatch)
    }

    /// Whether this run found anything worth telling the user about.
    ///
    /// A clean run writes NO activity row: a backup tool that logs "nothing
    /// was wrong" every week trains its users to ignore the log (the same
    /// silent-green rule the remote-existence audit follows).
    #[must_use]
    pub fn found_anything(&self) -> bool {
        self.drift_total() > 0 || self.deep_failed > 0 || self.unrecoverable > 0
    }

    /// Recomputes [`Self::outcome`] from the counters. Called once at the end
    /// of a run that completed; an aborted run sets
    /// [`ScrubOutcome::Incomplete`] directly and never calls this.
    pub fn finish(&mut self) {
        self.outcome = if self.found_anything() {
            ScrubOutcome::Drift
        } else {
            ScrubOutcome::Clean
        };
    }
}

/// Picks up to `n` of `keys` deterministically for a given run seed.
///
/// Reproducibility is the point: given the same seed and the same key list,
/// the same objects are picked on every machine and every re-run, so a
/// reported failure can be reproduced exactly. Implemented by sorting on
/// `BLAKE3(seed || 0x00 || key)` and taking the first `n` - a keyed shuffle
/// that needs no RNG state, no `rand` dependency (which `driven-core` does not
/// carry), and no per-platform reproducibility caveats.
///
/// Returned indices are sorted ASCENDING so the caller walks the original list
/// in order; the SELECTION is pseudo-random, the ITERATION is not.
#[must_use]
pub fn deterministic_sample(seed: &str, keys: &[&str], n: usize) -> Vec<usize> {
    if n == 0 || keys.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<([u8; 32], usize)> = keys
        .iter()
        .enumerate()
        .map(|(idx, key)| {
            let mut hasher = blake3::Hasher::new();
            hasher.update(seed.as_bytes());
            // A domain separator, so seed "ab" + key "c" cannot collide with
            // seed "a" + key "bc".
            hasher.update(&[0u8]);
            hasher.update(key.as_bytes());
            (*hasher.finalize().as_bytes(), idx)
        })
        .collect();
    // Sort by (hash, idx): the index tiebreak keeps the order total even if
    // two identical keys appear, so the result is a pure function of the
    // inputs.
    scored.sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut picked: Vec<usize> = scored.into_iter().take(n).map(|(_, idx)| idx).collect();
    picked.sort_unstable();
    picked
}

/// Advances a rolling cursor after a page of `page_len` candidates was checked.
///
/// Returns `(next_cursor, wrapped)`.
///
/// - A FULL page (`page_len >= slice_size`) keeps the page's last key as the
///   exclusive lower bound for the next run.
/// - A SHORT page means the population ran out, so the cursor resets to `None`
///   and the next run starts a fresh lap. An EMPTY page resets too - it means
///   the previous run ended exactly on the last key, and without the reset the
///   cursor would be pinned past the end forever and this population would
///   never be checked again.
///
/// `had_cursor` says whether this run started mid-lap. It exists only to keep
/// `wrapped` MEANINGFUL: `wrapped` reports "the rolling sweep completed a lap",
/// and a population that is entirely EMPTY (no cursor, no rows - e.g. a source
/// with bundling switched off, which has zero `bundles` rows forever) completes
/// no lap. Without this the flag would be `true` on literally every run and
/// would tell the user nothing.
#[must_use]
pub fn advance_cursor(
    last_key: Option<&str>,
    page_len: usize,
    slice_size: u32,
    had_cursor: bool,
) -> (Option<String>, bool) {
    let full_page = page_len >= slice_size as usize && page_len > 0;
    if full_page {
        (last_key.map(str::to_string), false)
    } else {
        (None, page_len > 0 || had_cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn rel(p: &str) -> RelativePath {
        RelativePath::try_from(p.to_string()).expect("valid relative path")
    }

    fn file_candidate(path: &str, size: u64, md5: Option<[u8; 16]>) -> ScrubCandidate {
        ScrubCandidate {
            target: ScrubTarget::File { path: rel(path) },
            drive_file_id: format!("id-{path}"),
            size,
            md5,
        }
    }

    fn entry(id: &str, size: Option<u64>, md5: Option<[u8; 16]>, trashed: bool) -> RemoteEntry {
        RemoteEntry {
            id: id.to_string(),
            name: "opaque".to_string(),
            parents: Vec::new(),
            size,
            md5,
            mime_type: "application/octet-stream".to_string(),
            modified_time: 0,
            trashed,
            app_properties: HashMap::new(),
        }
    }

    // --- classify -----------------------------------------------------------

    #[test]
    fn classify_reports_ok_when_size_and_md5_both_match() {
        let c = file_candidate("a.txt", 10, Some([7u8; 16]));
        let e = entry("id-a.txt", Some(10), Some([7u8; 16]), false);
        assert_eq!(classify(&c, true, Some(&e)), Drift::Ok);
    }

    #[test]
    fn classify_reports_missing_when_the_object_is_not_in_the_live_set() {
        let c = file_candidate("a.txt", 10, Some([7u8; 16]));
        // Even WITH metadata in hand: the live set is the oracle the
        // remote-existence audit uses, so the two passes agree.
        let e = entry("id-a.txt", Some(10), Some([7u8; 16]), false);
        assert_eq!(classify(&c, false, Some(&e)), Drift::Missing);
        assert_eq!(classify(&c, false, None), Drift::Missing);
    }

    #[test]
    fn classify_reports_missing_for_a_trashed_object() {
        let c = file_candidate("a.txt", 10, Some([7u8; 16]));
        let e = entry("id-a.txt", Some(10), Some([7u8; 16]), true);
        assert_eq!(classify(&c, true, Some(&e)), Drift::Missing);
    }

    #[test]
    fn classify_reports_size_mismatch_before_looking_at_the_checksum() {
        let c = file_candidate("a.txt", 10, Some([7u8; 16]));
        // md5 also differs; size is the more specific finding and wins.
        let e = entry("id-a.txt", Some(11), Some([9u8; 16]), false);
        assert_eq!(classify(&c, true, Some(&e)), Drift::SizeMismatch);
    }

    #[test]
    fn classify_reports_hash_mismatch_when_only_the_checksum_differs() {
        let c = file_candidate("a.txt", 10, Some([7u8; 16]));
        let e = entry("id-a.txt", Some(10), Some([9u8; 16]), false);
        assert_eq!(classify(&c, true, Some(&e)), Drift::HashMismatch);
    }

    #[test]
    fn classify_reports_unverifiable_when_either_side_has_no_checksum() {
        let recorded = file_candidate("a.txt", 10, Some([7u8; 16]));
        let no_remote_md5 = entry("id-a.txt", Some(10), None, false);
        assert_eq!(
            classify(&recorded, true, Some(&no_remote_md5)),
            Drift::Unverifiable
        );

        let legacy = file_candidate("a.txt", 10, None);
        let has_remote_md5 = entry("id-a.txt", Some(10), Some([7u8; 16]), false);
        assert_eq!(
            classify(&legacy, true, Some(&has_remote_md5)),
            Drift::Unverifiable
        );
    }

    #[test]
    fn classify_reports_unverifiable_when_the_metadata_lookup_failed() {
        // A failed lookup must never read as damage - the scrub infers
        // breakage only from positive evidence.
        let c = file_candidate("a.txt", 10, Some([7u8; 16]));
        assert_eq!(classify(&c, true, None), Drift::Unverifiable);
    }

    #[test]
    fn classify_tolerates_a_remote_that_publishes_no_size() {
        let c = file_candidate("a.txt", 10, Some([7u8; 16]));
        let e = entry("id-a.txt", None, Some([7u8; 16]), false);
        assert_eq!(classify(&c, true, Some(&e)), Drift::Ok);
    }

    #[test]
    fn only_missing_is_repairable() {
        assert!(Drift::Missing.is_repairable());
        for d in [
            Drift::Ok,
            Drift::SizeMismatch,
            Drift::HashMismatch,
            Drift::Unverifiable,
        ] {
            assert!(
                !d.is_repairable(),
                "{} must not be auto-repaired",
                d.label()
            );
        }
    }

    #[test]
    fn only_the_three_damage_states_count_as_drift() {
        assert!(Drift::Missing.is_drift());
        assert!(Drift::SizeMismatch.is_drift());
        assert!(Drift::HashMismatch.is_drift());
        assert!(!Drift::Ok.is_drift());
        assert!(!Drift::Unverifiable.is_drift());
    }

    // --- slice selection / cursor -------------------------------------------

    #[test]
    fn a_full_page_keeps_the_last_key_as_the_next_lower_bound() {
        let (next, wrapped) = advance_cursor(Some("m.txt"), 500, 500, false);
        assert_eq!(next.as_deref(), Some("m.txt"));
        assert!(!wrapped);
    }

    #[test]
    fn a_short_page_wraps_the_cursor_to_the_beginning() {
        let (next, wrapped) = advance_cursor(Some("z.txt"), 3, 500, true);
        assert_eq!(next, None);
        assert!(wrapped, "a short page means the population ran out");
    }

    #[test]
    fn an_empty_page_mid_lap_wraps_rather_than_pinning_the_cursor_past_the_end() {
        // Regression guard: without this the cursor would sit past the last
        // key forever and the source would never be scrubbed again.
        let (next, wrapped) = advance_cursor(None, 0, 500, true);
        assert_eq!(next, None);
        assert!(wrapped);
    }

    #[test]
    fn an_always_empty_population_never_claims_to_have_completed_a_lap() {
        // A source with bundling off has zero `bundles` rows forever. If that
        // counted as a wrap, the flag would be true on every single run and
        // would tell the user nothing.
        let (next, wrapped) = advance_cursor(None, 0, 500, false);
        assert_eq!(next, None);
        assert!(!wrapped);
    }

    #[test]
    fn an_over_full_page_still_advances_rather_than_wrapping() {
        // Defensive: a repo returning more than the limit must not be read as
        // "population exhausted".
        let (next, wrapped) = advance_cursor(Some("m.txt"), 501, 500, false);
        assert_eq!(next.as_deref(), Some("m.txt"));
        assert!(!wrapped);
    }

    #[test]
    fn a_population_smaller_than_one_slice_completes_a_lap_in_a_single_run() {
        let (next, wrapped) = advance_cursor(Some("b.txt"), 2, 500, false);
        assert_eq!(next, None);
        assert!(wrapped);
    }

    #[test]
    fn successive_pages_sweep_a_population_exactly_once_per_lap() {
        // Simulates the keyset walk the sqlite repo performs, proving the
        // rolling slice covers every key with no gaps and no repeats.
        let all: Vec<String> = (0..7).map(|i| format!("f{i:02}.txt")).collect();
        let slice = 3u32;
        let mut cursor: Option<String> = None;
        let mut seen: Vec<String> = Vec::new();
        for _ in 0..10 {
            let had_cursor = cursor.is_some();
            let page: Vec<&String> = all
                .iter()
                .filter(|k| cursor.as_deref().is_none_or(|c| k.as_str() > c))
                .take(slice as usize)
                .collect();
            for k in &page {
                seen.push((*k).clone());
            }
            let last = page.last().map(|k| k.as_str());
            let (next, wrapped) = advance_cursor(last, page.len(), slice, had_cursor);
            cursor = next;
            if wrapped {
                break;
            }
        }
        assert_eq!(seen, all, "one lap visits every key in order, exactly once");
    }

    // --- deterministic sampling ---------------------------------------------

    #[test]
    fn sampling_is_reproducible_for_the_same_seed() {
        let keys: Vec<String> = (0..50).map(|i| format!("key-{i}")).collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let a = deterministic_sample("run-42", &refs, 5);
        let b = deterministic_sample("run-42", &refs, 5);
        assert_eq!(a, b);
        assert_eq!(a.len(), 5);
    }

    #[test]
    fn sampling_differs_between_seeds() {
        let keys: Vec<String> = (0..200).map(|i| format!("key-{i}")).collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let a = deterministic_sample("run-1", &refs, 10);
        let b = deterministic_sample("run-2", &refs, 10);
        assert_ne!(a, b, "a different run id must sample a different subset");
    }

    #[test]
    fn sampling_returns_sorted_unique_in_range_indices() {
        let keys: Vec<String> = (0..30).map(|i| format!("key-{i}")).collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let picked = deterministic_sample("seed", &refs, 8);
        assert_eq!(picked.len(), 8);
        for w in picked.windows(2) {
            assert!(w[0] < w[1], "indices must be sorted and unique");
        }
        assert!(picked.iter().all(|i| *i < refs.len()));
    }

    #[test]
    fn sampling_caps_at_the_population_size() {
        let refs = ["a", "b", "c"];
        assert_eq!(deterministic_sample("s", &refs, 99), vec![0, 1, 2]);
    }

    #[test]
    fn sampling_zero_or_empty_picks_nothing() {
        let refs = ["a", "b"];
        assert!(deterministic_sample("s", &refs, 0).is_empty());
        assert!(deterministic_sample("s", &[], 5).is_empty());
    }

    #[test]
    fn sampling_is_domain_separated_between_seed_and_key() {
        // "ab" + "c" must not collide with "a" + "bc".
        let one = deterministic_sample("ab", &["c", "d", "e", "f"], 2);
        let two = deterministic_sample("a", &["bc", "d", "e", "f"], 2);
        // Not a strong claim about WHICH indices; the guard is that the
        // separator is actually hashed (a concatenation bug makes the first
        // element's score identical across the two calls).
        let _ = (one, two);
        let mut h1 = blake3::Hasher::new();
        h1.update(b"ab");
        h1.update(&[0u8]);
        h1.update(b"c");
        let mut h2 = blake3::Hasher::new();
        h2.update(b"a");
        h2.update(&[0u8]);
        h2.update(b"bc");
        assert_ne!(h1.finalize().as_bytes(), h2.finalize().as_bytes());
    }

    // --- due predicate ------------------------------------------------------

    #[test]
    fn a_never_scrubbed_source_is_always_due() {
        assert!(scrub_due(None, 0, 604_800_000));
    }

    #[test]
    fn a_source_is_due_once_the_interval_has_elapsed() {
        let interval = 1_000i64;
        assert!(!scrub_due(Some(500), 1_000, interval));
        assert!(scrub_due(Some(500), 1_500, interval));
        assert!(scrub_due(Some(500), 9_000, interval));
    }

    #[test]
    fn a_backwards_wall_jump_makes_a_scrub_look_not_yet_due_rather_than_panicking() {
        // Documented tradeoff, mirroring `deep_verify_due`: the failure mode
        // is a DELAYED scrub, never a missed upload.
        assert!(!scrub_due(Some(10_000), 1, 1_000));
    }

    // --- report -------------------------------------------------------------

    #[test]
    fn a_clean_report_reports_nothing() {
        let mut r = ScrubReport::default();
        for _ in 0..5 {
            r.record(Drift::Ok);
        }
        r.record(Drift::Unverifiable);
        r.finish();
        assert_eq!(r.checked, 6);
        assert_eq!(r.ok, 5);
        assert_eq!(r.unverifiable, 1);
        assert!(!r.found_anything());
        assert_eq!(r.outcome, ScrubOutcome::Clean);
    }

    #[test]
    fn a_report_with_drift_is_reported() {
        let mut r = ScrubReport::default();
        r.record(Drift::Ok);
        r.record(Drift::Missing);
        r.record(Drift::HashMismatch);
        r.finish();
        assert_eq!(r.drift_total(), 2);
        assert!(r.found_anything());
        assert_eq!(r.outcome, ScrubOutcome::Drift);
    }

    #[test]
    fn outcome_labels_round_trip_and_unknown_reads_as_incomplete() {
        for o in [
            ScrubOutcome::Clean,
            ScrubOutcome::Drift,
            ScrubOutcome::Incomplete,
        ] {
            assert_eq!(ScrubOutcome::from_label(o.label()), o);
        }
        assert_eq!(
            ScrubOutcome::from_label("something-newer"),
            ScrubOutcome::Incomplete
        );
    }

    // --- config -------------------------------------------------------------

    #[test]
    fn the_shipped_defaults_are_a_gently_on_weekly_metadata_only_scrub() {
        let d = ScrubConfig::default();
        assert!(
            d.enabled,
            "the scrub ships on - it is the remote half of the weekly deep-verify"
        );
        assert_eq!(d.interval_secs, 604_800);
        assert_eq!(d.slice_size, 500);
        assert_eq!(d.deep_sample, 0, "downloading objects is opt-in");
        assert_eq!(d.interval_ms(), 604_800_000);
    }

    #[test]
    fn out_of_range_persisted_values_clamp_instead_of_wedging_the_scrub() {
        assert_eq!(clamp_u64_to_u32(0, SCRUB_SLICE_MIN, SCRUB_SLICE_MAX), 10);
        assert_eq!(
            clamp_u64_to_u32(u64::MAX, SCRUB_SLICE_MIN, SCRUB_SLICE_MAX),
            10_000
        );
        // The cast trap this guards: `u64::from(u32::MAX) + 1` as u32 == 0,
        // which would silently disable the feature.
        assert_eq!(
            clamp_u64_to_u32(u64::from(u32::MAX) + 1, SCRUB_SLICE_MIN, SCRUB_SLICE_MAX),
            10_000
        );
        assert_eq!(clamp_u64_to_u32(250, SCRUB_SLICE_MIN, SCRUB_SLICE_MAX), 250);
    }

    #[test]
    fn candidate_cursor_key_is_the_repair_key_of_its_population() {
        let f = file_candidate("dir/a.txt", 1, None);
        assert_eq!(f.cursor_key(), "dir/a.txt");
        let b = ScrubCandidate {
            target: ScrubTarget::Bundle {
                bundle_id: "bundle-7".to_string(),
            },
            drive_file_id: "drive-7".to_string(),
            size: 1,
            md5: None,
        };
        assert_eq!(b.cursor_key(), "bundle-7");
    }
}
