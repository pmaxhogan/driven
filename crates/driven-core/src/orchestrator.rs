//! The sync orchestrator: one per account, driving the
//! [`OrchestratorState`](crate::types::OrchestratorState) machine
//! (SPEC s5, DESIGN s5.1).
//!
//! The orchestrator is the conductor. Each tick (scheduler timer, a
//! watcher [`ScanTickRequest`](crate::watcher::ScanTickRequest), or a
//! manual trigger) it checks the power / network gates (DESIGN s5.7), then
//! for each enabled source runs scan -> plan -> execute -> verify (SPEC
//! s5), transitioning the state machine and broadcasting
//! [`OrchestratorEvent`](crate::types::OrchestratorEvent)s to the IPC
//! bridge, tray, and activity-log writer (SPEC s11.7). It reacts to
//! [`PowerEvent`](crate::types::PowerEvent) suspend/resume (DESIGN s5.10)
//! and [`NetworkEvent`](crate::network::NetworkEvent) transitions (DESIGN
//! s5.8) on the same event loop.
//!
//! This module is the I/O-free *control-surface* contract. The full SPEC
//! s5 `Orchestrator` struct (with its `ActivityWriter`, `OrchestratorConfig`
//! handle, and broadcast wiring) is assembled by the M3 implementer; the
//! interfaces phase ships the state/event/progress types (in
//! [`crate::types`]) plus the [`Orchestrator`] control trait and the
//! shared [`OrchestratorConfig`] the implementers depend on. Deferring the
//! concrete struct keeps the activity-writer surface out of this phase
//! (see the M3 phase-1 finding).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::pacer::PacerCeilings;
use crate::types::OrchestratorState;

/// Runtime configuration the orchestrator reads each cycle (SPEC s5
/// `config: Arc<RwLock<OrchestratorConfig>>`).
///
/// Held behind a lock by the impl so a settings change takes effect on the
/// next cycle without restarting the orchestrator. This is the subset of
/// `settings` (SPEC s22) the orchestrator's *control flow* reads; per-op
/// crypto / bandwidth specifics flow through the executor + pacer seams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorConfig {
    /// When `true`, the cycle stops after planning and logs a dry-run
    /// summary instead of executing (SPEC s5 `if config.dry_run`).
    pub dry_run: bool,
    /// Pause sync while on battery (DESIGN s5.7). Maps a battery state to
    /// [`PauseReason::Battery`](crate::types::PauseReason::Battery).
    pub skip_on_battery: bool,
    /// Pause sync on a metered network (DESIGN s5.7). Maps to
    /// [`PauseReason::Metered`](crate::types::PauseReason::Metered).
    pub skip_on_metered: bool,
    /// Base scheduled-scan interval in seconds; the authoritative fallback
    /// the watcher only accelerates (DESIGN s5.9).
    pub scan_interval_secs: u64,
    /// Bandwidth cap in Mbps, or `None` for unlimited (SPEC s9 bandwidth
    /// bucket). Threaded to the [`Pacer`](crate::pacer::Pacer) at
    /// construction; carried here so a settings change re-derives it.
    pub bandwidth_cap_mbps: Option<u32>,
    /// The pacer's hard-cap ceilings (SPEC s9, DESIGN s18.1). The current
    /// AIMD budget lives in the pacer; only the user-configurable caps are
    /// config.
    pub pacer_ceilings: PacerCeilings,
}

impl Default for OrchestratorConfig {
    /// Conservative, gate-respecting defaults (DESIGN s5.7, s5.9, SPEC s9):
    /// no dry-run, skip on battery + metered, 15-minute scan interval, no
    /// bandwidth cap, default pacer ceilings.
    fn default() -> Self {
        Self {
            dry_run: false,
            skip_on_battery: true,
            skip_on_metered: true,
            scan_interval_secs: 15 * 60,
            bandwidth_cap_mbps: None,
            pacer_ceilings: PacerCeilings::default(),
        }
    }
}

/// What kicked off an orchestrator cycle (DESIGN s5.1 "tick").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TickSource {
    /// The scheduled-scan timer fired (the authoritative fallback,
    /// DESIGN s5.9.5).
    Scheduled,
    /// A debounced filesystem-watcher request (DESIGN s5.9.1).
    Watcher,
    /// The user clicked "Sync now".
    Manual,
    /// A resume-from-sleep sequence asked for a re-scan (DESIGN s5.10.3
    /// step 5).
    Wake,
}

/// The orchestrator control surface (SPEC s5).
///
/// One instance per account. The owning process spawns [`Self::run`] as a
/// long-lived task; the IPC layer and tray call the control methods to
/// pause/resume and to read the current state for display. The state
/// machine, scan/plan/execute/verify sequencing, and event broadcasting
/// are the implementer's; this trait is the seam the Tauri command layer
/// (SPEC s11.3) and tests code against.
#[async_trait]
pub trait Orchestrator: Send + Sync {
    /// Runs the orchestrator loop until cancelled (SPEC s5
    /// `run(self: Arc<Self>)`). Each iteration waits for a tick or signal,
    /// checks the gates, and runs every enabled source. Returns `Ok(())`
    /// on a clean shutdown; an unrecoverable error propagates.
    async fn run(&self) -> anyhow::Result<()>;

    /// Requests an out-of-band cycle now, recording `reason` as the tick
    /// source (DESIGN s5.1). Ticks every enabled source for the account.
    async fn trigger(&self, reason: TickSource);

    /// Sets the manual-pause signal (DESIGN s5.7: orthogonal to the
    /// gate-driven pauses, persists across restarts). `true` pauses,
    /// `false` resumes.
    async fn set_paused(&self, paused: bool);

    /// Returns a snapshot of the current
    /// [`OrchestratorState`] for the tray / Activity dashboard.
    async fn state(&self) -> OrchestratorState;

    /// Applies a new [`OrchestratorConfig`], taking effect on the next
    /// cycle (the `Arc<RwLock<OrchestratorConfig>>` swap, SPEC s5).
    async fn reconfigure(&self, config: OrchestratorConfig);
}
