//! Shared core types used across the sync engine.
//!
//! Every type in this module is a contract referenced from SPEC s2 (the
//! SQLite schema), SPEC s3 (the `RemoteStore` trait), SPEC s5 (the
//! orchestrator), or SPEC s24 (the error taxonomy). Where a type mirrors a
//! schema column or a spec field, the doc comment cites the section so a
//! reader can trace it back.
//!
//! M1 phase 1 (interfaces only): types and stubs. Implementation bodies
//! land in subsequent M1 phases.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unix epoch milliseconds.
///
/// Used wherever the SPEC schema (s2) stores `INTEGER` timestamps such as
/// `created_at`, `last_synced_at`, `last_uploaded_at`, `last_verified_at`,
/// and the `pending_ops.scheduled_for` due-time. Signed so subtraction is
/// safe across the epoch and across the kind of small backwards wall jumps
/// DESIGN s18.7 explicitly tolerates.
pub type UnixMs = i64;

// -----------------------------------------------------------------------------
// Newtype IDs
// -----------------------------------------------------------------------------

macro_rules! uuid_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Generate a new random v4 id.
            pub fn new_v4() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::from_str(s).map(Self)
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }
    };
}

macro_rules! i64_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = std::num::ParseIntError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                i64::from_str(s).map(Self)
            }
        }
    };
}

uuid_newtype! {
    /// Unique id of a Google account configured in Driven (SPEC s2
    /// `accounts.id`).
    AccountId
}

uuid_newtype! {
    /// Unique id of a backup source: a `(local folder, Drive destination,
    /// account)` triple (SPEC s2 `backup_sources.id`).
    SourceId
}

uuid_newtype! {
    /// Unique id of a restore job spawned from the UI (SPEC s11.5).
    RestoreJobId
}

i64_newtype! {
    /// Activity-log row id (SPEC s2 `activity_log.id`, `INTEGER PRIMARY KEY
    /// AUTOINCREMENT`).
    ActivityId
}

i64_newtype! {
    /// Pending-op work-queue row id (SPEC s2 `pending_ops.id`, `INTEGER
    /// PRIMARY KEY AUTOINCREMENT`).
    PendingOpId
}

// -----------------------------------------------------------------------------
// RelativePath
// -----------------------------------------------------------------------------

/// A path relative to a backup source's `local_path`, in canonical form.
///
/// Invariants the constructor must enforce (validation lands in M2):
/// - Uses forward slashes `/` as the separator, never backslashes.
/// - Never starts with a leading `/`.
/// - Never contains `..` segments.
/// - Never contains the NUL byte.
/// - Is valid UTF-8.
///
/// The canonical form is portable across Windows / macOS / Linux so the
/// SQLite `file_state.relative_path` column is a stable key across
/// platforms and survives a cross-platform restore.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RelativePath(String);

impl RelativePath {
    /// Returns the underlying string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for RelativePath {
    type Error = RelativePathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        use unicode_normalization::UnicodeNormalization;

        // Reject Windows-absolute / drive-relative / UNC / device paths on
        // the RAW input, BEFORE backslash normalization. Otherwise
        // `"C:\Users\me\file.txt"` would normalize to `"C:/Users/me/..."`
        // and slip past the relative checks - a restore-path-breakout risk
        // for a backup/restore tool. Order matters: these run on the raw
        // string; the leading-`/` check runs after normalization below.
        //
        // Drive-absolute / drive-relative: a second char of `:` with an
        // ascii-alphabetic first char covers `C:\x`, `C:/x`, and bare
        // `C:x`. (A manual check; no regex dependency needed.)
        {
            let bytes = value.as_bytes();
            if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
                return Err(RelativePathError::DriveOrUnc);
            }
        }
        // UNC / device prefixes on the raw input: `\\server\share`,
        // `\\?\C:\...`, and the forward-slash spelling `//server/share`.
        if value.starts_with("\\\\") || value.starts_with("//") {
            return Err(RelativePathError::DriveOrUnc);
        }

        // Normalise Windows separators to forward slashes so the
        // canonical form is portable across platforms (the doc invariant
        // above and SPEC s2 `file_state.relative_path`).
        let s = value.replace('\\', "/");
        if s.is_empty() {
            return Err(RelativePathError::Empty);
        }
        // After backslash normalization, a UNC `\\server` becomes
        // `//server`; the raw-input guard above already rejected both
        // spellings, but re-check here so any path that normalizes to a
        // `//`-prefixed form is also refused.
        if s.starts_with("//") {
            return Err(RelativePathError::DriveOrUnc);
        }
        if s.starts_with('/') {
            return Err(RelativePathError::NotRelative);
        }
        if s.contains('\0') {
            return Err(RelativePathError::NulByte);
        }
        if s.split('/').any(|seg| seg == "..") {
            return Err(RelativePathError::ParentSegment);
        }
        // NFC-normalize so byte-distinct spellings of the same logical path
        // (NFC vs NFD unicode) collapse to one `file_state` key. SPEC s24
        // local.unicode_collision depends on this canonical form. The
        // validity checks above run on the pre-normalized string because
        // NFC never introduces `/`, `..`, NUL, or a leading separator.
        let s: String = s.nfc().collect();
        Ok(Self(s))
    }
}

impl TryFrom<&Path> for RelativePath {
    type Error = RelativePathError;

    fn try_from(value: &Path) -> Result<Self, Self::Error> {
        let s = value.to_str().ok_or(RelativePathError::NotUtf8)?;
        Self::try_from(s.to_string())
    }
}

/// Errors produced when constructing a [`RelativePath`].
#[derive(Debug, thiserror::Error)]
pub enum RelativePathError {
    /// Path is the empty string.
    #[error("path must not be empty")]
    Empty,
    /// Path is absolute or starts with a leading separator.
    #[error("path must be relative")]
    NotRelative,
    /// Path is a Windows drive-absolute / drive-relative path (e.g.
    /// `C:\x`, `C:/x`, `C:x`) or a UNC / device path (`\\server\share`,
    /// `//server/share`, `\\?\C:\...`). Rejecting these prevents a
    /// restore-path breakout on Windows (SPEC s2 `file_state.relative_path`
    /// must stay strictly relative).
    #[error("path must not be a Windows drive-absolute or UNC path")]
    DriveOrUnc,
    /// Path contains a `..` parent segment.
    #[error("path must not contain `..` segments")]
    ParentSegment,
    /// Path contains a NUL byte.
    #[error("path must not contain a NUL byte")]
    NulByte,
    /// Path is not valid UTF-8.
    #[error("path must be valid UTF-8")]
    NotUtf8,
}

// -----------------------------------------------------------------------------
// FileStateStatus
// -----------------------------------------------------------------------------

/// Status of a row in the `file_state` table (SPEC s2: TEXT column with
/// values `'synced' | 'pending' | 'corrupt' | 'locked' | 'error' |
/// 'excluded_orphan'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStateStatus {
    /// Latest local bytes are uploaded and verified.
    Synced,
    /// Awaiting upload; an entry in `pending_ops` should exist.
    Pending,
    /// Deep-verify detected a checksum mismatch.
    Corrupt,
    /// The file is locked (Windows sharing violation; see SPEC s24
    /// `local.file_locked`).
    Locked,
    /// Last attempt failed with a non-retryable error.
    Error,
    /// The file was previously backed up but a later ignore-rule change
    /// (gitignore / default / `exclude_patterns`) now EXCLUDES it. Per
    /// DESIGN s5.5 such a path is NOT trashed on Drive - the local file may
    /// still exist, only the rules changed - so it is flagged here rather
    /// than treated as a local deletion. The implicit-delete path is
    /// reserved for actual on-disk deletions. (Serialized `"excluded_orphan"`;
    /// not constructed/persisted in M2, where the scanner only surfaces the
    /// orphan set on its [`ScanResult`] for the later Activity-banner UI.)
    ExcludedOrphan,
}

