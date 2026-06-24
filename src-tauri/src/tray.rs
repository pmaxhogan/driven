//! System-tray icon + menu (SPEC s12, DESIGN s8.1).
//!
//! The tray is always present. [`build`] constructs the `TrayIconBuilder`
//! with the menu from DESIGN s8.1 ("Sync now" / "Pause for..." / "Settings" /
//! "Activity" / "Restore" / "Quit") and wires its menu + click handlers to the
//! M5 sync IPC commands (SPEC s12). [`apply_state`] swaps the icon + tooltip as
//! the orchestrator's [`OrchestratorState`] changes (DESIGN s8.1 icon state
//! machine) and raises the DESIGN s117/s247 OS notifications (first-sync-
//! complete + red error).
//!
//! Linux caveat (SPEC s12, DESIGN s8.1): tray-icon click events may not fire
//! on every desktop environment, so EVERY action must also be reachable from
//! the right-click menu - the menu is the canonical surface.
//!
//! ## Icon assets (documented minimal approach)
//!
//! Rather than shipping five binary PNG variants, the per-state icons are
//! generated programmatically as flat RGBA tiles via
//! [`tauri::image::Image::new_owned`] (no image-decoding crate feature needed,
//! and Cargo.toml is frozen this milestone). Each state gets a distinct solid
//! colour with `set_icon_as_template(false)` so the colour survives macOS's
//! template-monochrome recolouring (the committed `tauri.conf.json` sets
//! `iconAsTemplate: true` for the static boot icon). HONEST LIMITATIONS, not
//! faked: `TrayIcon::Syncing` is a STATIC blue tile, not a real animated
//! spinner; `TrayIcon::NetworkAttention` is a distinct amber tile that
//! APPROXIMATES the yellow-with-`!` badge - no glyph is drawn into the tile.
//! The state machine, tooltip text, and notification routing are all real.

use std::sync::Mutex;

