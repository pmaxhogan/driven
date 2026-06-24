//! Activity IPC commands (SPEC s11.4): `query_activity`, `clear_activity_older_than`.
//!
//! These back the Activity dashboard (DESIGN s8.3): a paginated, filtered view
//! over the `activity_log` table plus a retention-prune command. The live tail
//! (DESIGN s8.3 "last 1000 events") is event-driven - the orchestrator
//! broadcasts [`OrchestratorEvent::ActivityWritten`](driven_core::types::OrchestratorEvent::ActivityWritten)
//! on every durable activity write and the app shell's event bridge re-emits it
//! as `activity:new` (SPEC s11.7) - so there is NO polling command for the tail;
//! `query_activity` serves only the persisted history + filter re-queries.
//!
//! IPC input safety (SPEC s11.6.1): `query_activity` takes ONLY scalar filters +
//! paging from the (untrusted) webview - no raw paths. The command body
//! validates and BOUNDS every input before the query: the page `limit` is capped
//! to [`MAX_ACTIVITY_PAGE_LIMIT`], `min_level` must be a known enum value, the
//! `source_id` must parse as a UUID, and the `event_types` IN-list is bounded in
//! both count and per-entry length so a hostile renderer cannot build a
//! pathological query.

use tauri::State;

use driven_core::state::{ActivityFilter, ActivityLevel, PageRequest};
use driven_core::time::{Clock, SystemClock};
use driven_core::types::{ActivityEntry, ErrorCode, FileStateStatus, SourceId};

use crate::app_state::AppState;
use crate::commands::dtos::{
    ActivityFilterDto, ActivityPageDto, ActivitySummaryDto, FileStatusCountDto, PageRequestDto,
};
use crate::commands::{CommandError, CommandResult};

/// Tracing target for the activity command layer.
const TARGET: &str = "driven::app::activity";

/// Max rows the webview may request per `query_activity` page (SPEC s11.6.1
/// bound). The Activity dashboard pages history in chunks of ~100; this cap
/// keeps a single round-trip bounded (and well under the StateRepo's own
/// `1..=10_000` guard) so a hostile / buggy renderer cannot ask for an
/// unbounded page. The dashboard's "scroll back 1000+ events without re-query"
/// acceptance is met by ACCUMULATING pages client-side, not by one giant page.
pub const MAX_ACTIVITY_PAGE_LIMIT: u32 = 1_000;

/// Max number of `event_types` discriminants the IN-list filter may carry
/// (SPEC s11.6.1 bound). There are only a couple dozen real event types; this
/// caps the JSON IN-list the query builds.
const MAX_EVENT_TYPE_FILTERS: usize = 64;

/// Max length (bytes) of a single `event_types` discriminant (SPEC s11.6.1
/// bound). Real codes are dotted identifiers well under this.
const MAX_EVENT_TYPE_LEN: usize = 128;

/// Hard cap on rows a single `clear_activity_older_than` call may delete, so a
/// runaway prune can never hold the write transaction for an unbounded time
/// (the StateRepo batches internally; this is the cumulative ceiling).
const CLEAR_ACTIVITY_HARD_CAP: u64 = 5_000_000;

/// `query_activity(filter, page)` - a paginated, filtered page of the
/// `activity_log` (SPEC s11.4).
///
/// Returns newest-first rows for the requested zero-based `page` plus the total
/// match count and a derived `has_more`, so the webview accumulates pages
/// client-side (M7 acceptance: scroll back through 1000+ events without
/// re-querying earlier pages). Re-querying with a changed filter resets the
/// accumulation on the frontend.
///
/// SPEC s11.6.1: every webview-supplied filter value is validated + bounded by
/// [`validate_filter`] / [`validate_page`] BEFORE the query.
#[tauri::command]
pub async fn query_activity(
    state: State<'_, AppState>,
    filter: ActivityFilterDto,
    page: PageRequestDto,
) -> CommandResult<ActivityPageDto> {
    let core_filter = validate_filter(filter)?;
    let core_page = validate_page(page)?;

    let result = state
        .state()
        .query_activity(core_filter, core_page)
        .await
        .map_err(CommandError::from)?;

    let entries: Vec<ActivityEntry> = result.rows.iter().map(ActivityEntry::from).collect();
    // `has_more` is true when rows AFTER this page still match: the count
    // consumed up to and including this page is `(page + 1) * limit`; more
    // remain when `total` exceeds that. Computed with saturating/checked
    // arithmetic so a large page index can never panic or wrap.
    let consumed = (core_page.page as u64)
        .saturating_add(1)
        .saturating_mul(core_page.limit as u64);
    let has_more = result.total > consumed;

    tracing::debug!(
        target: TARGET,
        page = core_page.page,
        limit = core_page.limit,
        returned = entries.len(),
        total = result.total,
        has_more,
        "query_activity page served"
    );

    Ok(ActivityPageDto {
        entries,
        total: result.total,
        page: core_page.page,
        limit: core_page.limit,
        has_more,
    })
}