// -----------------------------------------------------------------------------
// AccountState
// -----------------------------------------------------------------------------

/// Lifecycle state of an `accounts` row (SPEC s2: TEXT column with values
/// `'ok' | 'needs_reauth' | 'disabled'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    /// Normal operating state; refresh-token works.
    Ok,
    /// Refresh-token returned `invalid_grant`; user must re-consent.
    NeedsReauth,
    /// User has explicitly disabled sync for this account.
    Disabled,
}

// -----------------------------------------------------------------------------
// PauseReason
// -----------------------------------------------------------------------------

/// Reason the orchestrator is in the `Paused` state (DESIGN s5.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// User clicked "Pause" in the tray or settings UI.
    Manual,
    /// Running on battery and `skip_on_battery` is true.
    Battery,
    /// Connected to a metered network and `skip_on_metered` is true.
    Metered,
    /// No network reachability.
    Offline,
    /// A specific dependent service (Drive, OAuth, etc.) is down per the
    /// network-resilience probes (DESIGN s5.8).
    ServiceDown,
    /// Connected to a network but the `generate_204` probe fails - no Internet
    /// (DESIGN s5.8.1; SPEC s24 `net.no_internet`). Distinct from [`Offline`]
    /// so the orchestrator preserves the non-online state end-to-end
    /// (CODEX_NOTES P2-9, M4); additive per SPEC s24 (codes are add-only).
    NoInternet,
    /// A captive portal intercepted the connectivity probe; user action is
    /// required (DESIGN s5.8.1; SPEC s24 `net.captive_portal`). Kept distinct
    /// from [`Offline`] per CODEX_NOTES P2-9 (M4).
    CaptivePortal,
    /// The resolver returned no answer for a known-good domain (DESIGN s5.8.1
    /// DNS broken; SPEC s24 `net.dns_failed`). Kept distinct from [`Offline`]
    /// per CODEX_NOTES P2-9 (M4).
    DnsFailed,
    /// Outside the user's configured schedule window (V2 schedule windows,
    /// DESIGN s17). The orchestrator resumes automatically once the local
    /// clock re-enters the allowed window - no manual action required.
    Schedule,
}

// -----------------------------------------------------------------------------
// ScheduleConfig (V2 schedule windows - DESIGN s17)
// -----------------------------------------------------------------------------

/// A time-of-day + day-of-week window during which sync is allowed (V2
/// schedule windows, DESIGN s17 "only sync 23:00-06:00").
///
/// The window is expressed in the user's LOCAL wall-clock time. Like the
/// pacer's "midnight Pacific" quota boundary (see [`crate::pacer`]),
/// `driven-core` stays free of a timezone database: local time is derived
/// from a fixed [`Self::utc_offset_minutes`] the app layer captures from the
/// OS / browser. The bounded consequence is the same as the pacer's - across
/// a DST transition the window shifts by up to an hour until the app
/// re-reads the offset. This is deliberate and documented (DESIGN s17).
///
/// The predicate is a pure function of the injected [`Clock`](crate::time::Clock)
/// reading, so the orchestrator gate is deterministic under `FakeClock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleConfig {
    /// When `false` the schedule never gates (sync runs at any time). This is
    /// the V1 behaviour and the [`Default`].
    pub enabled: bool,
    /// Minutes after local midnight the allowed window opens, `0..=1439`.
    pub start_minute: u16,
    /// Minutes after local midnight the allowed window closes, `0..=1439`.
    ///
    /// - `end > start`: a same-day window `[start, end)`.
    /// - `end < start`: the window wraps past midnight (active `[start, 1440)`
    ///   and `[0, end)`).
    /// - `end == start`: the whole day is allowed (only [`Self::days`] gates).
    pub end_minute: u16,
    /// Which local days the window is active on, indexed `0 = Sunday ..=
    /// 6 = Saturday` to match JavaScript's `Date.getDay()`. The window is
    /// evaluated against the CURRENT local day, so a window that wraps past
    /// midnight (e.g. 23:00-06:00) needs both the evening day and the
    /// following morning's day enabled for the whole window to be allowed.
    pub days: [bool; 7],
    /// Minutes to ADD to UTC to reach the user's local wall-clock time
    /// (e.g. `-480` for PST = UTC-8). The app layer sets this from the OS;
    /// the browser value is `-new Date().getTimezoneOffset()`.
    pub utc_offset_minutes: i16,
}

impl Default for ScheduleConfig {
    /// Disabled: sync runs at any time (V1 behaviour). The window fields are
    /// inert while `enabled` is false.
    fn default() -> Self {
        Self {
            enabled: false,
            start_minute: 0,
            end_minute: 0,
            days: [true; 7],
            utc_offset_minutes: 0,
        }
    }
}

impl ScheduleConfig {
    /// Milliseconds per minute / minutes per day, for the local-time maths.
    const MS_PER_MIN: i64 = 60_000;
    const MINS_PER_DAY: i64 = 1_440;

    /// True if sync is allowed at the wall-clock instant `now_ms`.
    ///
    /// A disabled schedule always allows. Otherwise the UTC instant is shifted
    /// into local wall time by [`Self::utc_offset_minutes`], reduced to a
    /// local day-of-week + minute-of-day, and tested against the window. Uses
    /// Euclidean division/remainder so a negative (pre-epoch) or
    /// backwards-jumped clock reading still yields an in-range day/minute
    /// rather than a panic (DESIGN s18.7 - the clock may move backwards).
    pub fn allows(&self, now_ms: UnixMs) -> bool {
        if !self.enabled {
            return true;
        }
        let local_ms = now_ms.saturating_add((self.utc_offset_minutes as i64) * Self::MS_PER_MIN);
        let total_min = local_ms.div_euclid(Self::MS_PER_MIN);
        let min_of_day = total_min.rem_euclid(Self::MINS_PER_DAY) as u16;
        // Days since the Unix epoch in local time. 1970-01-01 was a Thursday,
        // which is `getDay() == 4`, so offset the day count by 4 before the
        // mod-7 to land on the Sunday-indexed weekday.
        let day_index = total_min.div_euclid(Self::MINS_PER_DAY);
        let dow = (day_index + 4).rem_euclid(7) as usize;
        if !self.days[dow] {
            return false;
        }
        let (s, e) = (self.start_minute, self.end_minute);
        if s == e {
            // Whole day allowed; only the day-of-week gates.
            return true;
        }
        if s < e {
            min_of_day >= s && min_of_day < e
        } else {
            // Wraps past midnight.
            min_of_day >= s || min_of_day < e
        }
    }
}

// -----------------------------------------------------------------------------
// Orchestrator state machine: OrchestratorState, ExecProgress, ErrorDetail
// -----------------------------------------------------------------------------