use driven_core::types::{ErrorCode, OrchestratorState, PauseReason};
use tauri::image::Image;
use tauri::menu::{Menu, MenuBuilder, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

const TARGET: &str = "driven::app::tray";

/// Canonical tray id (matches the committed `apply_state` lookup + SPEC s12).
const TRAY_ID: &str = "driven-main";

/// Id Tauri assigns to the tray created from `tauri.conf.json`'s
/// `app.trayIcon` block (it defaults to the main-window label `"main"`). We
/// remove it in [`build`] so the config-defined tray and our programmatic
/// `"driven-main"` tray do not BOTH show (a duplicate-icon footgun).
const CONFIG_TRAY_ID: &str = "main";

/// Menu item ids (SPEC s12 `event.id()` dispatch keys).
mod menu_id {
    pub const SYNC_NOW: &str = "sync_now";
    pub const PAUSE_30M: &str = "pause_30m";
    pub const RESUME: &str = "resume";
    pub const SETTINGS: &str = "settings";
    pub const ACTIVITY: &str = "activity";
    pub const RESTORE: &str = "restore";
    pub const QUIT: &str = "quit";
}

/// Tiny generated-tile dimensions. A 16x16 RGBA tile is a valid tray icon on
/// all three platforms; the OS scales it for HiDPI trays.
const TILE: u32 = 16;

/// The tray icon to display for a given orchestrator state (DESIGN s8.1).
///
/// Maps the [`OrchestratorState`] machine to the five DESIGN s8.1 visuals.
/// The yellow-with-`!` "network attention" visual is selected from a
/// `Paused`/`Error` whose reason/detail is a network condition (resolved in
/// [`TrayIcon::for_state`]); the bare variants below are the non-network cases.
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
    /// - `Idle` -> [`TrayIcon::Idle`];
    /// - `PowerCheck` / `Scanning` / `Planning` / `Executing` / `Verifying` /
    ///   `Backoff` -> [`TrayIcon::Syncing`] (`Backoff` is a transient retry,
    ///   not an error - per the committed scaffold note);
    /// - `Paused { reason }` -> [`TrayIcon::NetworkAttention`] when the reason
    ///   is a network condition (offline / no-internet / captive / DNS /
    ///   service-down), else [`TrayIcon::Paused`];
    /// - `Error { detail }` -> [`TrayIcon::NetworkAttention`] for a network /
    ///   reachability error code, else [`TrayIcon::Error`].
    #[must_use]
    pub fn for_state(state: &OrchestratorState) -> Self {
        match state {
            OrchestratorState::Idle { .. } => TrayIcon::Idle,
            OrchestratorState::PowerCheck
            | OrchestratorState::Scanning { .. }
            | OrchestratorState::Planning { .. }
            | OrchestratorState::Executing { .. }
            | OrchestratorState::Verifying { .. }
            | OrchestratorState::Backoff { .. } => TrayIcon::Syncing,
            OrchestratorState::Paused { reason } => {
                if pause_reason_is_network(*reason) {
                    TrayIcon::NetworkAttention
                } else {
                    TrayIcon::Paused
                }
            }
            OrchestratorState::Error { detail } => {
                if error_code_is_network(detail.code) {
                    TrayIcon::NetworkAttention
                } else {
                    TrayIcon::Error
                }
            }
        }
    }

    /// The solid RGBA colour (no alpha cutout) for this state's generated tile.
    fn rgba(self) -> [u8; 4] {
        match self {
            // Neutral gray - idle / last sync OK.
            TrayIcon::Idle => [0x9e, 0xa3, 0xa8, 0xff],
            // Blue - a sync cycle is running.
            TrayIcon::Syncing => [0x3b, 0x82, 0xf6, 0xff],
            // Amber/yellow - user/auto pause.
            TrayIcon::Paused => [0xf5, 0xb3, 0x00, 0xff],
            // Deeper amber - network attention (approximates yellow-with-`!`).
            TrayIcon::NetworkAttention => [0xff, 0x8c, 0x00, 0xff],
            // Red - error needs attention.
            TrayIcon::Error => [0xdc, 0x26, 0x26, 0xff],
        }
    }

    /// The flat `TILE x TILE` RGBA byte buffer for this state's tile
    /// (row-major, top-to-bottom; the shape [`Image::new_owned`] wants).
    fn rgba_buffer(self) -> Vec<u8> {
        let [r, g, b, a] = self.rgba();
        let mut buf = Vec::with_capacity((TILE * TILE * 4) as usize);
        for _ in 0..(TILE * TILE) {
            buf.extend_from_slice(&[r, g, b, a]);
        }
        buf
    }

    /// A freshly-allocated owned [`Image`] tile for this state's icon.
    fn image(self) -> Image<'static> {
        Image::new_owned(self.rgba_buffer(), TILE, TILE)
    }
}

/// Is this pause reason a network / reachability condition (DESIGN s8.1
/// yellow-with-`!`) rather than a plain user/auto pause (DESIGN s8.1 yellow)?
fn pause_reason_is_network(reason: PauseReason) -> bool {
    match reason {
        PauseReason::Manual | PauseReason::Battery | PauseReason::Metered => false,
        PauseReason::Offline
        | PauseReason::ServiceDown
        | PauseReason::NoInternet
        | PauseReason::CaptivePortal
        | PauseReason::DnsFailed => true,
    }
}

