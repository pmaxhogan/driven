//! Debug logging mode policy (issue #309).
//!
//! `logging.rs` owns the MECHANISM (a reloadable tracing filter + a dynamic
//! rolling-log cap); this module owns the POLICY: applying the persisted
//! `global.debug_logging_enabled` / `global.debug_logging_expires_at_ms`
//! (SPEC s22) to the live process at boot, and auto-turning the toggle back
//! off 24h after it was switched on - even if the app keeps running the
//! whole time, not just at the next restart.
//!
//! The expiry is PERSISTED (not an in-memory timer), so a restart mid-window
//! honours the original deadline rather than granting a fresh 24h, and an app
//! that was closed past the deadline turns itself off the moment it reopens
//! (before the first watchdog tick), matching the mockup's "turns itself off
//! automatically after 24 hours" promise regardless of whether the app was
//! running continuously.
//!
//! Lifecycle pattern: mirrors `bottleneck_hub.rs` / `iostat_hub.rs` (issue
//! #299's no-orphan quit drain) - a `tokio::spawn` loop, spawned once from
//! `setup` after `AppState` is managed, registered on `AppState` via
//! `set_debug_mode_task` so the app-quit drain (`lib.rs`
//! `ShutdownHandles`/`drain_shutdown_handles`) can signal it to stop and join
//! it with NO orphan, the same as every other periodic background task.

use std::sync::Arc;
use std::time::Duration;

use tauri::AppHandle;

use driven_core::state::StateRepo;
use driven_core::time::{Clock, SystemClock};

const TARGET: &str = "driven::app::debug_mode";

/// How long debug logging mode stays on after being switched on, before it
/// auto-turns-off (issue #309 mockup: "Turns itself off automatically after
/// 24 hours").
pub const AUTO_OFF_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// How often the watchdog re-checks the persisted expiry while the app keeps
/// running. Coarse enough to be free in steady state; fine enough that the
/// 24h auto-off lands within a few minutes of its deadline rather than only
/// at the next restart (which could be days later).
const CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Apply `enabled` to the LIVE process (issue #309): reload the tracing
/// filter to [`crate::logging::DEBUG_MODE_FILTER`] / back to
/// [`crate::logging::DEFAULT_FILTER`], and widen / restore the rolling
/// log-file cap. Best-effort: a reload-handle miss (process not fully booted
/// yet, or a test harness with no subscriber installed) is logged and never
/// fails the settings save that calls this.
pub fn apply(enabled: bool) {
    let directive = if enabled {
        crate::logging::DEBUG_MODE_FILTER
    } else {
        crate::logging::DEFAULT_FILTER
    };
    if !crate::logging::set_filter(directive) {
        tracing::debug!(
            target: TARGET,
            enabled,
            "tracing filter reload skipped (reload handle not installed yet)"
        );
    }
    crate::logging::set_debug_log_cap(enabled);
    tracing::info!(target: TARGET, enabled, "debug logging mode applied to the live process");
}

/// Boot-time reconciliation + the periodic 24h-window watchdog. Called once
/// from `setup`, after `AppState` is managed (mirrors
/// `bottleneck_hub::spawn_sampler` / `iostat_hub::spawn_sampler`, including
/// registering the task + shutdown sender on `AppState` so issue #299's
/// no-orphan quit drain stops and joins it).
pub fn spawn_watchdog(app: &AppHandle) {
    use tauri::Manager;
    let Some(app_state) = app.try_state::<crate::app_state::AppState>() else {
        tracing::warn!(target: TARGET, "AppState not managed; debug-mode watchdog not started");
        return;
    };
    let state: Arc<dyn StateRepo> = Arc::clone(app_state.state());
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let task = tokio::spawn(async move {
        // Boot-time reconciliation FIRST: apply whatever is currently
        // persisted (including an immediate auto-off if the 24h deadline
        // already passed while the app was closed) before the first tick.
        reconcile(state.as_ref()).await;

        let mut ticker = tokio::time::interval(CHECK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                res = shutdown_rx.changed() => {
                    match res {
                        Ok(()) if *shutdown_rx.borrow() => break,
                        Ok(()) => {}
                        Err(_) => break,
                    }
                }
                _ = ticker.tick() => {
                    reconcile(state.as_ref()).await;
                }
            }
        }
        tracing::debug!(target: TARGET, "debug-mode watchdog exited");
    });

    app_state.set_debug_mode_task(task, shutdown_tx);
    tracing::info!(
        target: TARGET,
        interval_ms = CHECK_INTERVAL.as_millis() as u64,
        "debug-mode watchdog started"
    );
}