/// The orchestrator's coarse-grained lifecycle state (SPEC s5, DESIGN
/// s5.1 state machine).
///
/// One orchestrator runs per account (DESIGN s5.1); two accounts with work
/// run two independent state machines. The variants and the transitions
/// between them are load-bearing (SPEC s5 calls them out as fixed even
/// though the surrounding struct shape is "illustrative"); the payloads
/// carry just enough for the tray + Activity dashboard to render "is it
/// stuck or working?" (DESIGN s5.8.6) without shipping the full plan or
/// op list across the IPC boundary.
///
/// `Clone` so a snapshot can be handed to the IPC layer; `Serialize`/
/// `Deserialize` so it crosses the Tauri boundary. The richer
/// network-aware substates DESIGN s5.8.6 sketches (`Idle.NetworkOffline`,
/// `Backoff.ServiceOpen`, `Executing.Degraded`) are modelled in M3 via the
/// [`PauseReason`] / [`NetworkState`](crate::network::NetworkState) /
/// [`ServiceHealth`](crate::network::ServiceHealth) payloads rather than as
/// extra top-level variants; see the M3 phase-1 finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum OrchestratorState {
    /// No work in progress. `last_run_at` is the wall-clock ms of the last
    /// completed cycle, or `None` before the first run.
    Idle {
        /// Unix epoch ms of the last completed cycle, if any.
        last_run_at: Option<UnixMs>,
    },
    /// Checking the power / network gates (DESIGN s5.7) before starting a
    /// batch. A failed gate transitions to [`OrchestratorState::Paused`].
    PowerCheck,
    /// Walking + diffing one source's local tree (SPEC s6). `scanned` is a
    /// running count of files visited, for a live progress readout.
    Scanning {
        /// Source currently being scanned.
        source_id: SourceId,
        /// Files visited so far this scan.
        scanned: u64,
    },
    /// The planner has produced a [`Plan`]; the summary is carried for the
    /// activity-log line and the dry-run display (SPEC s5, s7).
    Planning {
        /// Counts-only digest of the plan.
        plan: PlanSummary,
    },
    /// Executing the plan's ops (SPEC s8). `progress` is updated as ops
    /// complete.
    Executing {
        /// Live execution progress.
        progress: ExecProgress,
    },
    /// Running the sample-based post-upload verification pass (SPEC s5,
    /// DESIGN s3.3). `sampled` files checked so far, `mismatches` found.
    Verifying {
        /// Files sampled so far this pass.
        sampled: u64,
        /// Checksum mismatches found so far.
        mismatches: u64,
    },
    /// Backing off after a rate-limit or circuit-breaker trip (SPEC s5,
    /// DESIGN s5.8.3). `until` is the wall-clock ms the backoff lifts.
    Backoff {
        /// Unix epoch ms at which the backoff window ends.
        until: UnixMs,
    },
    /// Sync is paused. `reason` distinguishes a user pause from a
    /// gate-driven pause (battery / metered / offline / service-down) per
    /// DESIGN s5.7.
    Paused {
        /// Why sync is paused.
        reason: PauseReason,
    },
    /// A non-recoverable error halted the cycle; surfaced via the red tray
    /// icon + an OS notification (DESIGN s5.1).
    Error {
        /// The error, in the stable IPC shape (SPEC s24).
        detail: ErrorDetail,
    },
}

/// Live progress of an [`OrchestratorState::Executing`] phase (SPEC s5).
///
/// All counters are monotonic within one execution and reset to zero at
/// the start of each plan via [`ExecProgress::zero`]. Carried in the
/// state so the tray / Activity dashboard can show "N of M files, X of Y
/// bytes" without re-reading the plan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecProgress {
    /// Upload ops completed (succeeded) so far.
    pub files_done: u64,
    /// Total upload ops in the plan (the denominator for "N of M").
    pub files_total: u64,
    /// Bytes uploaded so far (sum of completed upload sizes).
    pub bytes_done: u64,
    /// Total bytes the plan's upload ops will move.
    pub bytes_total: u64,
    /// Trash ops completed so far.
    pub trashes_done: u64,
    /// Total trash ops in the plan.
    pub trashes_total: u64,
    /// Ops that failed (non-retryable) so far this execution. A non-zero
    /// value does not by itself stop the run; the orchestrator decides
    /// fail-fast vs continue per error class.
    pub errors: u64,
}

impl ExecProgress {
    /// Returns a zeroed progress, the starting value at the head of an
    /// [`OrchestratorState::Executing`] phase (SPEC s5
    /// `ExecProgress::zero()`).
    pub fn zero() -> Self {
        Self::default()
    }
}

/// An error in the stable IPC JSON shape (SPEC s24).
///
/// This is the payload of [`OrchestratorState::Error`] and the body the
/// IPC layer serialises to `{ code, message, retry_after_ms?, details? }`.
/// The [`code`](Self::code) is the i18n key (load-bearing; see [`ErrorCode`]);
/// `message` is a human-readable fallback (the frontend prefers
/// `t('errors.${code}.short')`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    /// The stable dotted error code (SPEC s24); serialised as its string
    /// form via [`ErrorCode::code`].
    pub code: ErrorCode,
    /// Human-readable fallback message. Not localised; the frontend maps
    /// `code` to a localised string and uses `message` only as a backstop.
    pub message: String,
    /// Optional retry-after hint in milliseconds, populated for codes that
    /// carry one (e.g. `drive.rate_limited`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// Optional free-form structured detail (SPEC s24 `details`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ErrorDetail {
    /// Builds an [`ErrorDetail`] from a code and message with no
    /// retry-after or extra details.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retry_after_ms: None,
            details: None,
        }
    }
}

// -----------------------------------------------------------------------------
// ActivityEntry: the wire shape of one activity-log row (SPEC s11.4 / s11.7)
// -----------------------------------------------------------------------------

/// One activity-log entry as it crosses the IPC boundary to the webview
/// (SPEC s11.4 `query_activity` page rows, SPEC s11.7 `activity:new` payload).
///
/// This is the SINGLE wire DTO for activity: it is both the per-row element of
/// the paginated `ActivityPage` returned by `query_activity` AND the payload of
/// the live-tail `activity:new` event the orchestrator broadcasts. Defining it
/// here (in `driven-core`) - rather than only in the app-shell IPC layer - lets
/// [`OrchestratorEvent::ActivityWritten`] carry it directly, so the app shell's
/// event bridge re-emits it to the webview with no re-serialisation or shape
/// drift between the live tail and the paged history.
///
/// Wire casing is `camelCase` to match the hand-written TypeScript IPC surface
/// (DESIGN s8.6; `design/CODEX_NOTES.md` M6 "the typed-IPC surface is camelCase
/// over the wire"). It mirrors [`crate::state::ActivityRow`] field-for-field with
/// `source_id` rendered as the UUID string the webview already keys sources by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    /// Stable auto-increment row id (`activity_log.id`). The webview dedups the
    /// live tail against the paged history by this id.
    pub id: i64,
    /// Wall-time the event occurred (Unix epoch ms).
    pub ts: UnixMs,
    /// Owning source id (UUID string), or `None` for a global event.
    pub source_id: Option<String>,
    /// Severity: `info` | `warn` | `error` (the serialised
    /// [`crate::state::ActivityLevel`]).
    pub level: crate::state::ActivityLevel,
    /// Event-type discriminant (e.g. `upload_done`, `scan_done`, a SPEC s24
    /// error code). Used as the i18n lookup base for the row's label.
    pub event_type: String,
    /// File count associated with the event, if any.
    pub file_count: Option<u64>,
    /// Byte count associated with the event, if any.
    pub bytes: Option<u64>,
    /// Free-form human-readable message, if any (already redaction-safe: the
    /// orchestrator only writes paths/codes here, never secrets).
    pub message: Option<String>,
}