/// Is this error code a network / reachability error (DESIGN s8.1: the
/// yellow-with-`!` state "covers all of s5.8's network failure modes",
/// including "Drive unreachable")? Matches on the enum, not a string prefix,
/// so a newly-added [`ErrorCode`] fails to compile here until classified.
fn error_code_is_network(code: ErrorCode) -> bool {
    match code {
        // Pure network probe / reachability codes (SPEC s24 `net.*`).
        ErrorCode::NetOffline
        | ErrorCode::NetNoInternet
        | ErrorCode::NetDnsFailed
        | ErrorCode::NetCaptivePortal
        | ErrorCode::NetTimeout
        | ErrorCode::NetIntermittent
        | ErrorCode::NetProxyRequired
        // Drive unreachable / circuit-open is explicitly a yellow-bang case
        // (DESIGN s8.1 "Drive unreachable").
        | ErrorCode::DriveUnreachable
        // Couldn't reach the OAuth endpoint - a reachability problem, not a
        // credential problem (distinct from auth.invalid_grant below).
        | ErrorCode::AuthNetworkUnreachable
        // The update endpoint being unreachable is also a network condition.
        | ErrorCode::UpdateEndpointUnreachable => true,

        // Credential / consent failures -> red error (and the reauth path).
        ErrorCode::AuthInvalidGrant
        | ErrorCode::AuthConsentRequired
        // Drive-side quota / size / permission / checksum -> red error.
        | ErrorCode::DriveRateLimited
        | ErrorCode::DriveDailyQuotaExhausted
        | ErrorCode::DriveQuotaExhausted
        | ErrorCode::DriveUploadSizeLimit
        | ErrorCode::DriveChecksumMismatch
        | ErrorCode::DriveResumableSessionInvalid
        | ErrorCode::DriveDestFolderMissing
        | ErrorCode::DriveDestFolderPermissionDenied
        // Local filesystem / VSS errors -> red error.
        | ErrorCode::LocalFileLocked
        | ErrorCode::LocalVssUnavailable
        | ErrorCode::LocalFileChangedDuringUpload
        | ErrorCode::LocalFileReplacedDuringUpload
        | ErrorCode::LocalIoError
        | ErrorCode::LocalPathTooLong
        | ErrorCode::LocalUnicodeCollision
        | ErrorCode::LocalDiskFull
        | ErrorCode::LocalInvalidFilename
        | ErrorCode::LocalAdsSkipped
        // Updater signature / crypto / state / harness / internal -> red.
        | ErrorCode::UpdateSignatureInvalid
        | ErrorCode::CryptoKeyMissing
        | ErrorCode::CryptoDecryptFailed
        | ErrorCode::CryptoRecoveryPhraseInvalid
        | ErrorCode::StateDbLocked
        | ErrorCode::StateDbCorrupt
        | ErrorCode::StateReconcileOrphan
        | ErrorCode::HarnessTimeout
        | ErrorCode::InternalBug => false,
    }
}

/// The localised tooltip string for `state` (DESIGN s8.1: the tooltip shows
/// the specific condition for the network-attention / paused / error states).
fn tooltip_for(state: &OrchestratorState) -> String {
    match state {
        OrchestratorState::Idle { .. } => rust_i18n::t!("tray.tooltip.idle").into_owned(),
        OrchestratorState::PowerCheck
        | OrchestratorState::Scanning { .. }
        | OrchestratorState::Planning { .. }
        | OrchestratorState::Executing { .. }
        | OrchestratorState::Verifying { .. }
        | OrchestratorState::Backoff { .. } => rust_i18n::t!("tray.tooltip.syncing").into_owned(),
        OrchestratorState::Paused { reason } => tooltip_for_pause(*reason),
        OrchestratorState::Error { detail } => tooltip_for_error(detail.code),
    }
}

fn tooltip_for_pause(reason: PauseReason) -> String {
    let key = match reason {
        PauseReason::Manual => "tray.tooltip.paused_manual",
        PauseReason::Battery => "tray.tooltip.paused_battery",
        PauseReason::Metered => "tray.tooltip.paused_metered",
        PauseReason::Offline => "tray.tooltip.offline",
        PauseReason::NoInternet => "tray.tooltip.no_internet",
        PauseReason::CaptivePortal => "tray.tooltip.captive_portal",
        PauseReason::DnsFailed => "tray.tooltip.dns_failed",
        PauseReason::ServiceDown => "tray.tooltip.service_down",
    };
    rust_i18n::t!(key).into_owned()
}

fn tooltip_for_error(code: ErrorCode) -> String {
    let key = match code {
        ErrorCode::NetOffline => "tray.tooltip.offline",
        ErrorCode::NetNoInternet => "tray.tooltip.no_internet",
        ErrorCode::NetCaptivePortal => "tray.tooltip.captive_portal",
        ErrorCode::NetDnsFailed => "tray.tooltip.dns_failed",
        ErrorCode::DriveUnreachable => "tray.tooltip.service_down",
        ErrorCode::AuthNetworkUnreachable => "tray.tooltip.offline",
        ErrorCode::AuthInvalidGrant | ErrorCode::AuthConsentRequired => "tray.tooltip.needs_reauth",
        _ => "tray.tooltip.error",
    };
    rust_i18n::t!(key).into_owned()
}