/// `clear_activity_older_than(before_ts)` - prune `activity_log` rows older than
/// `before_ts` (Unix ms), returning the number deleted (SPEC s11.4).
///
/// Backs the retention controls (DESIGN s8.3 "last 30 days minimum"); the
/// StateRepo batches the delete internally and checkpoints the WAL afterwards,
/// so this is safe to call on a large table. `before_ts` is a scalar timestamp -
/// no path validation applies; it is passed through to the batched prune with a
/// hard ceiling so a single call can never run unbounded.
#[tauri::command]
pub async fn clear_activity_older_than(
    state: State<'_, AppState>,
    before_ts: i64,
) -> CommandResult<u64> {
    let deleted = state
        .state()
        .prune_activity_older_than(before_ts, CLEAR_ACTIVITY_HARD_CAP, None)
        .await
        .map_err(CommandError::from)?;
    tracing::debug!(target: TARGET, before_ts, deleted, "clear_activity_older_than");
    Ok(deleted)
}

/// Max throughput window the webview may request for `activity_summary`
/// (SPEC s11.6.1 bound): 24h in ms. A larger window is rejected so the rate
/// denominator stays sane and the byte sum stays bounded to a day.
const MAX_THROUGHPUT_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;

/// `distinct_activity_event_types()` - the DISTINCT set of `event_type` values
/// in the durable `activity_log`, sorted ascending (M7-P2-4).
///
/// Backs the Activity dashboard's event-type filter dropdown so the user can
/// filter for a type present in history but not in the currently-loaded rows
/// (the loaded-rows-only derivation made the backend event-type filter
/// unreachable for older types). Read-only scalar query - no path validation.
#[tauri::command]
pub async fn distinct_activity_event_types(
    state: State<'_, AppState>,
) -> CommandResult<Vec<String>> {
    let types = state
        .state()
        .distinct_activity_event_types()
        .await
        .map_err(CommandError::from)?;
    tracing::debug!(target: TARGET, count = types.len(), "distinct_activity_event_types");
    Ok(types)
}

/// `activity_summary(day_start_ms, week_start_ms, throughput_window_ms)` - the
/// Activity dashboard header aggregates (M7-P2-5; DESIGN s8.3): bytes uploaded
/// today / this week, file count by status, and the current throughput window.
///
/// The day / week boundaries are supplied by the webview (computed from the
/// LOCAL `Date`, so the day boundary honours the user's timezone without a
/// timezone crate in the backend); the command bounds them and derives the
/// throughput window start from `now - throughput_window_ms`. All inputs are
/// scalars - no path validation applies (SPEC s11.6.1). Boundaries that are
/// negative or inverted (`week_start > day_start`) are rejected as a caller bug.
#[tauri::command]
pub async fn activity_summary(
    state: State<'_, AppState>,
    day_start_ms: i64,
    week_start_ms: i64,
    throughput_window_ms: u64,
) -> CommandResult<ActivitySummaryDto> {
    if day_start_ms < 0 || week_start_ms < 0 {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "activity_summary day/week start must be non-negative Unix ms",
        ));
    }
    if week_start_ms > day_start_ms {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            "activity_summary week_start_ms must be <= day_start_ms",
        ));
    }
    if !(1..=MAX_THROUGHPUT_WINDOW_MS).contains(&throughput_window_ms) {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            format!(
                "activity_summary throughput_window_ms must be 1..={MAX_THROUGHPUT_WINDOW_MS}, got {throughput_window_ms}"
            ),
        ));
    }

    let now = SystemClock.now_ms();
    // Clamp the window start at 0 so a clock skew / tiny `now` can never produce
    // a negative lower bound (the query treats `ts >= bound`).
    let window_start = now.saturating_sub(throughput_window_ms as i64).max(0);

    let summary = state
        .state()
        .activity_summary(
            day_start_ms,
            week_start_ms,
            window_start,
            throughput_window_ms,
        )
        .await
        .map_err(CommandError::from)?;

    let file_status_counts = summary
        .file_status_counts
        .into_iter()
        .map(|c| FileStatusCountDto {
            status: file_state_status_str(c.status).to_string(),
            count: c.count,
        })
        .collect();

    tracing::debug!(
        target: TARGET,
        bytes_today = summary.bytes_today,
        bytes_week = summary.bytes_week,
        "activity_summary served"
    );

    Ok(ActivitySummaryDto {
        bytes_today: summary.bytes_today,
        bytes_week: summary.bytes_week,
        file_status_counts,
        throughput_window_bytes: summary.throughput_window_bytes,
        throughput_window_ms: summary.throughput_window_ms,
    })
}