impl From<&crate::state::ActivityRow> for ActivityEntry {
    fn from(row: &crate::state::ActivityRow) -> Self {
        Self {
            id: row.id.0,
            ts: row.ts,
            source_id: row.source_id.map(|s| s.to_string()),
            level: row.level,
            event_type: row.event_type.clone(),
            file_count: row.file_count,
            bytes: row.bytes,
            message: row.message.clone(),
        }
    }
}

impl From<crate::state::ActivityRow> for ActivityEntry {
    fn from(row: crate::state::ActivityRow) -> Self {
        (&row).into()
    }
}

/// An event the orchestrator broadcasts to long-lived consumers
/// (the IPC event bridge, the tray, the activity-log writer) (SPEC s5
/// `events: broadcast::Sender<OrchestratorEvent>`, s11.7).
///
/// The orchestrator owns a [`tokio::sync::broadcast`] sender; consumers
/// `subscribe`. A slow consumer that lags the channel sees
/// `broadcast::error::RecvError::Lagged` and should re-read the current
/// [`OrchestratorState`] rather than assume an intermediate event is lost
/// data (the same recovery contract as [`PowerSource`](driven_power::PowerSource)).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OrchestratorEvent {
    /// The orchestrator transitioned to a new lifecycle state.
    StateChanged {
        /// The state just entered.
        state: OrchestratorState,
    },
    /// An execution-progress tick (throttled; not one per byte). Carried
    /// separately from [`OrchestratorEvent::StateChanged`] so the UI can
    /// update a progress bar without a full state re-render.
    Progress {
        /// Source the progress belongs to.
        source_id: SourceId,
        /// The latest progress snapshot.
        progress: ExecProgress,
    },
    /// A power transition observed by the orchestrator (DESIGN s5.10).
    Power {
        /// The power event.
        event: PowerEvent,
    },
    /// A network transition observed by the orchestrator (DESIGN s5.8).
    Network {
        /// The network event.
        event: crate::network::NetworkEvent,
    },
    /// An account's refresh token returned `invalid_grant`: the account was
    /// moved to [`AccountState::NeedsReauth`] and its sync cycle stopped (V-F,
    /// DESIGN s5.4 "400 invalidGrant / 401 invalidCredentials -> mark account
    /// `needs_reauth`, stop the orchestrator for this account, OS notify"). The
    /// app shell surfaces a reauth prompt + OS notification on this event
    /// (`account:needs_reauth`).
    AccountNeedsReauth {
        /// The account that needs re-consent.
        account_id: AccountId,
    },
    /// A new `activity_log` row was just written (SPEC s11.7 `activity:new`).
    ///
    /// Broadcast by the orchestrator immediately after every durable
    /// `write_activity`, so the app shell's event bridge can re-emit the
    /// carried [`ActivityEntry`] to the webview as `activity:new` for the
    /// live tail (M7 acceptance: the tail reflects an event within 500ms,
    /// event-driven - no polling). The entry carries the assigned row id so
    /// the webview can dedup it against the paged history.
    ActivityWritten {
        /// The entry just persisted (the wire shape the webview consumes).
        entry: ActivityEntry,
    },
}

// -----------------------------------------------------------------------------
// Power / sleep-wake events: PowerEvent
// -----------------------------------------------------------------------------

/// A sleep / wake transition the OS power layer surfaces to the
/// orchestrator (DESIGN s5.10).
///
/// Distinct from [`driven_power::PowerState`] (a steady-state snapshot of
/// AC / battery / metered / reachability): a [`PowerEvent`] marks the
/// *edge* of a suspend or resume so the orchestrator can run the strict
/// on-suspend / on-resume sequences in DESIGN s5.10.2 / s5.10.3 (snapshot
/// in-flight sessions on suspend; defer-30s, re-probe, refresh tokens,
/// discard stale resumable sessions on resume). Flows into the
/// orchestrator on the same channel as power-source and network events
/// (DESIGN s5.10.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerEvent {
    /// The machine is entering sleep / hibernate (`PBT_APMSUSPEND` /
    /// `NSWorkspaceWillSleepNotification` / `PrepareForSleep(true)`).
    /// Triggers the DESIGN s5.10.2 graceful-pause + session-snapshot path.
    Suspending,
    /// The machine has resumed from sleep (`PBT_APMRESUMEAUTOMATIC` /
    /// `NSWorkspaceDidWakeNotification` / `PrepareForSleep(false)`).
    /// Triggers the strict DESIGN s5.10.3 resume sequence.
    Resumed,
}

// -----------------------------------------------------------------------------
// Op + Plan
// -----------------------------------------------------------------------------

/// One unit of work the planner emits for the executor (SPEC s7).
///
/// M1 phase 1 stub: only the variants used by the M1 contract surface are
/// declared. The full variant set (resume, deep-verify, etc.) lands in M2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Hash a local file and upload it (creating or updating the remote
    /// object as appropriate). SPEC s8.
    HashThenUpload {
        /// Source the file belongs to.
        source_id: SourceId,
        /// Path relative to the source's `local_path`.
        relative_path: RelativePath,
        /// Local file size in bytes, captured pre-open.
        size: u64,
    },
    /// Trash a remote object that no longer has a local counterpart
    /// (SPEC s7). Trash is preferred over hard-delete so the user can
    /// recover from a mistaken delete via the Drive web UI.
    Trash {
        /// Source the (now-missing) file belonged to.
        source_id: SourceId,
        /// Relative path the file had before it was deleted locally.
        relative_path: RelativePath,
        /// Drive `file_id` of the remote object to trash.
        drive_file_id: String,
    },
    /// Pack many genuinely-new small files into ONE `.tar.gz` bundle object
    /// (V2 small-file bundling, issue #35). The planner emits this ONLY for
    /// files that have no existing `file_state` row (brand new), grouped and
    /// size-capped; a changed or previously-uploaded file always stays a
    /// [`Op::HashThenUpload`]. The executor reads + hashes every member,
    /// uploads a single Drive object, and atomically commits N `file_state`
    /// rows (each with `drive_file_id = NULL`) plus their bundle membership.
    UploadBundle {
        /// Source the members belong to.
        source_id: SourceId,
        /// The member files to pack (all under `source_id`).
        members: Vec<BundleMemberPlan>,
    },
}

/// One member of an [`Op::UploadBundle`] (V2 small-file bundling, issue #35):
/// the member's relative path plus the pre-open size the scanner captured. The
/// size feeds the progress denominator ([`Plan::summary`]); the executor
/// re-stats and re-hashes each member at upload time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleMemberPlan {
    /// Path relative to the source's `local_path`.
    pub relative_path: RelativePath,
    /// Local file size in bytes, captured pre-open by the scanner.
    pub size: u64,
}