// -----------------------------------------------------------------------------
// Notification dedup state
// -----------------------------------------------------------------------------

/// Module-level notification dedup state (DESIGN s117/s247): fire the
/// first-sync-complete toast exactly once, and fire one error toast per
/// ENTRY into an error code (not once per `StateChanged` event - the
/// orchestrator broadcast can replay the current state after a `Lagged`).
struct NotifyState {
    /// True once a sync cycle has been observed running this process (so the
    /// next `Idle` transition is a genuine completion, not the boot `Idle`).
    saw_active_cycle: bool,
    /// True once the first-sync-complete toast has fired.
    first_sync_notified: bool,
    /// The error code last notified, to suppress repeat toasts while the app
    /// stays parked in the same error.
    last_error_code: Option<ErrorCode>,
}

impl NotifyState {
    const fn new() -> Self {
        Self {
            saw_active_cycle: false,
            first_sync_notified: false,
            last_error_code: None,
        }
    }
}

static NOTIFY: Mutex<NotifyState> = Mutex::new(NotifyState::new());

/// Lock the dedup state, recovering a poisoned lock (HARD RULE: no panic on
/// a poisoned mutex).
fn notify_state() -> std::sync::MutexGuard<'static, NotifyState> {
    NOTIFY.lock().unwrap_or_else(|e| e.into_inner())
}

// -----------------------------------------------------------------------------
// build
// -----------------------------------------------------------------------------

/// Build the tray icon + menu and register its handlers (SPEC s12).
///
/// Removes the auto-created `tauri.conf.json` tray (id `"main"`) first so it
/// does not coexist with our canonical `"driven-main"` tray, then builds the
/// DESIGN s8.1 flat menu wired to the M5 sync commands / window-show / quit.
/// Left-click opens the window (graceful no-op where it never fires on Linux);
/// `show_menu_on_left_click(false)` keeps left-click distinct from the menu.
pub fn build(app: &AppHandle) -> tauri::Result<()> {
    // Drop the config-defined tray so we don't show two icons. No-op (and a
    // debug log) if the config block was removed or used a different id.
    if app.remove_tray_by_id(CONFIG_TRAY_ID).is_none() {
        tracing::debug!(
            target: TARGET,
            "no config-defined tray (id {CONFIG_TRAY_ID}) to remove; only {TRAY_ID} will show"
        );
    }

    let menu = build_menu(app)?;
    let idle_icon = TrayIcon::Idle.image();

    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(idle_icon)
        .tooltip(rust_i18n::t!("app.name"))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| on_menu_event(app, event.id.as_ref()))
        .on_tray_icon_event(|tray, event| {
            // Linux DEs may never deliver this; the menu remains canonical.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    // Our generated icons are colour-bearing; the config sets the boot icon as
    // a template (macOS recolours templates monochrome, which would erase the
    // yellow/red distinction), so force template OFF on the live tray.
    if let Err(err) = tray.set_icon_as_template(false) {
        tracing::debug!(target: TARGET, "set_icon_as_template(false) failed: {err}");
    }

    Ok(())
}

/// Build the DESIGN s8.1 flat tray menu (every action is a menu item so Linux
/// users are never stuck on a non-firing left-click). `MenuBuilder` cleanly
/// mixes id'd items with separators without slice-coercion ambiguity.
fn build_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let item = |id: &str, label_key: &str| -> tauri::Result<MenuItem<tauri::Wry>> {
        MenuItem::with_id(app, id, rust_i18n::t!(label_key), true, None::<&str>)
    };

    MenuBuilder::new(app)
        .item(&item(menu_id::SYNC_NOW, "tray.sync_now")?)
        .item(&item(menu_id::PAUSE_30M, "tray.pause_30m")?)
        .item(&item(menu_id::RESUME, "tray.resume")?)
        .separator()
        .item(&item(menu_id::SETTINGS, "tray.settings")?)
        .item(&item(menu_id::ACTIVITY, "tray.activity")?)
        .item(&item(menu_id::RESTORE, "tray.restore")?)
        .separator()
        .item(&item(menu_id::QUIT, "tray.quit")?)
        .build()
}