/// Map a [`FileStateStatus`] to its stable wire string (matching the SPEC s2
/// `file_state.status` TEXT values + the SQLite serialiser). An explicit match
/// (not serde) so the wire contract is visible + the i18n key base is stable.
fn file_state_status_str(s: FileStateStatus) -> &'static str {
    match s {
        FileStateStatus::Synced => "synced",
        FileStateStatus::Pending => "pending",
        FileStateStatus::Corrupt => "corrupt",
        FileStateStatus::Locked => "locked",
        FileStateStatus::Error => "error",
        FileStateStatus::ExcludedOrphan => "excluded_orphan",
    }
}

/// Validate + lower the webview filter DTO to the core [`ActivityFilter`]
/// (SPEC s11.6.1). Rejects a malformed source id, an unknown `min_level`, an
/// over-long / over-numerous `event_types` list, and an inverted time window.
fn validate_filter(dto: ActivityFilterDto) -> CommandResult<ActivityFilter> {
    let source_id = match dto.source_id {
        None => None,
        Some(s) => Some(s.parse::<SourceId>().map_err(|_| {
            CommandError::with_code(
                ErrorCode::InvalidInput,
                "activity filter source_id is not a valid id",
            )
        })?),
    };

    let min_level = match dto.min_level.as_deref() {
        None => None,
        Some("info") => Some(ActivityLevel::Info),
        Some("warn") => Some(ActivityLevel::Warn),
        Some("error") => Some(ActivityLevel::Error),
        Some(other) => {
            return Err(CommandError::with_code(
                ErrorCode::InvalidInput,
                format!("activity filter min_level `{other}` is not a known level"),
            ))
        }
    };

    if dto.event_types.len() > MAX_EVENT_TYPE_FILTERS {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            format!(
                "activity filter event_types has {} entries; max is {MAX_EVENT_TYPE_FILTERS}",
                dto.event_types.len()
            ),
        ));
    }
    for et in &dto.event_types {
        if et.is_empty() || et.len() > MAX_EVENT_TYPE_LEN {
            return Err(CommandError::with_code(
                ErrorCode::InvalidInput,
                "activity filter event_type entry is empty or too long",
            ));
        }
    }

    // An inverted window (since >= before) can only match nothing; reject it as
    // a caller bug rather than silently returning an empty page.
    if let (Some(since), Some(before)) = (dto.since_ms, dto.before_ms) {
        if since >= before {
            return Err(CommandError::with_code(
                ErrorCode::InvalidInput,
                "activity filter sinceMs must be < beforeMs",
            ));
        }
    }

    Ok(ActivityFilter {
        source_id,
        since_ms: dto.since_ms,
        before_ms: dto.before_ms,
        min_level,
        event_types: dto.event_types,
    })
}