/// A batched list of [`Op`] values produced by the planner (SPEC s7).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Ops in planner-emitted order. The executor is free to reorder for
    /// concurrency but must preserve happens-before semantics for ops on
    /// the same `(source_id, relative_path)`.
    pub ops: Vec<Op>,
    /// NFC `RelativePath` keys the scanner dropped because two distinct raw
    /// on-disk paths normalised onto one key (DESIGN s5.2.3, SPEC s24
    /// `local.unicode_collision`), copied verbatim from
    /// [`ScanResult::collisions`]. The planner emits no op for these (M2 just
    /// threads them through); the M3 orchestrator surfaces them as
    /// `local.unicode_collision` activity errors and decides fail-closed
    /// (block the source) vs skip-the-colliding-file-with-an-error policy.
    pub collisions: Vec<RelativePath>,
}

impl Plan {
    /// Tallies the plan into a [`PlanSummary`] for activity logging and the
    /// orchestrator's `Planning { plan: PlanSummary }` state (SPEC s5).
    ///
    /// `bytes` counts only [`Op::HashThenUpload`] sizes - trashes move no
    /// bytes. This is a pure fold over `ops`, not sync behaviour.
    pub fn summary(&self) -> PlanSummary {
        let mut summary = PlanSummary::default();
        for op in &self.ops {
            match op {
                Op::HashThenUpload { size, .. } => {
                    summary.uploads += 1;
                    summary.bytes += *size;
                }
                Op::Trash { .. } => summary.trashes += 1,
                // A bundle uploads ONE object but represents N member files: count
                // each member so the progress "N of M files" denominator reflects
                // real files, and sum their sizes for the byte total.
                Op::UploadBundle { members, .. } => {
                    summary.uploads += members.len();
                    summary.bytes += members.iter().map(|m| m.size).sum::<u64>();
                }
            }
        }
        summary
    }
}

/// A counts-only digest of a [`Plan`] (SPEC s5
/// `OrchestratorState::Planning { plan: PlanSummary }`).
///
/// Used for the activity-log "scan_done"/dry-run summary line and the
/// orchestrator state without carrying the full op list across the IPC
/// boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    /// Number of [`Op::HashThenUpload`] ops in the plan.
    pub uploads: usize,
    /// Number of [`Op::Trash`] ops in the plan.
    pub trashes: usize,
    /// Total bytes the upload ops will move (sum of their `size` fields).
    /// Trashes contribute nothing.
    pub bytes: u64,
}

// -----------------------------------------------------------------------------
// Scanner surface: ScanResult, LocalEntry, ScanMode, SymlinkPolicy
// -----------------------------------------------------------------------------

/// The diff a single scan of one source produces (SPEC s6).
///
/// The scanner walks the local tree, compares each file against the
/// `file_state` rows (SPEC s2) loaded for the source, and emits the set of
/// files that need uploading plus the set whose `file_state` rows have no
/// surviving local file. The planner (SPEC s7) turns this into a [`Plan`].
///
/// Paths are [`RelativePath`] (NFC-canonical, the `file_state` primary-key
/// form per DESIGN s5.2.3), not raw `PathBuf` - the SPEC s6 pseudocode
/// uses `PathBuf` only illustratively.
///
/// Note: this shape does NOT carry the walk-error / partial-success signal
/// DESIGN s5.2 step 3 requires to gate safe deletion (a permission denial
/// under a subtree must never cascade into trashing everything under it).
/// That signal's channel is unresolved at this layer - see the M2 phase-1
/// finding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanResult {
    /// Files that are new or whose `(size, mtime_ns)` (or, under
    /// [`ScanMode::DeepVerify`], whose hash) differs from the stored
    /// `file_state` row. Each becomes one [`Op::HashThenUpload`].
    pub new_or_changed: Vec<LocalEntry>,
    /// Relative paths present in `file_state` but no longer on disk. The
    /// planner trashes the ones that reached Drive and drops the rest
    /// (SPEC s7).
    pub deleted: Vec<RelativePath>,
    /// NFC `RelativePath` keys that two or more distinct raw on-disk paths
    /// normalised onto (DESIGN s5.2.3, SPEC s24 `local.unicode_collision`).
    /// The scanner keeps the first-seen file for each colliding key and
    /// drops the later one(s) rather than emitting a duplicate upload op;
    /// each dropped key is recorded here so the UI can surface the
    /// collision instead of silently losing a file. One entry per
    /// additional collider (the first occurrence is not recorded).
    pub collisions: Vec<RelativePath>,
    /// Paths present in `file_state` (so previously backed up) that the walk
    /// did NOT yield because the CURRENT ignore rules now exclude them - an
    /// ignore-rule change, not a local deletion (DESIGN s5.5). These are
    /// split out of `deleted` precisely so the planner emits NO trash for
    /// them: the local file may still be on disk, only the rules changed, and
    /// trashing the Drive copy on a pure config change would be data loss.
    /// At M2 this is surfaced for the later Activity-banner UI; the planner
    /// no-ops on it.
    pub excluded_orphans: Vec<RelativePath>,
    /// Paths whose file carried one or more NTFS Alternate Data Streams that
    /// Driven backs up the main (unnamed `::$DATA`) stream of but NOT the
    /// named streams (DESIGN s5.2.1 / STRESS_HARNESS s3.5
    /// `ads-alternate-data-stream`, SPEC s24 `local.ads_skipped`). The main
    /// stream still uploads; this is a one-notice-per-affected-file warning,
    /// never a trash and never a fatal - the named stream is silently dropped
    /// today, so surfacing it is what stops that being silent data loss. Only
    /// ever populated on Windows + NTFS; empty everywhere else.
    pub ads_skipped: Vec<RelativePath>,
    /// Lossy display strings of local paths the scanner skipped because they
    /// are not representable as a `RelativePath` (e.g. a Win32 name with an
    /// unpaired UTF-16 surrogate that fails UTF-8 conversion, STRESS_HARNESS
    /// s3.4 `name-unpaired-surrogate`, SPEC s24 `local.invalid_filename`). The
    /// file is NOT backed up; surfacing the skip as a one-notice-per-path
    /// warning is what keeps it from being a silent omission. Stored as a lossy
    /// `String` because there is, by definition, no valid `RelativePath` for it.
    pub invalid_filenames: Vec<String>,
}

/// One local file the scanner observed (SPEC s6 `LocalEntry`).
///
/// Carries exactly the cheap stat fields the fast-path diff compares
/// against the `file_state` row (DESIGN s5.2 step 2); the BLAKE3 hash is
/// computed later by the executor's `HashThenUpload`, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEntry {
    /// Path under the source's `local_path`, NFC-canonical.
    pub rel: RelativePath,
    /// File size in bytes from the entry's metadata.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch. Signed to
    /// match `file_state.mtime_ns` (SPEC s2) and tolerate pre-epoch mtimes.
    pub mtime_ns: i64,
}

/// How aggressively a scan decides a file changed (DESIGN s3.3, s5.2).
///
/// The two modes differ only in the change-detection predicate; both emit
/// the same [`ScanResult`] shape, so a [`ScanMode::DeepVerify`] hit lands
/// in `new_or_changed` exactly like an mtime/size change and produces one
/// [`Op::HashThenUpload`] (ROADMAP M2 "deep-verify catches bit-rot" row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    /// Default per-tick scan: a file is changed iff its `(size, mtime_ns)`
    /// differs from the stored `file_state` row. Cheap; never reads
    /// content (DESIGN s5.2 step 2 fast path).
    FastPath,
    /// Periodic re-verification (default weekly per
    /// `deep_verify_interval_secs`): re-hash every file regardless of
    /// `(size, mtime_ns)` and treat a hash mismatch against the stored
    /// `hash_blake3` as a change. Catches silent bit-rot and filesystem
    /// timestamp lies (DESIGN s3.3, s5.2 step 4).
    DeepVerify,
}