/// Dispatch a tray menu click to the M5 sync commands / window show / quit
/// (SPEC s12). Async commands run on the Tauri runtime so the menu callback
/// returns immediately.
fn on_menu_event(app: &AppHandle, id: &str) {
    match id {
        menu_id::SYNC_NOW => spawn_command(app, |app| async move {
            let Some(state) = app.try_state::<crate::app_state::AppState>() else {
                return missing_state_err();
            };
            crate::commands::sync::sync_now(state, None).await
        }),
        menu_id::PAUSE_30M => spawn_command(app, |app| async move {
            let Some(state) = app.try_state::<crate::app_state::AppState>() else {
                return missing_state_err();
            };
            crate::commands::sync::pause_sync(state, Some(30 * 60)).await
        }),
        menu_id::RESUME => spawn_command(app, |app| async move {
            let Some(state) = app.try_state::<crate::app_state::AppState>() else {
                return missing_state_err();
            };
            crate::commands::sync::resume_sync(state).await
        }),
        menu_id::SETTINGS | menu_id::ACTIVITY | menu_id::RESTORE => {
            // Route selection (Settings/Activity/Restore) is an M6 frontend
            // concern; for M5 every item surfaces the main window and hints
            // the target route to the webview so M6 can deep-link.
            show_main_window(app);
            navigate_hint(app, id);
        }
        menu_id::QUIT => app.exit(0),
        other => tracing::warn!(target: TARGET, "unknown tray menu id: {other}"),
    }
}

/// The error a tray command returns when [`AppState`](crate::app_state::AppState)
/// is not managed yet (e.g. assembly is still running / failed). Returned
/// instead of panicking via `Manager::state` (HARD RULE: no panics).
fn missing_state_err() -> crate::commands::CommandResult<()> {
    Err(crate::commands::CommandError::new("app state not ready"))
}

/// Spawn an async sync command on the Tauri runtime from a tray callback,
/// logging any [`CommandError`](crate::commands::CommandError).
fn spawn_command<F, Fut>(app: &AppHandle, f: F)
where
    F: FnOnce(AppHandle) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = crate::commands::CommandResult<()>> + Send,
{
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(err) = f(app).await {
            tracing::warn!(target: TARGET, "tray command failed: {err:?}");
        }
    });
}

/// Show, unminimize, and focus the main window (the left-click + menu action).
/// Every step is best-effort: a missing window or a platform that rejects the
/// op is logged, never panicked (DESIGN s8.1 - menu is canonical, never stuck).
fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        tracing::warn!(target: TARGET, "main window not found; cannot surface it");
        return;
    };
    if let Err(err) = window.unminimize() {
        tracing::debug!(target: TARGET, "unminimize main window failed: {err}");
    }
    if let Err(err) = window.show() {
        tracing::warn!(target: TARGET, "show main window failed: {err}");
    }
    if let Err(err) = window.set_focus() {
        tracing::debug!(target: TARGET, "focus main window failed: {err}");
    }
}

/// Emit a lightweight `tray:navigate` hint carrying the target route so the
/// M6 frontend can route to Settings / Activity / Restore. Harmless if no
/// listener exists yet (M5 ships no router).
fn navigate_hint(app: &AppHandle, route: &str) {
    use tauri::Emitter;
    if let Err(err) = app.emit("tray:navigate", route) {
        tracing::debug!(target: TARGET, "tray:navigate emit failed: {err}");
    }
}

// -----------------------------------------------------------------------------
// apply_state + notifications
// -----------------------------------------------------------------------------

