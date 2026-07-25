//! Frontend (webview) console capture IPC command.
//!
//! Backend `tracing::` output lands in the rolling `driven.*.log` files and so
//! in the SPEC s18 diagnostic bundle, but until this command existed the
//! webview's own `console.*` output, uncaught errors, and unhandled promise
//! rejections were captured NOWHERE - a UI-side failure left no trace at all in
//! a user-submitted bundle.
//!
//! `ui/src/frontendLog.ts` wraps the console, buffers entries in a bounded ring,
//! and periodically ships batches here. [`report_frontend_logs`] re-emits each
//! entry through `tracing` under the `driven::frontend` target, so frontend and
//! backend lines interleave in ONE timeline in the same file - which is the
//! whole point when diagnosing "the button did nothing" against what the backend
//! was doing at that moment.
//!
//! ## Hardening (R3)
//!
//! The webview is untrusted: it can call this command in a loop with arbitrary
//! payloads, and everything it sends is written to a file on the user's disk
//! that later leaves the machine in a bundle. So the command bounds its input
//! rather than trusting the frontend's own limits:
//!
//! - at most [`MAX_ENTRIES_PER_CALL`] entries per call (an over-long batch is
//!   REJECTED, not silently halved - a caller sending 10k entries is buggy or
//!   hostile and should hear about it),
//! - each `text` truncated to [`MAX_TEXT_CHARS`] characters (truncated rather
//!   than rejected: a single huge line is usually a legitimate stack trace or a
//!   dumped object, and the first 2000 chars of it are still useful),
//! - control characters stripped so a crafted entry cannot forge extra log lines
//!   or inject terminal escapes into the log file,
//! - at most [`MAX_CALLS_PER_WINDOW`] calls per minute, beyond which entries are
//!   dropped with ONE warn per window (a rate-limit that itself logged per call
//!   would be the log-flood it exists to prevent).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use driven_core::types::ErrorCode;

use crate::commands::{CommandError, CommandResult};

/// Tracing target for this command's own diagnostics (rejections, rate limits).
const TARGET: &str = "driven::app::frontend_log";

/// Tracing target every re-emitted frontend entry carries, so a reader (and a
/// `RUST_LOG` filter) can tell webview lines from backend ones at a glance.
const FRONTEND_TARGET: &str = "driven::frontend";

/// Maximum entries one `report_frontend_logs` call may carry. Matches the
/// frontend's batch size with no headroom on purpose: a larger batch means the
/// caller is not `ui/src/frontendLog.ts`.
pub const MAX_ENTRIES_PER_CALL: usize = 200;

/// Maximum characters of a single entry's text. Counted in CHARS, not bytes, so
/// the truncation can never split a UTF-8 sequence.
pub const MAX_TEXT_CHARS: usize = 2000;

/// Rate limit: calls allowed per [`RATE_WINDOW`].
const MAX_CALLS_PER_WINDOW: u32 = 50;

/// Rate-limit window. With the frontend's 5s flush timer a well-behaved UI uses
/// ~12 calls a minute, so 50 leaves generous room for eager flushes during a
/// burst while still capping a runaway loop.
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// One captured frontend log entry, as the webview sends it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrontendLogEntry {
    /// `error` | `warn` | `info` | `debug` | `trace`. Anything else maps to
    /// `info` - an unknown level is not worth dropping a log line over.
    pub level: String,
    /// `Date.now()` at capture time (epoch milliseconds). Recorded as a field
    /// rather than used as THE timestamp because the log file's own timestamps
    /// come from the backend clock; carrying both makes a clock skew visible
    /// instead of silently reordering the timeline.
    pub ts: i64,
    /// The formatted console arguments (or the error / rejection description).
    pub text: String,
}