/// What the scanner does when it meets a symbolic link (DESIGN s5.2.1).
///
/// V1 ships only [`SymlinkPolicy::Skip`]: the link is not followed and the
/// link itself is not backed up, because following can walk out of the
/// configured source, can loop, and is almost never what the user
/// intended. A per-source "follow symlinks" toggle (the `Follow` variant)
/// is V2 - omitted here so V1 code can never accidentally follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymlinkPolicy {
    /// Do not follow symlinks; do not back up the link itself. V1 default
    /// and only option (DESIGN s5.2.1).
    #[default]
    Skip,
}

// -----------------------------------------------------------------------------
// ErrorCode
// -----------------------------------------------------------------------------

/// Stable dotted error codes surfaced across the IPC boundary (SPEC s24).
///
/// Codes are load-bearing for i18n: they are translation-bundle keys, so
/// they must never change between minor versions. New codes may be added;
/// existing codes may be deprecated but stay translatable for at least one
/// major release.
///
/// [`Display`] and [`ErrorCode::code`] both return the dotted string form
/// (e.g. `"drive.rate_limited"`); the human-readable meanings live only
/// in doc comments below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// `auth.invalid_grant` - refresh token revoked; reauth required.
    AuthInvalidGrant,
    /// `auth.consent_required` - first-time auth or scope change.
    AuthConsentRequired,
    /// `auth.network_unreachable` - couldn't reach accounts.google.com.
    AuthNetworkUnreachable,
    /// `drive.rate_limited` - 429 / userRateLimitExceeded.
    DriveRateLimited,
    /// `drive.daily_quota_exhausted` - 403 dailyLimitExceeded, paused
    /// until reset.
    DriveDailyQuotaExhausted,
    /// `drive.quota_exhausted` - 403 storageQuotaExceeded (user's Drive
    /// is full).
    DriveQuotaExhausted,
    /// `drive.upload_size_limit` - file exceeds Drive's per-file size
    /// limit.
    DriveUploadSizeLimit,
    /// `drive.checksum_mismatch` - verification failed after upload.
    DriveChecksumMismatch,
    /// `drive.unreachable` - Drive API down, unreachable, or 5xx
    /// circuit-open.
    DriveUnreachable,
    /// `drive.resumable_session_invalid` - 4xx during resumable upload;
    /// caller must restart the session.
    DriveResumableSessionInvalid,
    /// `drive.dest_folder_missing` - the configured destination folder
    /// was deleted from Drive by the user.
    DriveDestFolderMissing,
    /// `drive.dest_folder_permission_denied` - destination folder's
    /// sharing changed to read-only for this account.
    DriveDestFolderPermissionDenied,
    /// `local.file_locked` - couldn't open even with `FILE_SHARE_DELETE`
    /// (V1: locked file, VSS path failed too).
    LocalFileLocked,
    /// `local.vss_unavailable` - Driven needs elevation to use VSS but
    /// isn't elevated.
    LocalVssUnavailable,
    /// `local.file_changed_during_upload` - pre/post fstat showed file
    /// mutated mid-upload; re-queued.
    LocalFileChangedDuringUpload,
    /// `local.file_replaced_during_upload` - atomic-replace detected by
    /// inode identity check; re-queued.
    LocalFileReplacedDuringUpload,
    /// `local.io_error` - generic disk error.
    LocalIoError,
    /// `local.path_too_long` - OS path-length limit hit.
    LocalPathTooLong,
    /// `local.unicode_collision` - two distinct paths normalise to the
    /// same NFC string.
    LocalUnicodeCollision,
    /// `local.disk_full` - source filesystem out of space during a
    /// verify-style read or restore write.
    LocalDiskFull,
    /// `local.invalid_filename` - a name the local OS allowed but Drive
    /// will reject (reserved name, trailing dot/space, etc.).
    LocalInvalidFilename,
    /// `local.ads_skipped` - NTFS Alternate Data Stream encountered; main
    /// stream backed up, ADS skipped.
    LocalAdsSkipped,
    /// `net.offline` - OS reports no network connectivity.
    NetOffline,
    /// `net.no_internet` - connected but generate-204 probe fails.
    NetNoInternet,
    /// `net.dns_failed` - resolver returned no answer for a known-good
    /// domain.
    NetDnsFailed,
    /// `net.captive_portal` - captive portal detected; user action
    /// required.
    NetCaptivePortal,
    /// `net.timeout` - request exceeded its configured timeout.
    NetTimeout,
    /// `net.intermittent` - circuit-breaker tripped after N failures.
    NetIntermittent,
    /// `net.proxy_required` - 407 from HTTP proxy, proxy auth needed.
    NetProxyRequired,
    /// `update.endpoint_unreachable` - driven.maxhogan.dev/updates
    /// unreachable.
    UpdateEndpointUnreachable,
    /// `update.signature_invalid` - Tauri updater signature verification
    /// failed.
    UpdateSignatureInvalid,
    /// `update.manual_required_macos` - the in-app updater is disabled on macOS
    /// (unsigned V1: the in-app updater is unreliable there - SPEC s15 / DESIGN
    /// s9.4), so an install was short-circuited. The UI must direct the user to
    /// download + reinstall the latest DMG manually instead of installing in
    /// place. R8-P1-2.
    UpdateManualRequiredMacos,
    /// `crypto.key_missing` - keychain entry not found.
    CryptoKeyMissing,
    /// `crypto.decrypt_failed` - AEAD verification failed.
    CryptoDecryptFailed,
    /// `crypto.recovery_phrase_invalid` - BIP39 input failed checksum.
    CryptoRecoveryPhraseInvalid,
    /// `state.db_locked` - SQLite locked (transient).
    StateDbLocked,
    /// `state.db_corrupt` - SQLite integrity_check failed; rebuild from
    /// Drive backup advised.
    StateDbCorrupt,
    /// `state.reconcile_orphan` - startup found a remote object without a
    /// local row; adopted or cleaned.
    StateReconcileOrphan,
    /// `harness.timeout` - a stress-harness scenario exceeded its budget
    /// (chaos crate only).
    HarnessTimeout,
    /// `internal.bug` - programming error; please report.
    InternalBug,
    /// `internal.invalid_input` - a value the (untrusted) webview / renderer
    /// submitted failed backend validation (an out-of-range setting, an invalid
    /// enum, or an invalid / over-count / over-length glob pattern). Distinct
    /// from [`Self::InternalBug`]: this is bad INPUT crossing the IPC boundary,
    /// not an internal programming error, so the UI shows a "check your input"
    /// message rather than "please report a bug".
    InvalidInput,
}