/// Swap the tray icon + tooltip to match `state` (DESIGN s8.1), called from
/// the orchestrator-event bridge on every [`OrchestratorState`] transition
/// (tray must update within 1s per ROADMAP M5 acceptance), and raise the
/// DESIGN s117/s247 OS notifications (first-sync-complete + red error).
///
/// Best-effort: a missing tray or a failed icon/tooltip set is logged, never
/// panicked. `apply_state` returns `()` (the committed signature) so all
/// errors are swallowed with a `tracing` line.
pub fn apply_state(app: &AppHandle, state: OrchestratorState) {
    let icon = TrayIcon::for_state(&state);

    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Err(err) = tray.set_icon(Some(icon.image())) {
            tracing::warn!(target: TARGET, "set tray icon failed: {err}");
        }
        // Generated tiles are colour-bearing; never let the OS recolour them.
        if let Err(err) = tray.set_icon_as_template(false) {
            tracing::debug!(target: TARGET, "set_icon_as_template(false) failed: {err}");
        }
        if let Err(err) = tray.set_tooltip(Some(tooltip_for(&state))) {
            tracing::warn!(target: TARGET, "set tray tooltip failed: {err}");
        }
    } else {
        tracing::warn!(target: TARGET, "tray {TRAY_ID} not found; cannot apply state");
    }

    notify_for_state(app, &state);
}

/// Raise the DESIGN s117/s247 OS notifications for a state transition.
///
/// Two triggers only (the rest of the state machine is icon+tooltip only):
/// - first-sync-complete: the first `Idle` reached after a real sync cycle;
/// - red error (`TrayIcon::Error`): a non-network error that needs attention,
///   deduped so it fires once per entry into a given error code. The reauth
///   case (`auth.invalid_grant` / `auth.consent_required`) is deliberately
///   skipped here - it is covered by [`notify_needs_reauth`], which the shell
///   calls with the account + email the state cannot carry.
fn notify_for_state(app: &AppHandle, state: &OrchestratorState) {
    match state {
        OrchestratorState::PowerCheck
        | OrchestratorState::Scanning { .. }
        | OrchestratorState::Planning { .. }
        | OrchestratorState::Executing { .. }
        | OrchestratorState::Verifying { .. }
        | OrchestratorState::Backoff { .. } => {
            // A cycle is underway; the next Idle is a genuine completion.
            let mut s = notify_state();
            s.saw_active_cycle = true;
            // Leaving any error state clears the dedup latch so a recurrence
            // notifies again.
            s.last_error_code = None;
        }
        OrchestratorState::Idle { .. } => {
            let mut s = notify_state();
            s.last_error_code = None;
            let should_fire = s.saw_active_cycle && !s.first_sync_notified;
            if should_fire {
                s.first_sync_notified = true;
                drop(s);
                show_notification(
                    app,
                    rust_i18n::t!("notifications.first_sync_complete.title").into_owned(),
                    rust_i18n::t!("notifications.first_sync_complete.body").into_owned(),
                );
            }
        }
        OrchestratorState::Paused { .. } => {
            // Pauses (battery / metered / network) are icon+tooltip only, no
            // toast on every blip (DESIGN s117/s247).
            let mut s = notify_state();
            s.last_error_code = None;
        }
        OrchestratorState::Error { detail } => {
            // Reauth is handled by notify_needs_reauth (needs account/email).
            if matches!(
                detail.code,
                ErrorCode::AuthInvalidGrant | ErrorCode::AuthConsentRequired
            ) {
                return;
            }
            // Only the red error visual toasts; yellow-bang network errors are
            // icon+tooltip only.
            if TrayIcon::for_state(state) != TrayIcon::Error {
                return;
            }
            let mut s = notify_state();
            if s.last_error_code == Some(detail.code) {
                return; // already toasted this error; suppress the replay
            }
            s.last_error_code = Some(detail.code);
            drop(s);
            let body = error_notification_body(detail.code);
            show_notification(
                app,
                rust_i18n::t!("notifications.error.title").into_owned(),
                body,
            );
        }
    }
}