/// `report_frontend_logs(entries)` - accept a batch of webview console entries
/// and re-emit them into the backend tracing pipeline (and thus the rolling log
/// file + the SPEC s18 diagnostic bundle).
///
/// Returns `Err(internal.invalid_input)` only for a batch that breaks the size
/// contract. A rate-limited batch returns `Ok(())` with the entries dropped:
/// signalling failure would make a well-behaved frontend re-queue and retry the
/// very traffic the limit is shedding.
#[tauri::command]
pub fn report_frontend_logs(entries: Vec<FrontendLogEntry>) -> CommandResult<()> {
    let entries = sanitize_batch(entries)?;
    if entries.is_empty() {
        return Ok(());
    }
    if !rate_limiter_allows() {
        return Ok(());
    }
    for entry in entries {
        emit(&entry);
    }
    Ok(())
}

/// Enforce the batch contract: reject an over-long batch, truncate + scrub each
/// entry's text, and drop entries left empty by scrubbing.
///
/// Split out from the command body so the bounds are unit-testable without a
/// Tauri runtime.
fn sanitize_batch(entries: Vec<FrontendLogEntry>) -> CommandResult<Vec<FrontendLogEntry>> {
    if entries.len() > MAX_ENTRIES_PER_CALL {
        let count = entries.len();
        tracing::warn!(
            target: TARGET,
            count,
            max = MAX_ENTRIES_PER_CALL,
            "rejecting oversized frontend log batch"
        );
        return Err(CommandError::with_code(
            ErrorCode::InvalidInput,
            format!("frontend log batch of {count} exceeds the maximum of {MAX_ENTRIES_PER_CALL}"),
        ));
    }
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            let text = sanitize_text(&entry.text);
            if text.is_empty() {
                return None;
            }
            Some(FrontendLogEntry { text, ..entry })
        })
        .collect())
}

/// Scrub and bound one entry's text.
///
/// Control characters (including the newlines and carriage returns that would
/// let a crafted console message forge additional log lines, and the ESC that
/// would inject a terminal escape sequence into a file someone later `cat`s)
/// become spaces. Then the result is truncated to [`MAX_TEXT_CHARS`] CHARS with
/// an explicit marker, so a reader knows they are looking at a prefix rather
/// than a complete message.
fn sanitize_text(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.chars().count() <= MAX_TEXT_CHARS {
        return cleaned.to_string();
    }
    let head: String = cleaned.chars().take(MAX_TEXT_CHARS).collect();
    format!("{head}...[truncated]")
}

/// Re-emit one sanitized entry under [`FRONTEND_TARGET`] at its mapped level.
///
/// `tracing`'s macros need a level known at compile time, so this is a match
/// over five call sites rather than a dynamic level.
fn emit(entry: &FrontendLogEntry) {
    let ts = entry.ts;
    let text = entry.text.as_str();
    match entry.level.to_ascii_lowercase().as_str() {
        "error" => tracing::error!(target: FRONTEND_TARGET, ts, "{text}"),
        "warn" | "warning" => tracing::warn!(target: FRONTEND_TARGET, ts, "{text}"),
        "debug" => tracing::debug!(target: FRONTEND_TARGET, ts, "{text}"),
        "trace" => tracing::trace!(target: FRONTEND_TARGET, ts, "{text}"),
        // `info` and anything unrecognised.
        _ => tracing::info!(target: FRONTEND_TARGET, ts, "{text}"),
    }
}

/// Fixed-window call counter behind the rate limit.
///
/// A fixed window (rather than a sliding one or a token bucket) is deliberate:
/// the goal is only to stop a runaway webview from filling the log file, and the
/// worst case - 2x the nominal rate across a window boundary - is irrelevant at
/// this scale. `now` is a parameter so the window rollover is testable without
/// sleeping for a minute.
#[derive(Debug)]
struct RateLimiter {
    window_start: Option<Instant>,
    calls: u32,
    /// Whether the "dropping frontend logs" warn has already been emitted for
    /// the CURRENT window. One warn per window, not one per dropped batch.
    warned: bool,
}