/// Validate + bound the webview page DTO to the core [`PageRequest`]
/// (SPEC s11.6.1). The `limit` MUST be `1..=MAX_ACTIVITY_PAGE_LIMIT`; a `0` or
/// over-cap value is rejected (not silently clamped, mirroring the StateRepo's
/// own bound) so a buggy caller learns its page request is wrong.
fn validate_page(dto: PageRequestDto) -> CommandResult<PageRequest> {
    if !(1..=MAX_ACTIVITY_PAGE_LIMIT).contains(&dto.limit) {
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            format!(
                "activity page limit must be 1..={MAX_ACTIVITY_PAGE_LIMIT}, got {}",
                dto.limit
            ),
        ));
    }
    Ok(PageRequest {
        page: dto.page,
        limit: dto.limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_page_rejects_zero_and_over_cap() {
        let err = validate_page(PageRequestDto { page: 0, limit: 0 })
            .expect_err("zero limit must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        let err = validate_page(PageRequestDto {
            page: 0,
            limit: MAX_ACTIVITY_PAGE_LIMIT + 1,
        })
        .expect_err("over-cap limit must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn validate_page_accepts_inclusive_bounds() {
        let p = validate_page(PageRequestDto { page: 3, limit: 1 }).expect("min limit");
        assert_eq!(p.page, 3);
        assert_eq!(p.limit, 1);
        let p = validate_page(PageRequestDto {
            page: 0,
            limit: MAX_ACTIVITY_PAGE_LIMIT,
        })
        .expect("max limit");
        assert_eq!(p.limit, MAX_ACTIVITY_PAGE_LIMIT);
    }

    #[test]
    fn validate_filter_rejects_unknown_level() {
        let dto = ActivityFilterDto {
            min_level: Some("verbose".to_string()),
            ..Default::default()
        };
        let err = validate_filter(dto).expect_err("unknown level rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn validate_filter_maps_known_levels() {
        for (s, want) in [
            ("info", ActivityLevel::Info),
            ("warn", ActivityLevel::Warn),
            ("error", ActivityLevel::Error),
        ] {
            let dto = ActivityFilterDto {
                min_level: Some(s.to_string()),
                ..Default::default()
            };
            let f = validate_filter(dto).expect("known level");
            assert_eq!(f.min_level, Some(want));
        }
    }

    #[test]
    fn validate_filter_rejects_bad_source_id() {
        let dto = ActivityFilterDto {
            source_id: Some("not-a-uuid".to_string()),
            ..Default::default()
        };
        let err = validate_filter(dto).expect_err("bad source id rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn validate_filter_rejects_inverted_window() {
        let dto = ActivityFilterDto {
            since_ms: Some(2000),
            before_ms: Some(1000),
            ..Default::default()
        };
        let err = validate_filter(dto).expect_err("inverted window rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn validate_filter_rejects_too_many_event_types() {
        let dto = ActivityFilterDto {
            event_types: vec!["upload_done".to_string(); MAX_EVENT_TYPE_FILTERS + 1],
            ..Default::default()
        };
        let err = validate_filter(dto).expect_err("over-count event_types rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn validate_filter_rejects_oversized_event_type() {
        let dto = ActivityFilterDto {
            event_types: vec!["x".repeat(MAX_EVENT_TYPE_LEN + 1)],
            ..Default::default()
        };
        let err = validate_filter(dto).expect_err("oversized event_type rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn file_state_status_str_round_trips_all_variants() {
        // M7-P2-5: the status->wire mapping must cover every variant and match
        // the SPEC s2 / SQLite TEXT values exactly (the i18n key base).
        assert_eq!(file_state_status_str(FileStateStatus::Synced), "synced");
        assert_eq!(file_state_status_str(FileStateStatus::Pending), "pending");
        assert_eq!(file_state_status_str(FileStateStatus::Corrupt), "corrupt");
        assert_eq!(file_state_status_str(FileStateStatus::Locked), "locked");
        assert_eq!(file_state_status_str(FileStateStatus::Error), "error");
        assert_eq!(
            file_state_status_str(FileStateStatus::ExcludedOrphan),
            "excluded_orphan"
        );
    }

    #[test]
    fn validate_filter_passes_clean_input() {
        let sid = SourceId::new_v4();
        let dto = ActivityFilterDto {
            source_id: Some(sid.to_string()),
            since_ms: Some(1000),
            before_ms: Some(2000),
            min_level: Some("warn".to_string()),
            event_types: vec!["upload_done".to_string(), "scan_done".to_string()],
        };
        let f = validate_filter(dto).expect("clean filter");
        assert_eq!(f.source_id, Some(sid));
        assert_eq!(f.since_ms, Some(1000));
        assert_eq!(f.before_ms, Some(2000));
        assert_eq!(f.min_level, Some(ActivityLevel::Warn));
        assert_eq!(f.event_types.len(), 2);
    }
}