impl ErrorCode {
    /// Returns the stable dotted code string used as the i18n key and as
    /// the JSON `code` field at the IPC boundary (SPEC s24).
    pub fn code(self) -> &'static str {
        match self {
            ErrorCode::AuthInvalidGrant => "auth.invalid_grant",
            ErrorCode::AuthConsentRequired => "auth.consent_required",
            ErrorCode::AuthNetworkUnreachable => "auth.network_unreachable",
            ErrorCode::DriveRateLimited => "drive.rate_limited",
            ErrorCode::DriveDailyQuotaExhausted => "drive.daily_quota_exhausted",
            ErrorCode::DriveQuotaExhausted => "drive.quota_exhausted",
            ErrorCode::DriveUploadSizeLimit => "drive.upload_size_limit",
            ErrorCode::DriveChecksumMismatch => "drive.checksum_mismatch",
            ErrorCode::DriveUnreachable => "drive.unreachable",
            ErrorCode::DriveResumableSessionInvalid => "drive.resumable_session_invalid",
            ErrorCode::DriveDestFolderMissing => "drive.dest_folder_missing",
            ErrorCode::DriveDestFolderPermissionDenied => "drive.dest_folder_permission_denied",
            ErrorCode::LocalFileLocked => "local.file_locked",
            ErrorCode::LocalVssUnavailable => "local.vss_unavailable",
            ErrorCode::LocalFileChangedDuringUpload => "local.file_changed_during_upload",
            ErrorCode::LocalFileReplacedDuringUpload => "local.file_replaced_during_upload",
            ErrorCode::LocalIoError => "local.io_error",
            ErrorCode::LocalPathTooLong => "local.path_too_long",
            ErrorCode::LocalUnicodeCollision => "local.unicode_collision",
            ErrorCode::LocalDiskFull => "local.disk_full",
            ErrorCode::LocalInvalidFilename => "local.invalid_filename",
            ErrorCode::LocalAdsSkipped => "local.ads_skipped",
            ErrorCode::NetOffline => "net.offline",
            ErrorCode::NetNoInternet => "net.no_internet",
            ErrorCode::NetDnsFailed => "net.dns_failed",
            ErrorCode::NetCaptivePortal => "net.captive_portal",
            ErrorCode::NetTimeout => "net.timeout",
            ErrorCode::NetIntermittent => "net.intermittent",
            ErrorCode::NetProxyRequired => "net.proxy_required",
            ErrorCode::UpdateEndpointUnreachable => "update.endpoint_unreachable",
            ErrorCode::UpdateSignatureInvalid => "update.signature_invalid",
            ErrorCode::UpdateManualRequiredMacos => "update.manual_required_macos",
            ErrorCode::CryptoKeyMissing => "crypto.key_missing",
            ErrorCode::CryptoDecryptFailed => "crypto.decrypt_failed",
            ErrorCode::CryptoRecoveryPhraseInvalid => "crypto.recovery_phrase_invalid",
            ErrorCode::StateDbLocked => "state.db_locked",
            ErrorCode::StateDbCorrupt => "state.db_corrupt",
            ErrorCode::StateReconcileOrphan => "state.reconcile_orphan",
            ErrorCode::HarnessTimeout => "harness.timeout",
            ErrorCode::InternalBug => "internal.bug",
            ErrorCode::InvalidInput => "internal.invalid_input",
        }
    }

    /// Parses a dotted code string back into an [`ErrorCode`] (the inverse
    /// of [`ErrorCode::code`]). Returns `None` for an unknown code so a
    /// forward-compatible reader can degrade gracefully rather than panic
    /// (SPEC s24: codes may be added across versions).
    pub fn from_code(s: &str) -> Option<Self> {
        // Exhaustive over the variant set; kept paired with `code()` so a
        // new variant fails to compile here until it is added.
        Some(match s {
            "auth.invalid_grant" => ErrorCode::AuthInvalidGrant,
            "auth.consent_required" => ErrorCode::AuthConsentRequired,
            "auth.network_unreachable" => ErrorCode::AuthNetworkUnreachable,
            "drive.rate_limited" => ErrorCode::DriveRateLimited,
            "drive.daily_quota_exhausted" => ErrorCode::DriveDailyQuotaExhausted,
            "drive.quota_exhausted" => ErrorCode::DriveQuotaExhausted,
            "drive.upload_size_limit" => ErrorCode::DriveUploadSizeLimit,
            "drive.checksum_mismatch" => ErrorCode::DriveChecksumMismatch,
            "drive.unreachable" => ErrorCode::DriveUnreachable,
            "drive.resumable_session_invalid" => ErrorCode::DriveResumableSessionInvalid,
            "drive.dest_folder_missing" => ErrorCode::DriveDestFolderMissing,
            "drive.dest_folder_permission_denied" => ErrorCode::DriveDestFolderPermissionDenied,
            "local.file_locked" => ErrorCode::LocalFileLocked,
            "local.vss_unavailable" => ErrorCode::LocalVssUnavailable,
            "local.file_changed_during_upload" => ErrorCode::LocalFileChangedDuringUpload,
            "local.file_replaced_during_upload" => ErrorCode::LocalFileReplacedDuringUpload,
            "local.io_error" => ErrorCode::LocalIoError,
            "local.path_too_long" => ErrorCode::LocalPathTooLong,
            "local.unicode_collision" => ErrorCode::LocalUnicodeCollision,
            "local.disk_full" => ErrorCode::LocalDiskFull,
            "local.invalid_filename" => ErrorCode::LocalInvalidFilename,
            "local.ads_skipped" => ErrorCode::LocalAdsSkipped,
            "net.offline" => ErrorCode::NetOffline,
            "net.no_internet" => ErrorCode::NetNoInternet,
            "net.dns_failed" => ErrorCode::NetDnsFailed,
            "net.captive_portal" => ErrorCode::NetCaptivePortal,
            "net.timeout" => ErrorCode::NetTimeout,
            "net.intermittent" => ErrorCode::NetIntermittent,
            "net.proxy_required" => ErrorCode::NetProxyRequired,
            "update.endpoint_unreachable" => ErrorCode::UpdateEndpointUnreachable,
            "update.signature_invalid" => ErrorCode::UpdateSignatureInvalid,
            "update.manual_required_macos" => ErrorCode::UpdateManualRequiredMacos,
            "crypto.key_missing" => ErrorCode::CryptoKeyMissing,
            "crypto.decrypt_failed" => ErrorCode::CryptoDecryptFailed,
            "crypto.recovery_phrase_invalid" => ErrorCode::CryptoRecoveryPhraseInvalid,
            "state.db_locked" => ErrorCode::StateDbLocked,
            "state.db_corrupt" => ErrorCode::StateDbCorrupt,
            "state.reconcile_orphan" => ErrorCode::StateReconcileOrphan,
            "harness.timeout" => ErrorCode::HarnessTimeout,
            "internal.bug" => ErrorCode::InternalBug,
            "internal.invalid_input" => ErrorCode::InvalidInput,
            _ => return None,
        })
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl Serialize for ErrorCode {
    /// Serialises as the stable dotted code string (SPEC s24), so the IPC
    /// `code` field and any persisted activity row carry the i18n key
    /// rather than a Rust variant name.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    /// Deserialises from the dotted code string. An unknown code is a hard
    /// error here (the value crossed our own IPC boundary); use
    /// [`ErrorCode::from_code`] directly for lenient forward-compat reads.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        ErrorCode::from_code(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown error code: {s}")))
    }
}

impl FromStr for ErrorCode {
    type Err = UnknownErrorCode;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ErrorCode::from_code(s).ok_or_else(|| UnknownErrorCode(s.to_string()))
    }
}