/// The localised body line for a red-error notification, keyed off the stable
/// error code so each class gets a meaningful sentence; falls back to a
/// generic line for codes without a dedicated string.
fn error_notification_body(code: ErrorCode) -> String {
    let key = match code {
        ErrorCode::DriveQuotaExhausted => "notifications.error.drive_quota",
        ErrorCode::DriveDailyQuotaExhausted => "notifications.error.drive_daily_quota",
        ErrorCode::CryptoKeyMissing => "notifications.error.crypto_key_missing",
        ErrorCode::CryptoDecryptFailed => "notifications.error.crypto_decrypt_failed",
        ErrorCode::LocalDiskFull => "notifications.error.disk_full",
        ErrorCode::LocalVssUnavailable => "notifications.error.vss_unavailable",
        ErrorCode::StateDbCorrupt => "notifications.error.db_corrupt",
        _ => "notifications.error.generic",
    };
    rust_i18n::t!(key, code = code.code()).into_owned()
}

/// Raise the DESIGN s117 "needs sign-in again" notification for an account
/// whose refresh token was revoked (CODEX_NOTES V-F: the M5 shell performs
/// this transition). MUST be called by the shell's reauth bridge at the same
/// point it calls `events::emit_account_needs_reauth`; the `OrchestratorState`
/// alone cannot carry the `account` email, so `apply_state` cannot fire it.
pub fn notify_needs_reauth(app: &AppHandle, account: &str) {
    show_notification(
        app,
        rust_i18n::t!("notifications.needs_reauth.title").into_owned(),
        rust_i18n::t!("notifications.needs_reauth.body", account = account).into_owned(),
    );
}

/// Set the tray to the "Suspending..." visual (DESIGN s5.10.2): yellow icon +
/// suspend tooltip. Driven by [`PowerEvent::Suspending`](driven_core::types::PowerEvent),
/// which is NOT an [`OrchestratorState`], so the shell's power bridge MUST call
/// this on suspend (and call [`apply_state`] again on resume to restore the
/// live state's icon).
///
/// Uncalled in M5: the suspend/resume EDGE `PowerEvent` (DESIGN s5.10) arrives
/// on an OS message-pump seam (`WM_POWERBROADCAST`) that `driven-power` does not
/// yet expose, so there is no event source to bridge into this. The visual is
/// implemented and ready; allow dead_code until that power-edge seam exists.
#[allow(dead_code)]
pub fn apply_suspending(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        tracing::warn!(target: TARGET, "tray {TRAY_ID} not found; cannot show suspending");
        return;
    };
    if let Err(err) = tray.set_icon(Some(TrayIcon::Paused.image())) {
        tracing::warn!(target: TARGET, "set suspending icon failed: {err}");
    }
    if let Err(err) = tray.set_icon_as_template(false) {
        tracing::debug!(target: TARGET, "set_icon_as_template(false) failed: {err}");
    }
    if let Err(err) = tray.set_tooltip(Some(rust_i18n::t!("tray.tooltip.suspending").into_owned()))
    {
        tracing::warn!(target: TARGET, "set suspending tooltip failed: {err}");
    }
}