/// Read the persisted debug-logging state and reconcile the live process +
/// (if the window expired) the persisted settings with it. Best-effort: a
/// state-DB read/write failure is logged and skipped - the next tick (or the
/// next restart) tries again, and the live process is never left MORE
/// verbose than the persisted settings say it should be for longer than one
/// `CHECK_INTERVAL`.
async fn reconcile(state: &dyn StateRepo) {
    let Some((enabled, expires_at_ms)) =
        crate::commands::settings::load_debug_logging_state(state).await
    else {
        tracing::debug!(target: TARGET, "could not read debug-logging settings; skipping this reconcile pass");
        return;
    };

    if !enabled {
        // Off is always applied (idempotent - a reload to the same filter is
        // cheap), so a process that booted before `apply(false)` ever ran
        // (e.g. the reload handle was not installed yet at settings-load
        // time) still converges to the correct live filter here.
        apply(false);
        return;
    }

    let now_ms = SystemClock.now_ms();
    let expired = expires_at_ms.is_none_or(|deadline| now_ms >= deadline);
    if expired {
        tracing::info!(
            target: TARGET,
            expires_at_ms,
            now_ms,
            "debug logging mode's 24h window elapsed; turning it off automatically"
        );
        if let Err(err) = crate::commands::settings::persist_debug_logging_off(state).await {
            tracing::warn!(target: TARGET, %err, "failed to persist debug-logging auto-off; will retry next tick");
            // Still apply off to the LIVE process even if the persist failed,
            // so at minimum this process's verbosity/log-cap correct
            // themselves now rather than staying wrong until the retry lands.
            apply(false);
            return;
        }
        apply(false);
        return;
    }

    apply(true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_core::state::sqlite::SqliteStateRepo;

    #[test]
    fn auto_off_window_is_24_hours() {
        assert_eq!(AUTO_OFF_WINDOW_MS, 24 * 60 * 60 * 1000);
    }

    /// A temp-backed state repo with the SPEC s22 settings seeded (mirrors
    /// `commands::settings::tests::seeded_repo` - duplicated rather than
    /// shared since that helper is private to its module and this module has
    /// no other reason to depend on it).
    async fn seeded_repo() -> (SqliteStateRepo, std::path::PathBuf) {
        // CodeQL `rust/path-injection` (driven-ci-flakes precedent, PR 151 /
        // src-tauri/Cargo.toml's `tempfile` dependency comment): a hand-rolled
        // `std::env::temp_dir().join(format!(...))` is exactly the pattern the
        // rule flags feeding `SqliteStateRepo::open`. `tempfile::tempdir()` is
        // an opaque external call CodeQL's dataflow does not see into, so the
        // taint chain never forms - fix, not a dismissal. `keep()` keeps
        // the directory alive (matching the pre-`tempfile` behaviour) so the
        // caller's existing `cleanup(dir)` teardown still applies.
        let dir = tempfile::tempdir().expect("create temp dir").keep();
        let repo = SqliteStateRepo::open(&dir.join("state.db"))
            .await
            .expect("open seeded state repo");
        (repo, dir)
    }

    fn cleanup(dir: std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn reconcile_is_a_no_op_when_debug_logging_was_never_enabled() {
        let (repo, dir) = seeded_repo().await;
        // Must not panic / error on the default (off, no expiry) state - this
        // is the path every normal boot takes.
        reconcile(&repo).await;
        let (enabled, expires_at_ms) = crate::commands::settings::load_debug_logging_state(&repo)
            .await
            .expect("read state");
        assert!(!enabled);
        assert_eq!(expires_at_ms, None);
        cleanup(dir);
    }

    #[tokio::test]
    async fn reconcile_auto_turns_off_an_expired_window() {
        let (repo, dir) = seeded_repo().await;
        // Simulate "debug mode was switched on a long time ago" - an expiry
        // far in the past, as if the app had been closed past the 24h
        // deadline (issue #309: "turns itself off automatically after 24
        // hours" must hold even across a restart, not just while running).
        crate::commands::settings::set_debug_logging_state_for_test(&repo, true, Some(1)).await;

        reconcile(&repo).await;

        let (enabled, expires_at_ms) = crate::commands::settings::load_debug_logging_state(&repo)
            .await
            .expect("read state");
        assert!(!enabled, "an expired window must be turned off");
        assert_eq!(expires_at_ms, None, "the expiry must be cleared with it");
        cleanup(dir);
    }

    #[tokio::test]
    async fn reconcile_turns_off_a_missing_expiry_defensively() {
        // `enabled=true` with `expires_at_ms=None` should never happen through
        // the normal write path (compute_debug_logging_state always sets an
        // expiry when enabling), but a corrupted / hand-edited DB row could
        // produce it - fail closed to OFF rather than staying verbose forever
        // with no deadline.
        let (repo, dir) = seeded_repo().await;
        crate::commands::settings::set_debug_logging_state_for_test(&repo, true, None).await;

        reconcile(&repo).await;

        let (enabled, _) = crate::commands::settings::load_debug_logging_state(&repo)
            .await
            .expect("read state");
        assert!(!enabled, "a missing expiry must fail closed to off");
        cleanup(dir);
    }

    #[tokio::test]
    async fn reconcile_keeps_a_still_valid_window_enabled() {
        let (repo, dir) = seeded_repo().await;
        // An expiry far in the future - the window has not elapsed yet.
        let far_future_ms = SystemClock.now_ms() + AUTO_OFF_WINDOW_MS;
        crate::commands::settings::set_debug_logging_state_for_test(
            &repo,
            true,
            Some(far_future_ms),
        )
        .await;

        reconcile(&repo).await;

        let (enabled, expires_at_ms) = crate::commands::settings::load_debug_logging_state(&repo)
            .await
            .expect("read state");
        assert!(enabled, "a still-valid window must stay on");
        assert_eq!(
            expires_at_ms,
            Some(far_future_ms),
            "reconcile must not touch a still-valid expiry"
        );
        cleanup(dir);
    }
}