/// Error returned when [`ErrorCode::from_str`] is given an unrecognised
/// dotted code.
#[derive(Debug, thiserror::Error)]
#[error("unknown error code: {0}")]
pub struct UnknownErrorCode(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_accepts_canonical_forms() {
        for s in ["a/b.txt", "deeply/nested/file", "file.txt"] {
            let rp = RelativePath::try_from(s.to_string()).expect("happy");
            assert_eq!(rp.as_str(), s);
        }
    }

    #[test]
    fn relative_path_normalises_backslashes() {
        let rp = RelativePath::try_from(r"a\b\c.txt".to_string()).unwrap();
        assert_eq!(rp.as_str(), "a/b/c.txt");
    }

    #[test]
    fn relative_path_rejects_empty() {
        assert!(matches!(
            RelativePath::try_from(String::new()),
            Err(RelativePathError::Empty)
        ));
    }

    #[test]
    fn relative_path_rejects_absolute() {
        assert!(matches!(
            RelativePath::try_from("/etc/passwd".to_string()),
            Err(RelativePathError::NotRelative)
        ));
    }

    #[test]
    fn relative_path_rejects_parent_segment() {
        assert!(matches!(
            RelativePath::try_from("a/../b".to_string()),
            Err(RelativePathError::ParentSegment)
        ));
        assert!(matches!(
            RelativePath::try_from("..".to_string()),
            Err(RelativePathError::ParentSegment)
        ));
        // A leading "." is fine; a segment that just contains ".." is not.
        assert!(RelativePath::try_from("a/..b/c".to_string()).is_ok());
    }

    #[test]
    fn relative_path_rejects_windows_absolute_and_unc() {
        // Drive-absolute / drive-relative (checked on raw input, BEFORE
        // backslash normalization would mask `C:\` as `C:/`).
        for s in [r"C:\Users\me", "C:/Users/me", "C:x", r"d:\file"] {
            assert!(
                matches!(
                    RelativePath::try_from(s.to_string()),
                    Err(RelativePathError::DriveOrUnc)
                ),
                "expected DriveOrUnc for {s:?}"
            );
        }
        // UNC / device prefixes in both spellings.
        for s in [r"\\server\share\f", "//server/share/f"] {
            assert!(
                matches!(
                    RelativePath::try_from(s.to_string()),
                    Err(RelativePathError::DriveOrUnc)
                ),
                "expected DriveOrUnc for {s:?}"
            );
        }
        // Ordinary relative paths still accepted.
        for s in ["a/b.txt", "deep/nested/file"] {
            assert!(
                RelativePath::try_from(s.to_string()).is_ok(),
                "expected Ok for {s:?}"
            );
        }
    }

    #[test]
    fn relative_path_rejects_nul_byte() {
        assert!(matches!(
            RelativePath::try_from("a\0b".to_string()),
            Err(RelativePathError::NulByte)
        ));
    }

    #[test]
    fn relative_path_from_path_round_trips() {
        let rp: RelativePath = std::path::Path::new("a/b.txt").try_into().unwrap();
        assert_eq!(rp.as_str(), "a/b.txt");
    }

    // --- ScheduleConfig (V2 schedule windows) -------------------------------

    /// Monday 2024-01-01 00:00:00 UTC, in epoch ms. The dow formula resolves
    /// this to `getDay() == 1` (Monday); used as the anchor for the cases
    /// below (offsets in minutes/days are added on top).
    const MON_2024_01_01_UTC_MS: UnixMs = 1_704_067_200_000;
    const MIN_MS: UnixMs = 60_000;
    const DAY_MS: UnixMs = 1_440 * MIN_MS;

    fn all_days() -> [bool; 7] {
        [true; 7]
    }

    #[test]
    fn schedule_disabled_always_allows() {
        let s = ScheduleConfig::default();
        assert!(!s.enabled);
        assert!(s.allows(MON_2024_01_01_UTC_MS));
        assert!(s.allows(0));
        assert!(s.allows(-1)); // pre-epoch must not panic
    }

    #[test]
    fn schedule_same_day_window_half_open() {
        // 09:00-17:00 every day.
        let s = ScheduleConfig {
            enabled: true,
            start_minute: 9 * 60,
            end_minute: 17 * 60,
            days: all_days(),
            utc_offset_minutes: 0,
        };
        let at = |min: i64| s.allows(MON_2024_01_01_UTC_MS + min * MIN_MS);
        assert!(!at(0)); // 00:00 - before
        assert!(!at(8 * 60 + 59)); // 08:59 - before
        assert!(at(9 * 60)); // 09:00 - open (inclusive)
        assert!(at(16 * 60 + 59)); // 16:59 - inside
        assert!(!at(17 * 60)); // 17:00 - close (exclusive)
        assert!(!at(23 * 60)); // 23:00 - after
    }

    #[test]
    fn schedule_wrap_past_midnight() {
        // 23:00-06:00 every day.
        let s = ScheduleConfig {
            enabled: true,
            start_minute: 23 * 60,
            end_minute: 6 * 60,
            days: all_days(),
            utc_offset_minutes: 0,
        };
        let at = |min: i64| s.allows(MON_2024_01_01_UTC_MS + min * MIN_MS);
        assert!(at(23 * 60)); // 23:00 - open
        assert!(at(23 * 60 + 30)); // 23:30 - evening tail
        assert!(at(0)); // 00:00 - past midnight
        assert!(at(5 * 60 + 59)); // 05:59 - morning
        assert!(!at(6 * 60)); // 06:00 - close (exclusive)
        assert!(!at(12 * 60)); // noon - outside
    }

    #[test]
    fn schedule_equal_bounds_is_whole_day() {
        // start == end => only the day-of-week gates.
        let s = ScheduleConfig {
            enabled: true,
            start_minute: 0,
            end_minute: 0,
            days: all_days(),
            utc_offset_minutes: 0,
        };
        for h in [0, 6, 12, 18, 23] {
            assert!(s.allows(MON_2024_01_01_UTC_MS + h * 60 * MIN_MS));
        }
    }

    #[test]
    fn schedule_day_of_week_gates() {
        // Whole-day window, but only Monday (index 1) enabled.
        let mut days = [false; 7];
        days[1] = true; // Monday
        let s = ScheduleConfig {
            enabled: true,
            start_minute: 0,
            end_minute: 0,
            days,
            utc_offset_minutes: 0,
        };
        assert!(s.allows(MON_2024_01_01_UTC_MS)); // Monday
        assert!(!s.allows(MON_2024_01_01_UTC_MS + DAY_MS)); // Tuesday
        assert!(!s.allows(MON_2024_01_01_UTC_MS - DAY_MS)); // Sunday
        assert!(s.allows(MON_2024_01_01_UTC_MS + 7 * DAY_MS)); // next Monday
    }

    #[test]
    fn schedule_utc_offset_shifts_local_time() {
        // 00:00-01:00 LOCAL, every day, at UTC+1. 00:00 UTC == 01:00 local,
        // which is outside [00:00, 01:00); one hour earlier (23:00 UTC) ==
        // 00:00 local, which is inside.
        let s = ScheduleConfig {
            enabled: true,
            start_minute: 0,
            end_minute: 60,
            days: all_days(),
            utc_offset_minutes: 60,
        };
        assert!(!s.allows(MON_2024_01_01_UTC_MS)); // 01:00 local
        assert!(s.allows(MON_2024_01_01_UTC_MS - 60 * MIN_MS)); // 00:00 local
    }
}