/// Show an OS notification via `tauri-plugin-notification`, logging on failure
/// (a denied/unavailable notification permission must never crash the tray).
fn show_notification(app: &AppHandle, title: String, body: String) {
    let result = app.notification().builder().title(title).body(body).show();
    if let Err(err) = result {
        tracing::warn!(target: TARGET, "OS notification failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_core::types::{ErrorDetail, ExecProgress, PlanSummary};

    fn err_state(code: ErrorCode) -> OrchestratorState {
        OrchestratorState::Error {
            detail: ErrorDetail::new(code, "test"),
        }
    }

    #[test]
    fn idle_maps_to_idle() {
        assert_eq!(
            TrayIcon::for_state(&OrchestratorState::Idle { last_run_at: None }),
            TrayIcon::Idle
        );
    }

    #[test]
    fn working_states_map_to_syncing() {
        let working = [
            OrchestratorState::PowerCheck,
            OrchestratorState::Scanning {
                source_id: driven_core::types::SourceId::new_v4(),
                scanned: 0,
            },
            OrchestratorState::Planning {
                plan: PlanSummary::default(),
            },
            OrchestratorState::Executing {
                progress: ExecProgress::zero(),
            },
            OrchestratorState::Verifying {
                sampled: 0,
                mismatches: 0,
            },
            OrchestratorState::Backoff { until: 0 },
        ];
        for s in working {
            assert_eq!(TrayIcon::for_state(&s), TrayIcon::Syncing, "{s:?}");
        }
    }

    #[test]
    fn manual_battery_metered_pause_is_yellow_not_bang() {
        for reason in [
            PauseReason::Manual,
            PauseReason::Battery,
            PauseReason::Metered,
        ] {
            assert_eq!(
                TrayIcon::for_state(&OrchestratorState::Paused { reason }),
                TrayIcon::Paused,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn network_pause_is_network_attention() {
        for reason in [
            PauseReason::Offline,
            PauseReason::NoInternet,
            PauseReason::CaptivePortal,
            PauseReason::DnsFailed,
            PauseReason::ServiceDown,
        ] {
            assert_eq!(
                TrayIcon::for_state(&OrchestratorState::Paused { reason }),
                TrayIcon::NetworkAttention,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn network_error_codes_are_network_attention() {
        for code in [
            ErrorCode::NetOffline,
            ErrorCode::NetNoInternet,
            ErrorCode::NetDnsFailed,
            ErrorCode::NetCaptivePortal,
            ErrorCode::NetTimeout,
            ErrorCode::NetIntermittent,
            ErrorCode::NetProxyRequired,
            ErrorCode::DriveUnreachable,
            ErrorCode::AuthNetworkUnreachable,
            ErrorCode::UpdateEndpointUnreachable,
        ] {
            assert_eq!(
                TrayIcon::for_state(&err_state(code)),
                TrayIcon::NetworkAttention,
                "{code:?}"
            );
        }
    }

    #[test]
    fn non_network_error_codes_are_red_error() {
        for code in [
            ErrorCode::AuthInvalidGrant,
            ErrorCode::AuthConsentRequired,
            ErrorCode::DriveQuotaExhausted,
            ErrorCode::CryptoKeyMissing,
            ErrorCode::CryptoDecryptFailed,
            ErrorCode::LocalDiskFull,
            ErrorCode::LocalVssUnavailable,
            ErrorCode::StateDbCorrupt,
            ErrorCode::InternalBug,
        ] {
            assert_eq!(
                TrayIcon::for_state(&err_state(code)),
                TrayIcon::Error,
                "{code:?}"
            );
        }
    }

    #[test]
    fn every_state_has_a_nonempty_tooltip() {
        let states = [
            OrchestratorState::Idle { last_run_at: None },
            OrchestratorState::PowerCheck,
            OrchestratorState::Paused {
                reason: PauseReason::Battery,
            },
            OrchestratorState::Paused {
                reason: PauseReason::Offline,
            },
            err_state(ErrorCode::CryptoKeyMissing),
            err_state(ErrorCode::NetCaptivePortal),
            err_state(ErrorCode::AuthInvalidGrant),
        ];
        for s in states {
            assert!(!tooltip_for(&s).is_empty(), "{s:?}");
        }
    }

    #[test]
    fn generated_icon_tile_has_expected_dimensions() {
        for icon in [
            TrayIcon::Idle,
            TrayIcon::Syncing,
            TrayIcon::Paused,
            TrayIcon::NetworkAttention,
            TrayIcon::Error,
        ] {
            let buf = icon.rgba_buffer();
            // TILE x TILE pixels, 4 bytes (RGBA) each.
            assert_eq!(buf.len(), (TILE * TILE * 4) as usize, "{icon:?}");
            // Every pixel carries this state's exact colour (solid tile).
            let expected = icon.rgba();
            assert!(
                buf.chunks_exact(4).all(|px| px == expected),
                "{icon:?} tile is not a solid fill"
            );
        }
    }

    #[test]
    fn each_state_icon_has_a_distinct_colour() {
        let colours = [
            TrayIcon::Idle.rgba(),
            TrayIcon::Syncing.rgba(),
            TrayIcon::Paused.rgba(),
            TrayIcon::NetworkAttention.rgba(),
            TrayIcon::Error.rgba(),
        ];
        for i in 0..colours.len() {
            for j in (i + 1)..colours.len() {
                assert_ne!(colours[i], colours[j], "icons {i} and {j} share a colour");
            }
        }
    }
}
