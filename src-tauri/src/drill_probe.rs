//! The app-shell half of the scheduled RESTORE DRILL (`driven_core::drill`).
//!
//! `driven-core` owns the schedule, the deterministic sampling, and the report;
//! it cannot own the restore itself, because the real restore path (dialog-token
//! destination confinement, per-source crypto resolution, stream decryption,
//! bundle extraction, atomic writes) lives here in the Tauri shell alongside the
//! `AppState` it needs. This module is the seam between the two: a
//! [`RestoreProbe`] that runs one candidate through
//! [`crate::commands::restore::drill_restore_one`].
//!
//! It holds an [`AppHandle`] rather than an `AppState` reference because the
//! orchestrator outlives any borrow: the probe resolves the managed state fresh
//! on each call, so a drill that fires after an account was rebuilt sees the
//! current state rather than a stale snapshot.

use std::sync::Arc;

use tauri::{AppHandle, Manager};

use driven_core::drill::{DrillAttempt, DrillCandidate, RestoreProbe};
use driven_core::types::ErrorCode;

use crate::app_state::AppState;

const TARGET: &str = "driven::drill";

/// The real restore probe: runs a drill candidate through the app's actual
/// restore path.
pub struct AppRestoreProbe {
    app: AppHandle,
}

impl AppRestoreProbe {
    /// Wrap an app handle as a restore probe.
    #[must_use]
    pub fn new(app: AppHandle) -> Arc<Self> {
        Arc::new(AppRestoreProbe { app })
    }
}

#[async_trait::async_trait]
impl RestoreProbe for AppRestoreProbe {
    async fn restore_and_verify(&self, candidate: &DrillCandidate) -> DrillAttempt {
        // `try_state` rather than `state`: `state` PANICS when the managed value
        // is absent, and a drill firing during teardown (or in a test harness
        // with no managed state) must degrade to "skipped", never take the
        // process down. A backup tool that crashes while checking itself is
        // strictly worse than one that skips the check.
        let Some(state) = self.app.try_state::<AppState>() else {
            tracing::debug!(
                target: TARGET,
                "restore drill skipped: app state is not available"
            );
            return DrillAttempt::Skipped {
                code: ErrorCode::InternalBug,
            };
        };
        crate::commands::restore::drill_restore_one(state.inner(), candidate).await
    }
}
