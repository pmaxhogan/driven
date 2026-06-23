//! System-tray icon + menu (SPEC s12, DESIGN s8.1).
//!
//! The tray is always present. [`build`] constructs the `TrayIconBuilder`
//! with the menu from DESIGN s8.1 ("Sync now" / "Pause for..." / "Settings" /
//! "Activity" / "Restore" / "About" / "Quit") and wires its menu + click
//! handlers to the sync IPC commands (SPEC s12). [`apply_state`] swaps the
//! icon as the orchestrator's [`OrchestratorState`] changes (DESIGN s8.1 icon
//! state machine).
//!
//! Linux caveat (SPEC s12, DESIGN s8.1): tray-icon click events may not fire
//! on every desktop environment, so EVERY action must also be reachable from
//! the right-click menu - the menu is the canonical surface.

use driven_core::types::OrchestratorState;
use tauri::AppHandle;

/// The tray icon to display for a given orchestrator state (DESIGN s8.1).
///
/// Maps the [`OrchestratorState`] machine to the five DESIGN s8.1 visuals.
/// The yellow-with-`!` "network attention" visual is selected from a
/// `Paused`/`Error` whose reason/detail is a network condition (resolved in
/// [`apply_state`]); the bare variants below are the non-network cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIcon {
    /// Default gray: idle, last sync OK.
    Idle,
    /// Animated spinner: a sync cycle is in progress.
    Syncing,
    /// Yellow: paused (user or auto - battery / metered / schedule).
    Paused,
    /// Yellow with `!` badge: network attention (offline / captive portal /
    /// Drive unreachable) - covers all DESIGN s5.8 network failure modes.
    NetworkAttention,
    /// Red: an error needs attention (auth needed, decrypt failure, disk full).
    Error,
}

impl TrayIcon {
    /// Pick the icon for `state` (DESIGN s8.1 state machine).
    ///
    /// TODO(M5): implement the full DESIGN s8.1 mapping:
    /// - `Idle` -> [`TrayIcon::Idle`];
    /// - `PowerCheck` / `Scanning` / `Planning` / `Executing` / `Verifying` ->
    ///   [`TrayIcon::Syncing`];
    /// - `Backoff` -> [`TrayIcon::Syncing`] (transient retry, not an error);
    /// - `Paused { reason }` -> [`TrayIcon::NetworkAttention`] when the reason
    ///   is a network condition (offline / captive / service-down), else
    ///   [`TrayIcon::Paused`];
    /// - `Error { detail }` -> [`TrayIcon::NetworkAttention`] for a network
    ///   error code, else [`TrayIcon::Error`].
    #[must_use]
    pub fn for_state(state: &OrchestratorState) -> Self {
        let _ = state;
        todo!("M5: map OrchestratorState -> TrayIcon per DESIGN s8.1")
    }
}

/// Build the tray icon + menu and register its handlers (SPEC s12).
///
/// TODO(M5): per SPEC s12 -
/// `TrayIconBuilder::with_id("driven-main").icon(idle_icon()).menu(&build_menu(app)?)`
/// with `on_menu_event` dispatching `sync_now` / `settings` / `activity` /
/// `restore` / `pause_30m` / `quit` (spawn the async sync commands), and
/// `on_tray_icon_event` opening Activity on a left click. Gracefully degrade
/// on Linux (menu is canonical).
pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let _ = app;
    todo!("M5: TrayIconBuilder with DESIGN s8.1 menu + SPEC s12 handlers")
}

/// Swap the tray icon to match `state` (DESIGN s8.1), called from the
/// orchestrator-event bridge on every [`OrchestratorState`] transition (tray
/// must update within 1s per ROADMAP M5 acceptance).
///
/// TODO(M5): resolve `TrayIcon::for_state(&state)`, look up the tray by id
/// (`app.tray_by_id("driven-main")`), and set its icon + tooltip (the DESIGN
/// s8.1 condition string for the network-attention state).
pub fn apply_state(app: &AppHandle, state: OrchestratorState) {
    let _ = (app, state);
    todo!("M5: set tray icon + tooltip from TrayIcon::for_state(state)")
}