impl RateLimiter {
    const fn new() -> Self {
        Self {
            window_start: None,
            calls: 0,
            warned: false,
        }
    }

    /// Record a call. `Some(true)` = allowed; `Some(false)` = dropped, and the
    /// caller should emit the one-per-window warn; `None` = dropped silently
    /// (already warned this window).
    fn check(&mut self, now: Instant, max_calls: u32, window: Duration) -> RateDecision {
        let expired = match self.window_start {
            None => true,
            Some(start) => now.duration_since(start) >= window,
        };
        if expired {
            self.window_start = Some(now);
            self.calls = 0;
            self.warned = false;
        }
        if self.calls < max_calls {
            self.calls += 1;
            return RateDecision::Allow;
        }
        if self.warned {
            RateDecision::DropQuietly
        } else {
            self.warned = true;
            RateDecision::DropAndWarn
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RateDecision {
    Allow,
    DropAndWarn,
    DropQuietly,
}

/// Process-wide limiter state. A `Mutex` (not an atomic) because the window
/// start and the counter must move together.
static RATE_LIMITER: Mutex<RateLimiter> = Mutex::new(RateLimiter::new());

/// Consult the process limiter, emitting the one-per-window warn when it starts
/// shedding. A poisoned lock fails OPEN (allow the call): losing the rate limit
/// is a far smaller problem than losing every frontend log line for the rest of
/// the process's life.
fn rate_limiter_allows() -> bool {
    let decision = match RATE_LIMITER.lock() {
        Ok(mut limiter) => limiter.check(Instant::now(), MAX_CALLS_PER_WINDOW, RATE_WINDOW),
        Err(_) => RateDecision::Allow,
    };
    match decision {
        RateDecision::Allow => true,
        RateDecision::DropAndWarn => {
            tracing::warn!(
                target: TARGET,
                max_calls = MAX_CALLS_PER_WINDOW,
                window_secs = RATE_WINDOW.as_secs(),
                "frontend log rate limit hit; dropping further batches this window"
            );
            false
        }
        RateDecision::DropQuietly => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(level: &str, text: &str) -> FrontendLogEntry {
        FrontendLogEntry {
            level: level.to_string(),
            ts: 1_750_000_000_000,
            text: text.to_string(),
        }
    }

    #[test]
    fn a_normal_batch_passes_through_unchanged() {
        let batch = vec![entry("warn", "fetch failed"), entry("error", "boom")];
        let out = sanitize_batch(batch.clone()).expect("normal batch accepted");
        assert_eq!(out, batch);
    }

    #[test]
    fn an_oversized_batch_is_rejected() {
        let batch = vec![entry("info", "x"); MAX_ENTRIES_PER_CALL + 1];
        let err = sanitize_batch(batch).expect_err("oversized batch must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        assert!(err.message.contains("201"), "message names the batch size");
    }

    #[test]
    fn a_batch_at_exactly_the_limit_is_accepted() {
        let batch = vec![entry("info", "x"); MAX_ENTRIES_PER_CALL];
        let out = sanitize_batch(batch).expect("batch at the limit accepted");
        assert_eq!(out.len(), MAX_ENTRIES_PER_CALL);
    }

    #[test]
    fn an_oversized_entry_is_truncated_not_rejected() {
        let long = "a".repeat(MAX_TEXT_CHARS + 500);
        let out = sanitize_batch(vec![entry("info", &long)]).expect("long entry accepted");
        assert_eq!(out.len(), 1);
        assert!(out[0].text.ends_with("...[truncated]"));
        assert_eq!(
            out[0].text.chars().count(),
            MAX_TEXT_CHARS + "...[truncated]".chars().count()
        );
    }

    #[test]
    fn truncation_counts_chars_so_it_never_splits_a_utf8_sequence() {
        // Multi-byte chars: a byte-based truncation would panic or produce
        // mojibake here.
        let long = "\u{1f4be}".repeat(MAX_TEXT_CHARS + 10);
        let out = sanitize_batch(vec![entry("info", &long)]).expect("multibyte entry accepted");
        let text = &out[0].text;
        assert!(text.starts_with('\u{1f4be}'));
        assert_eq!(
            text.chars().count(),
            MAX_TEXT_CHARS + "...[truncated]".chars().count()
        );
    }

    #[test]
    fn an_entry_at_exactly_the_char_limit_is_left_alone() {
        let exact = "b".repeat(MAX_TEXT_CHARS);
        let out = sanitize_batch(vec![entry("info", &exact)]).expect("exact-length entry accepted");
        assert_eq!(out[0].text, exact);
    }

    #[test]
    fn control_characters_cannot_forge_extra_log_lines() {
        let hostile = "real\n2026-07-25T00:00:00Z ERROR driven::core forged line\r\nmore";
        let out = sanitize_batch(vec![entry("info", hostile)]).expect("accepted");
        assert!(!out[0].text.contains('\n'));
        assert!(!out[0].text.contains('\r'));
        assert!(!out[0].text.contains('\u{1b}'));
        assert!(out[0].text.starts_with("real "));
    }

    #[test]
    fn entries_that_are_empty_after_scrubbing_are_dropped() {
        let out = sanitize_batch(vec![
            entry("info", "   \n\t  "),
            entry("info", ""),
            entry("info", "kept"),
        ])
        .expect("accepted");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "kept");
    }

    #[test]
    fn an_empty_batch_is_accepted_and_emits_nothing() {
        assert!(sanitize_batch(Vec::new())
            .expect("empty accepted")
            .is_empty());
        // The command short-circuits an all-empty batch before it ever consults
        // the rate limiter, so a chatty-but-blank frontend cannot burn the quota.
        assert!(report_frontend_logs(vec![entry("info", "  ")]).is_ok());
    }

    #[test]
    fn the_rate_limiter_allows_up_to_the_cap_then_sheds() {
        let mut limiter = RateLimiter::new();
        let now = Instant::now();
        for _ in 0..MAX_CALLS_PER_WINDOW {
            assert_eq!(
                limiter.check(now, MAX_CALLS_PER_WINDOW, RATE_WINDOW),
                RateDecision::Allow
            );
        }
        // First over-cap call warns once...
        assert_eq!(
            limiter.check(now, MAX_CALLS_PER_WINDOW, RATE_WINDOW),
            RateDecision::DropAndWarn
        );
        // ...and every subsequent one in the same window is silent, so the
        // rate-limit warn cannot itself become the log flood.
        for _ in 0..10 {
            assert_eq!(
                limiter.check(now, MAX_CALLS_PER_WINDOW, RATE_WINDOW),
                RateDecision::DropQuietly
            );
        }
    }

    #[test]
    fn the_rate_limit_window_rolls_over() {
        let mut limiter = RateLimiter::new();
        let start = Instant::now();
        for _ in 0..MAX_CALLS_PER_WINDOW {
            let _ = limiter.check(start, MAX_CALLS_PER_WINDOW, RATE_WINDOW);
        }
        assert_eq!(
            limiter.check(start, MAX_CALLS_PER_WINDOW, RATE_WINDOW),
            RateDecision::DropAndWarn
        );
        // A fresh window resets both the counter and the warned-once flag.
        let later = start + RATE_WINDOW + Duration::from_millis(1);
        assert_eq!(
            limiter.check(later, MAX_CALLS_PER_WINDOW, RATE_WINDOW),
            RateDecision::Allow
        );
    }

    #[test]
    fn every_level_string_maps_without_panicking() {
        // `emit` has no observable return; this pins that the full level set
        // (plus casing variants and an unknown value) is handled rather than
        // hitting an unreachable arm.
        for level in [
            "error", "warn", "warning", "info", "debug", "trace", "ERROR", "Warn", "nonsense", "",
        ] {
            emit(&entry(level, "level mapping"));
        }
    }
}
