//! Settings & misc IPC commands (SPEC s11.6).
//!
//! The Rules + About settings tabs (DESIGN s8.2) drive these. Each is a
//! `#[tauri::command]` over `State<AppState>`.
//!
//! IPC path safety (SPEC s11.6.1): `export_diagnostic_bundle` takes a
//! destination `PathBuf` from the (untrusted) webview and validates it via
//! [`crate::commands::validate_writable_dest`] (dialog-derived, confined, no
//! traversal, no symlink-at-leaf, atomic write) before writing the ZIP.
//!
//! ## Updates: REAL GitHub-releases backend (SPEC s11.6 / s15, ROADMAP M6/M9)
//!
//! ROADMAP M9 owns the Tauri `update.json` manifest hosting
//! (`driven.maxhogan.dev/updates`) + the `tauri-plugin-updater` install/relaunch
//! path - that endpoint does NOT exist in M6 (M9's sequencing note: "the in-app
//! updater needs a real `update.json` to fetch, which only exists once the
//! release pipeline is in place"). So Driven does NOT query that manifest here.
//!
//! Instead BOTH [`check_for_updates`] and [`list_releases`] hit the GitHub
//! releases API for the Driven repo - which is real and reachable today - and
//! map results to the frozen DTOs. The active channel ([`UpdaterSettings`]
//! `channel`) selects `stable` (skip pre-releases) vs `dev` (include
//! pre-releases). `check_for_updates` compares the newest channel release's
//! semver tag against the running build's version and returns
//! `Some(UpdateInfo)` only when the remote is strictly newer. This is the honest
//! "is there a newer release" answer the About tab needs without the M9 manifest
//! + signed-bundle download path (which stays M9). No `todo!()` / panic / fake.

use std::path::PathBuf;

use serde::Deserialize;
use tauri::{AppHandle, State};

use driven_core::orchestrator::{MeteredMode, OrchestratorConfig};
use driven_core::state::StateRepo;
use driven_core::types::ErrorCode;

use driven_vss::VssMode;

use crate::app_state::AppState;
use crate::commands::dtos::{
    CustomCaValidation, GlobalSettings, ReleaseDto, ScheduleSettings, SettingsDto, SettingsPatch,
    TelemetrySettings, UiSettings, UpdateInfo, UpdaterSettings, VssHelperStatus, WindowsSettings,
};
use crate::commands::{
    atomic_write, validate_writable_dest, CommandError, CommandResult, DialogToken,
};

/// Tracing target for the settings command layer.
const TARGET: &str = "driven::app::settings";

/// SPEC s22 settings KV keys.
const KEY_GLOBAL: &str = "global";
const KEY_TELEMETRY: &str = "telemetry";
const KEY_UPDATER: &str = "updater";
const KEY_UI: &str = "ui";
/// Windows-only KV group (SPEC s22): absent on macOS / Linux.
const KEY_WINDOWS: &str = "windows";

/// The GitHub `owner/repo` slug whose releases drive the About tab + the update
/// check (workspace `repository` = `https://github.com/pmaxhogan/driven`).
const GITHUB_REPO: &str = "pmaxhogan/driven";

/// Releases page size for [`list_releases`] (the About tab paginates).
const RELEASES_PER_PAGE: u32 = 10;

/// `User-Agent` for the GitHub API (GitHub rejects requests with no UA).
const GITHUB_USER_AGENT: &str = concat!("driven-app/", env!("CARGO_PKG_VERSION"));

// ---------------------------------------------------------------------------
// get_settings
// ---------------------------------------------------------------------------

/// `get_settings()` - the full settings snapshot (SPEC s11.6).
///
/// Reads the SPEC s22 KV groups (`global`, `telemetry`, `updater`, `ui`, and
/// `windows` on Windows) from the `settings` table into one [`SettingsDto`]. A
/// group absent from the table (e.g. a fresh DB before a seed, or `windows` on a
/// non-Windows host) falls back to the SPEC s22 defaults so the UI always has a
/// complete document to render.
#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> CommandResult<SettingsDto> {
    load_settings_dto(state.state().as_ref()).await
}

// ---------------------------------------------------------------------------
// get_vss_helper_status (DESIGN s5.3.1, issue #25)
// ---------------------------------------------------------------------------

/// Report whether least-privilege locked-file backup is available or degraded,
/// for the Settings banner. Truthful on every platform: off Windows VSS is
/// unsupported (never degraded); on Windows, locked-file backup is DEGRADED when
/// the app is not elevated and no least-privilege helper is active (the current
/// behaviour), so the banner can explain why locked files are being skipped.
#[tauri::command]
pub async fn get_vss_helper_status(state: State<'_, AppState>) -> CommandResult<VssHelperStatus> {
    let helper_enabled = load_vss_helper_enabled(state.state().as_ref()).await;
    // Issue #25: consult the app-side broker manager for TRUTHFUL state.
    // `helper_alive` = the broker is up + serving; `helper_launchable` = it is up /
    // coming up / not-yet-tried (so locked-file backup is available on demand);
    // `launch_pending` = a launch is awaiting elevation approval / pipe (the UI
    // shows a "waiting for approval" hint); `launch_declined` = the user declined.
    let (helper_alive, helper_launchable, launch_pending, launch_declined) =
        match state.vss_helper_manager() {
            Some(manager) => (
                manager.helper_alive(),
                manager.helper_launchable(),
                manager.launch_pending(),
                manager.launch_declined(),
            ),
            None => (false, false, false, false),
        };
    Ok(compute_vss_helper_status(
        cfg!(windows),
        driven_vss::is_elevated(),
        helper_enabled,
        helper_alive,
        helper_launchable,
        launch_pending,
        launch_declined,
    ))
}

/// Read the `windows.vss_helper` setting (SPEC s22): whether the user opted into
/// the least-privilege elevated helper. `false` off Windows, or on any read/parse
/// error (best-effort, like the cold-start config load). Shared by the boot
/// assembly (which decides whether to build the broker manager) and the status
/// command.
pub async fn load_vss_helper_enabled(state: &dyn StateRepo) -> bool {
    if !cfg!(windows) {
        return false;
    }
    load_group::<storage::Windows>(state, KEY_WINDOWS)
        .await
        .ok()
        .flatten()
        .map(|w| w.vss_helper)
        .unwrap_or(false)
}

/// Pure status derivation (unit-tested off Windows). Locked-file backup is
/// degraded only where VSS is supported (Windows) and NONE of the ways to create
/// a snapshot apply: the app is not elevated (in-process VSS) AND the
/// least-privilege helper is neither already launched (`helper_alive`) nor
/// launchable on demand (`helper_launchable`). When the helper is launchable the
/// banner is NOT degraded - the broker comes up on the first locked file.
#[allow(clippy::too_many_arguments)]
fn compute_vss_helper_status(
    supported: bool,
    elevated: bool,
    helper_enabled: bool,
    helper_alive: bool,
    helper_launchable: bool,
    launch_pending: bool,
    launch_declined: bool,
) -> VssHelperStatus {
    let locked_file_backup_degraded =
        supported && !elevated && !(helper_alive || helper_launchable);
    VssHelperStatus {
        supported,
        elevated,
        helper_enabled,
        helper_alive,
        helper_launchable,
        launch_pending,
        launch_declined,
        locked_file_backup_degraded,
    }
}

/// Read every SPEC s22 KV group into a [`SettingsDto`] (shared by `get_settings`
/// and by `update_settings`'s read-back). A malformed stored value surfaces a
/// typed `state.db_corrupt` error rather than a panic.
async fn load_settings_dto(state: &dyn StateRepo) -> CommandResult<SettingsDto> {
    let global = load_group::<storage::Global>(state, KEY_GLOBAL)
        .await?
        .map(Into::into)
        .unwrap_or_else(default_global);
    let telemetry = load_group::<storage::Telemetry>(state, KEY_TELEMETRY)
        .await?
        .map(Into::into)
        .unwrap_or_else(default_telemetry);
    let updater = load_group::<storage::Updater>(state, KEY_UPDATER)
        .await?
        .map(Into::into)
        .unwrap_or_else(default_updater);
    let ui = load_group::<storage::Ui>(state, KEY_UI)
        .await?
        .map(Into::into)
        .unwrap_or_else(default_ui);

    // The `windows` group is Windows-only (SPEC s22): present on Windows
    // (defaulting to `auto` if unseeded), `None` elsewhere so the DTO honestly
    // reflects the platform.
    let windows = if cfg!(windows) {
        Some(
            load_group::<storage::Windows>(state, KEY_WINDOWS)
                .await?
                .map(Into::into)
                .unwrap_or_else(default_windows),
        )
    } else {
        None
    };

    // V2 small-file bundling toggle (issue #35 item d): a standalone advanced
    // setting backed by the `bundle_small_files` KV key the core planner reads
    // directly (NOT a group blob field). Absent/malformed reads as `false`.
    let bundle_small_files = state
        .get_setting(driven_core::planner::SETTING_BUNDLE_ENABLED)
        .await
        .map_err(CommandError::from)?
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    Ok(SettingsDto {
        global,
        telemetry,
        updater,
        ui,
        windows,
        bundle_small_files,
    })
}

/// Read one settings KV group as its `snake_case` STORAGE struct `S` (see
/// [`storage`]) - the on-disk format the migration 0002 seed writes.
///
/// The DB stores each group in `snake_case`; the wire/DTO groups are
/// `camelCase`, so callers convert the returned storage struct to the DTO group
/// via `S: Into<..DtoGroup>`. A JSON parse failure maps to `state.db_corrupt` (a
/// stored value that no longer matches the schema).
async fn load_group<S>(state: &dyn StateRepo, key: &str) -> CommandResult<Option<S>>
where
    S: serde::de::DeserializeOwned,
{
    let Some(value) = state.get_setting(key).await.map_err(CommandError::from)? else {
        return Ok(None);
    };
    serde_json::from_value::<S>(value).map(Some).map_err(|e| {
        CommandError::with_code(
            ErrorCode::StateDbCorrupt,
            format!("settings `{key}` is malformed: {e}"),
        )
    })
}

// ---------------------------------------------------------------------------
// update_settings
// ---------------------------------------------------------------------------

/// `update_settings(patch)` - apply a settings patch (SPEC s11.6).
///
/// Merges the present [`SettingsPatch`] groups/fields into the stored settings,
/// persists each touched group, applies the real side effects, and returns the
/// updated [`SettingsDto`].
///
/// Side effects (DESIGN s8.2, SPEC s13/s22):
/// - `global.auto_start_on_login` toggles the OS autostart registration via the
///   autostart plugin (`enable`/`disable`);
/// - `global.log_level` reconfigures the live `tracing` max level;
/// - `ui.locale` re-renders the tray menu/tooltip in the new locale AND notifies
///   the frontend (a `settings:locale_changed` event) so vue-i18n re-renders;
/// - any orchestrator-affecting `global` / `windows` field (battery/metered gate,
///   scan interval, bandwidth cap, VSS mode) reconfigures EVERY running account's
///   orchestrator so the change takes effect on the next cycle without a restart.
#[tauri::command]
pub async fn update_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    patch: SettingsPatch,
) -> CommandResult<SettingsDto> {
    let repo = state.state().as_ref();

    // Track which side effects the patch requires so they fire exactly once
    // after persistence (and only when the relevant field actually changed).
    let mut autostart_target: Option<bool> = None;
    let mut new_log_level: Option<String> = None;
    let mut new_locale: Option<String> = None;
    let mut orchestrator_affecting = false;
    // Issue #25: the `windows.vss_helper` toggle transition (Some(new) iff it
    // actually changed), applied after persistence to (dis)arm + eagerly launch
    // the least-privilege helper broker.
    let mut vss_helper_target: Option<bool> = None;

    // --- global group -------------------------------------------------------
    if let Some(g) = patch.global {
        let mut cur: GlobalSettings = load_group::<storage::Global>(repo, KEY_GLOBAL)
            .await?
            .map(Into::into)
            .unwrap_or_else(default_global);
        if let Some(v) = g.auto_start_on_login {
            if v != cur.auto_start_on_login {
                autostart_target = Some(v);
            }
            cur.auto_start_on_login = v;
        }
        if let Some(v) = g.default_concurrent_uploads {
            // `Some(None)` = reset to auto (valid); `Some(Some(n))` = override in
            // the SPEC s22 1..=32 range.
            if let Some(n) = v {
                check_range(
                    "default_concurrent_uploads",
                    n,
                    CONCURRENCY_MIN,
                    CONCURRENCY_MAX,
                )?;
            }
            cur.default_concurrent_uploads = v;
        }
        if let Some(v) = g.adaptive_parallelism_enabled {
            cur.adaptive_parallelism_enabled = v;
        }
        if let Some(v) = g.bandwidth_cap_mbps {
            // `None` = unlimited (valid); `Some(n)` must be in range.
            if let Some(n) = v {
                check_range(
                    "bandwidth_cap_mbps",
                    n,
                    BANDWIDTH_CAP_MIN,
                    BANDWIDTH_CAP_MAX,
                )?;
            }
            cur.bandwidth_cap_mbps = v;
            orchestrator_affecting = true;
        }
        if let Some(v) = g.skip_on_battery {
            cur.skip_on_battery = v;
            orchestrator_affecting = true;
        }
        if let Some(v) = g.skip_on_metered {
            cur.skip_on_metered = v;
            orchestrator_affecting = true;
        }
        if let Some(v) = g.scan_interval_secs {
            check_range(
                "scan_interval_secs",
                v,
                SCAN_INTERVAL_MIN,
                SCAN_INTERVAL_MAX,
            )?;
            cur.scan_interval_secs = v;
            orchestrator_affecting = true;
        }
        if let Some(v) = g.deep_verify_interval_secs {
            check_range(
                "deep_verify_interval_secs",
                v,
                DEEP_VERIFY_MIN,
                DEEP_VERIFY_MAX,
            )?;
            cur.deep_verify_interval_secs = v;
            // Per-source cadence feeds the orchestrator's deep-verify schedule.
            orchestrator_affecting = true;
        }
        if let Some(v) = g.io_priority {
            check_enum("io_priority", &v, IO_PRIORITIES)?;
            cur.io_priority = v;
        }
        if let Some(v) = g.log_level {
            check_enum("log_level", &v, LOG_LEVELS)?;
            if v != cur.log_level {
                new_log_level = Some(v.clone());
            }
            cur.log_level = v;
        }
        if let Some(v) = g.schedule {
            // Schedule window (DESIGN s17): bounds are local minutes 0..=1439
            // (an end of 0 means midnight, which wraps a same-evening start).
            check_range("schedule.start_minute", v.start_minute, 0, 1439)?;
            check_range("schedule.end_minute", v.end_minute, 0, 1439)?;
            cur.schedule = v;
            orchestrator_affecting = true;
        }
        if let Some(v) = g.pre_backup_hook {
            // A blank command clears the hook.
            cur.pre_backup_hook = v.and_then(|s| {
                let t = s.trim().to_string();
                (!t.is_empty()).then_some(t)
            });
            orchestrator_affecting = true;
        }
        if let Some(v) = g.post_backup_hook {
            cur.post_backup_hook = v.and_then(|s| {
                let t = s.trim().to_string();
                (!t.is_empty()).then_some(t)
            });
            orchestrator_affecting = true;
        }
        if let Some(v) = g.hook_timeout_secs {
            check_range("hook_timeout_secs", v, 1, 86_400)?;
            cur.hook_timeout_secs = v;
            orchestrator_affecting = true;
        }
        if let Some(v) = g.metered_mode {
            check_enum("metered_mode", &v, METERED_MODES)?;
            cur.metered_mode = v;
            orchestrator_affecting = true;
        }
        if let Some(v) = g.metered_bandwidth_cap_mbps {
            if let Some(n) = v {
                check_range(
                    "metered_bandwidth_cap_mbps",
                    n,
                    BANDWIDTH_CAP_MIN,
                    BANDWIDTH_CAP_MAX,
                )?;
            }
            cur.metered_bandwidth_cap_mbps = v;
            orchestrator_affecting = true;
        }
        if let Some(v) = g.custom_root_ca_path {
            // Issue #34: normalise a blank path to `None` (system trust only),
            // and VALIDATE a real path parses as a PEM cert bundle up front so a
            // broken CA can never be persisted (which would then fail-closed
            // every outbound client). The trust is only ever ADDED on top of the
            // OS roots - this validation neither weakens nor bypasses TLS.
            let normalized = normalize_ca_path(v);
            if let Some(path) = &normalized {
                let count = driven_tls::validate_ca_file(path).map_err(|e| {
                    invalid_setting(format!("custom root CA file is not usable: {e}"))
                })?;
                tracing::info!(
                    target: TARGET,
                    certs = count,
                    "custom root CA validated on save"
                );
            }
            cur.custom_root_ca_path = normalized;
            // NOTE: applied to NEW client builds (restart / account re-add); the
            // long-lived Drive/network clients are not rebuilt in place here.
        }
        store_group(repo, KEY_GLOBAL, &storage::Global::from(cur)).await?;
    }

    // --- telemetry group ----------------------------------------------------
    // R2-P1-1 + R2-P2-1: route an `enabled` change through telemetry.rs's SINGLE
    // cancel-preserving path so EVERY renderer path (this generic update_settings
    // AND the dedicated set_telemetry_enabled) flips the in-flight cancel flag and
    // honors opt-out IMMEDIATELY. `apply_enabled_change` commits an ATOMIC,
    // COMMUTING field-level patch that mutates ONLY `enabled` (R3-P1-1: a SQLite
    // `json_set` so a racing `last_sent_at` write can never resurrect a stale
    // flag), keeping `install_id`, `endpoint`, AND the `last_sent_at` delta
    // checkpoint intact, AND coordinates via the shared send-admission gate
    // (R3-P1-2) so no post-disable send is admitted. A telemetry patch with no
    // `enabled` field is a no-op (nothing else in the group is user-writable here).
    if let Some(t) = patch.telemetry {
        if let Some(v) = t.enabled {
            let cancel = state.telemetry_cancel();
            let gate = state.telemetry_send_gate();
            let latency = state.telemetry_latency();
            crate::telemetry::apply_enabled_change(repo, &cancel, &gate, latency.as_ref(), v)
                .await?;
        }
    }

    // --- updater group ------------------------------------------------------
    if let Some(u) = patch.updater {
        let mut cur: UpdaterSettings = load_group::<storage::Updater>(repo, KEY_UPDATER)
            .await?
            .map(Into::into)
            .unwrap_or_else(default_updater);
        if let Some(v) = u.channel {
            check_enum("channel", &v, UPDATE_CHANNELS)?;
            cur.channel = v;
        }
        if let Some(v) = u.check_interval_secs {
            check_range("check_interval_secs", v, UPDATE_CHECK_MIN, UPDATE_CHECK_MAX)?;
            cur.check_interval_secs = v;
        }
        store_group(repo, KEY_UPDATER, &storage::Updater::from(cur)).await?;
    }

    // --- ui group -----------------------------------------------------------
    if let Some(ui) = patch.ui {
        let mut cur: UiSettings = load_group::<storage::Ui>(repo, KEY_UI)
            .await?
            .map(Into::into)
            .unwrap_or_else(default_ui);
        if let Some(v) = ui.tray_left_click_opens {
            check_enum("tray_left_click_opens", &v, TRAY_TARGETS)?;
            cur.tray_left_click_opens = v;
        }
        if let Some(v) = ui.locale {
            check_locale(&v)?;
            if v != cur.locale {
                new_locale = Some(v.clone());
            }
            cur.locale = v;
        }
        if let Some(v) = ui.color_mode {
            check_enum("color_mode", &v, COLOR_MODES)?;
            cur.color_mode = v;
        }
        store_group(repo, KEY_UI, &storage::Ui::from(cur)).await?;
    }

    // --- windows group (Windows-only) ---------------------------------------
    if let Some(w) = patch.windows {
        // Persist the patch on every host so a settings DB synced between
        // machines keeps the value; the field only has a runtime effect on
        // Windows (the VSS provider), where it reconfigures the orchestrator.
        let mut cur: WindowsSettings = load_group::<storage::Windows>(repo, KEY_WINDOWS)
            .await?
            .map(Into::into)
            .unwrap_or_else(default_windows);
        if let Some(v) = w.vss_mode {
            check_enum("vss_mode", &v, VSS_MODES)?;
            cur.vss_mode = v;
            if cfg!(windows) {
                orchestrator_affecting = true;
            }
        }
        if let Some(v) = w.vss_helper {
            // Issue #25: on a real change, (dis)arm the shared broker manager
            // AFTER persistence. Enabling fires the ATTENDED eager launch (the
            // user is at the Settings screen to approve the one UAC prompt);
            // disabling shuts the broker down. The already-wired providers pick
            // this up live via the launcher - no orchestrator rebuild needed.
            if v != cur.vss_helper {
                vss_helper_target = Some(v);
            }
            cur.vss_helper = v;
        }
        store_group(repo, KEY_WINDOWS, &storage::Windows::from(cur)).await?;
    }

    // --- bundling toggle (issue #35 item d) ---------------------------------
    // A standalone advanced setting, not a group blob: write the
    // `bundle_small_files` KV key the core planner reads directly. The change
    // takes effect on the next scan cycle (the planner re-reads it per run), so
    // no orchestrator reconfigure is needed.
    if let Some(v) = patch.bundle_small_files {
        repo.set_setting(
            driven_core::planner::SETTING_BUNDLE_ENABLED,
            &serde_json::Value::Bool(v),
        )
        .await
        .map_err(CommandError::from)?;
    }

    // --- side effects (after persistence) -----------------------------------

    // Autostart register / unregister (SPEC s13). A plugin error is surfaced as
    // a typed command error: the user toggled it, so a silent failure would lie.
    if let Some(enable) = autostart_target {
        apply_autostart(&app, enable)?;
    }

    // Live log-level change (SPEC s22 `global.log_level`).
    if let Some(level) = &new_log_level {
        apply_log_level(level);
    }

    // Locale change: re-render the tray (Rust-side i18n) + notify the frontend
    // so vue-i18n re-renders (DESIGN s8.7).
    if let Some(locale) = &new_locale {
        apply_locale(&app, locale);
    }

    // Reconfigure every running orchestrator so a changed gate / cadence / cap /
    // VSS mode takes effect on the next cycle without a restart (DESIGN s5.7).
    if orchestrator_affecting {
        reconfigure_all(&state).await;
    }

    // Issue #25: (dis)arm + eagerly launch the least-privilege helper broker on a
    // `windows.vss_helper` change. Enabling fires the attended UAC prompt NOW
    // (the user is present); disabling shuts the broker down. Best-effort: no
    // manager (off Windows / elevated) is a no-op.
    if let Some(enabled) = vss_helper_target {
        if let Some(manager) = state.vss_helper_manager() {
            manager.set_enabled(enabled);
            if enabled {
                // Eager, non-blocking: the launch runs on a background thread so
                // this IPC returns at once; the UI polls get_vss_helper_status to
                // show pending -> ready/declined.
                manager.launch_now();
            }
        }
    }

    // Return the full, freshly-stored settings document.
    load_settings_dto(repo).await
}

// ---------------------------------------------------------------------------
// R2-P2-3: backend settings validation (SPEC s22 limits + enums)
// ---------------------------------------------------------------------------
//
// The settings IPC accepts numeric + enum values from the (untrusted) renderer.
// A buggy or compromised UI could persist a zero/huge scan interval, an invalid
// log level / channel / locale / VSS mode, etc. These validators run BEFORE
// `store_group`, rejecting an out-of-range / invalid value with the stable
// `internal.invalid_input` SPEC s24 code so nothing bad reaches the DB.

/// Min/max scan cadence (seconds): 30s..7 days. The SPEC s22 default is 600.
const SCAN_INTERVAL_MIN: u32 = 30;
const SCAN_INTERVAL_MAX: u32 = 604_800;
/// Min/max deep-verify cadence (seconds): 1 hour .. 1 year (default 7 days).
/// `pub(crate)` so the per-source `update_source` IPC (sources.rs) validates a
/// patched `deep_verify_interval_secs` against the SAME duration cap (R3-P2-2).
pub(crate) const DEEP_VERIFY_MIN: u32 = 3_600;
pub(crate) const DEEP_VERIFY_MAX: u32 = 31_536_000;
/// Min/max bandwidth cap (Mbps) when set (`None` = unlimited): 1 .. 100_000.
const BANDWIDTH_CAP_MIN: u32 = 1;
const BANDWIDTH_CAP_MAX: u32 = 100_000;
/// Concurrency override range when set (SPEC s22: user may override 1..=32).
const CONCURRENCY_MIN: u32 = 1;
const CONCURRENCY_MAX: u32 = 32;
/// Min/max update-check cadence (seconds): 5 minutes .. 7 days.
const UPDATE_CHECK_MIN: u32 = 300;
const UPDATE_CHECK_MAX: u32 = 604_800;

/// Valid `io_priority` values (SPEC s22).
const IO_PRIORITIES: &[&str] = &["normal", "low", "idle"];
/// V2 metered pause-or-throttle modes (DESIGN s17).
const METERED_MODES: &[&str] = &["pause", "throttle"];
/// Valid `log_level` values (the `tracing` levels).
const LOG_LEVELS: &[&str] = &["error", "warn", "info", "debug", "trace"];
/// Valid updater `channel` values (SPEC s22).
const UPDATE_CHANNELS: &[&str] = &["stable", "dev"];
/// Valid `color_mode` values (SPEC s22).
const COLOR_MODES: &[&str] = &["system", "light", "dark"];
/// Valid `tray_left_click_opens` targets (the tray's window routes).
const TRAY_TARGETS: &[&str] = &["activity", "settings", "restore"];
/// Valid `vss_mode` values (SPEC s22, Windows-only).
const VSS_MODES: &[&str] = &["auto", "always", "never"];

/// An invalid settings value -> the stable `internal.invalid_input` SPEC s24
/// error (so the webview shows a "check your input" message).
fn invalid_setting(msg: impl Into<String>) -> CommandError {
    CommandError::with_code(ErrorCode::InvalidInput, msg.into())
}

/// Bound a numeric setting to `min..=max`, else reject (R2-P2-3).
fn check_range(field: &str, value: u32, min: u32, max: u32) -> CommandResult<()> {
    if value < min || value > max {
        return Err(invalid_setting(format!(
            "{field} must be between {min} and {max} (got {value})"
        )));
    }
    Ok(())
}

/// R3-P2-2: validate a per-source `deep_verify_interval_secs` against the SAME
/// `DEEP_VERIFY_MIN..=DEEP_VERIFY_MAX` bound the global settings validator uses,
/// returning the stable `internal.invalid_input` SPEC s24 code. Shared with
/// `update_source` (sources.rs) so a direct IPC patch cannot set `0` (constant
/// deep-verify churn) or `u32::MAX` (suppress deep verify for decades).
pub(crate) fn validate_deep_verify_interval(value: u32) -> CommandResult<()> {
    check_range(
        "deep_verify_interval_secs",
        value,
        DEEP_VERIFY_MIN,
        DEEP_VERIFY_MAX,
    )
}

/// Require `value` to be one of `allowed`, else reject (R2-P2-3).
fn check_enum(field: &str, value: &str, allowed: &[&str]) -> CommandResult<()> {
    if !allowed.contains(&value) {
        return Err(invalid_setting(format!(
            "{field} must be one of [{}] (got `{value}`)",
            allowed.join(", ")
        )));
    }
    Ok(())
}

/// Validate a locale tag (R2-P2-3): non-empty, ASCII, and BCP-47-shaped
/// (alphanumeric subtags separated by single hyphens, e.g. `en`, `en-US`,
/// `zh-Hant-TW`). Rejects a malformed / injected locale string without
/// hard-coding the (currently single) bundled locale set, so a future locale
/// does not require a code change while a garbage value is still rejected.
fn check_locale(value: &str) -> CommandResult<()> {
    let ok = !value.is_empty()
        && value.len() <= 35
        && value
            .split('-')
            .all(|sub| !sub.is_empty() && sub.chars().all(|c| c.is_ascii_alphanumeric()));
    if !ok {
        return Err(invalid_setting(format!(
            "locale must be a well-formed BCP-47 tag (got `{value}`)"
        )));
    }
    Ok(())
}

/// Serialize + persist one settings KV group, mapping a serialization failure to
/// `internal.bug` (a DTO that cannot serialize is a programming error).
async fn store_group<T: serde::Serialize>(
    state: &dyn StateRepo,
    key: &str,
    value: &T,
) -> CommandResult<()> {
    let v = serde_json::to_value(value).map_err(|e| {
        CommandError::with_code(
            ErrorCode::InternalBug,
            format!("serialize settings `{key}`: {e}"),
        )
    })?;
    state.set_setting(key, &v).await.map_err(CommandError::from)
}

/// On-disk (`snake_case`) STORAGE structs for the settings KV groups.
///
/// The migration 0002 seed writes each group in `snake_case` (e.g.
/// `auto_start_on_login`), and that is the canonical on-disk format Driven keeps
/// (so a DB seeded by the migration and one rewritten by `update_settings` are
/// byte-shape-compatible, and any future non-IPC reader sees one casing). The
/// wire/DTO groups in [`crate::commands::dtos`] are `camelCase` (the M6 typed-IPC
/// convention). These structs bridge the two: every settings read deserializes
/// into a `storage::*` struct, every write serializes one, and `From`
/// conversions map field-for-field to/from the matching DTO group. The field SET
/// is identical to the DTO's; only the serde casing differs.
mod storage {
    use serde::{Deserialize, Serialize};

    use crate::commands::dtos::{
        GlobalSettings, ScheduleSettings, TelemetrySettings, UiSettings, UpdaterSettings,
        WindowsSettings,
    };

    /// `snake_case` on-disk form of the V2 schedule window (DESIGN s17).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Schedule {
        pub enabled: bool,
        pub start_minute: u32,
        pub end_minute: u32,
        pub days: Vec<bool>,
        pub utc_offset_minutes: i32,
    }

    impl Default for Schedule {
        fn default() -> Self {
            // Disabled, all-day/every-day (matches `default_schedule`).
            Schedule {
                enabled: false,
                start_minute: 0,
                end_minute: 0,
                days: vec![true; 7],
                utc_offset_minutes: 0,
            }
        }
    }

    impl From<Schedule> for ScheduleSettings {
        fn from(s: Schedule) -> Self {
            ScheduleSettings {
                enabled: s.enabled,
                start_minute: s.start_minute,
                end_minute: s.end_minute,
                days: s.days,
                utc_offset_minutes: s.utc_offset_minutes,
            }
        }
    }

    impl From<ScheduleSettings> for Schedule {
        fn from(d: ScheduleSettings) -> Self {
            Schedule {
                enabled: d.enabled,
                start_minute: d.start_minute,
                end_minute: d.end_minute,
                days: d.days,
                utc_offset_minutes: d.utc_offset_minutes,
            }
        }
    }

    /// `snake_case` on-disk form of the SPEC s22 `global` group.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Global {
        pub auto_start_on_login: bool,
        pub default_concurrent_uploads: Option<u32>,
        // Added with adaptive parallelism (DESIGN s11.4.7). `serde(default)`
        // returns `true` so a `global` blob persisted before this field still
        // deserialises with adaptation ON (the default-on behaviour).
        #[serde(default = "default_adaptive_parallelism_enabled")]
        pub adaptive_parallelism_enabled: bool,
        pub bandwidth_cap_mbps: Option<u32>,
        pub skip_on_battery: bool,
        pub skip_on_metered: bool,
        pub scan_interval_secs: u32,
        pub deep_verify_interval_secs: u32,
        pub io_priority: String,
        pub log_level: String,
        // Added in V2 (schedule windows). `serde(default)` so a `global` blob
        // persisted before this field still deserialises (the disabled
        // default = V1 behaviour).
        #[serde(default)]
        pub schedule: Schedule,
        // Added in V2 (pre/post backup hooks). `serde(default)` so a pre-V2
        // `global` blob still deserialises.
        #[serde(default)]
        pub pre_backup_hook: Option<String>,
        #[serde(default)]
        pub post_backup_hook: Option<String>,
        #[serde(default = "default_hook_timeout_secs")]
        pub hook_timeout_secs: u32,
        #[serde(default = "default_metered_mode")]
        pub metered_mode: String,
        #[serde(default)]
        pub metered_bandwidth_cap_mbps: Option<u32>,
        // Issue #34 (corporate CA pinning). `serde(default)` so a `global` blob
        // persisted before this field still deserialises (default = None =
        // system trust only, the unchanged behaviour).
        #[serde(default)]
        pub custom_root_ca_path: Option<std::path::PathBuf>,
    }

    /// Default hook timeout (seconds) for a pre-V2 `global` blob missing it.
    fn default_hook_timeout_secs() -> u32 {
        60
    }

    /// Default for a `global` blob predating adaptive parallelism: ON (DESIGN
    /// s11.4.7 ships default-on).
    fn default_adaptive_parallelism_enabled() -> bool {
        true
    }

    /// Default metered mode (V1 behaviour: pause) for a pre-V2 `global` blob.
    fn default_metered_mode() -> String {
        "pause".to_string()
    }

    impl From<Global> for GlobalSettings {
        fn from(s: Global) -> Self {
            GlobalSettings {
                auto_start_on_login: s.auto_start_on_login,
                default_concurrent_uploads: s.default_concurrent_uploads,
                adaptive_parallelism_enabled: s.adaptive_parallelism_enabled,
                bandwidth_cap_mbps: s.bandwidth_cap_mbps,
                skip_on_battery: s.skip_on_battery,
                skip_on_metered: s.skip_on_metered,
                scan_interval_secs: s.scan_interval_secs,
                deep_verify_interval_secs: s.deep_verify_interval_secs,
                io_priority: s.io_priority,
                log_level: s.log_level,
                schedule: s.schedule.into(),
                pre_backup_hook: s.pre_backup_hook,
                post_backup_hook: s.post_backup_hook,
                hook_timeout_secs: s.hook_timeout_secs,
                metered_mode: s.metered_mode,
                metered_bandwidth_cap_mbps: s.metered_bandwidth_cap_mbps,
                custom_root_ca_path: s.custom_root_ca_path,
            }
        }
    }

    impl From<GlobalSettings> for Global {
        fn from(d: GlobalSettings) -> Self {
            Global {
                auto_start_on_login: d.auto_start_on_login,
                default_concurrent_uploads: d.default_concurrent_uploads,
                adaptive_parallelism_enabled: d.adaptive_parallelism_enabled,
                bandwidth_cap_mbps: d.bandwidth_cap_mbps,
                skip_on_battery: d.skip_on_battery,
                skip_on_metered: d.skip_on_metered,
                scan_interval_secs: d.scan_interval_secs,
                deep_verify_interval_secs: d.deep_verify_interval_secs,
                io_priority: d.io_priority,
                log_level: d.log_level,
                schedule: d.schedule.into(),
                pre_backup_hook: d.pre_backup_hook,
                post_backup_hook: d.post_backup_hook,
                hook_timeout_secs: d.hook_timeout_secs,
                metered_mode: d.metered_mode,
                metered_bandwidth_cap_mbps: d.metered_bandwidth_cap_mbps,
                custom_root_ca_path: d.custom_root_ca_path,
            }
        }
    }

    /// `snake_case` on-disk form of the SPEC s22 `telemetry` group.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Telemetry {
        pub enabled: bool,
        pub install_id: String,
        pub endpoint: String,
    }

    impl From<Telemetry> for TelemetrySettings {
        fn from(s: Telemetry) -> Self {
            TelemetrySettings {
                enabled: s.enabled,
                install_id: s.install_id,
                endpoint: s.endpoint,
            }
        }
    }

    impl From<TelemetrySettings> for Telemetry {
        fn from(d: TelemetrySettings) -> Self {
            Telemetry {
                enabled: d.enabled,
                install_id: d.install_id,
                endpoint: d.endpoint,
            }
        }
    }

    /// `snake_case` on-disk form of the SPEC s22 `updater` group.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Updater {
        pub channel: String,
        pub check_interval_secs: u32,
    }

    impl From<Updater> for UpdaterSettings {
        fn from(s: Updater) -> Self {
            UpdaterSettings {
                channel: s.channel,
                check_interval_secs: s.check_interval_secs,
            }
        }
    }

    impl From<UpdaterSettings> for Updater {
        fn from(d: UpdaterSettings) -> Self {
            Updater {
                channel: d.channel,
                check_interval_secs: d.check_interval_secs,
            }
        }
    }

    /// `snake_case` on-disk form of the SPEC s22 `ui` group.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Ui {
        pub tray_left_click_opens: String,
        pub locale: String,
        pub color_mode: String,
    }

    impl From<Ui> for UiSettings {
        fn from(s: Ui) -> Self {
            UiSettings {
                tray_left_click_opens: s.tray_left_click_opens,
                locale: s.locale,
                color_mode: s.color_mode,
            }
        }
    }

    impl From<UiSettings> for Ui {
        fn from(d: UiSettings) -> Self {
            Ui {
                tray_left_click_opens: d.tray_left_click_opens,
                locale: d.locale,
                color_mode: d.color_mode,
            }
        }
    }

    /// `snake_case` on-disk form of the SPEC s22 `windows` group (Windows-only).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Windows {
        pub vss_mode: String,
        // Absent in DBs written before the least-privilege helper landed
        // (DESIGN s5.3.1); default to `false` so an older row deserialises.
        #[serde(default)]
        pub vss_helper: bool,
    }

    impl From<Windows> for WindowsSettings {
        fn from(s: Windows) -> Self {
            WindowsSettings {
                vss_mode: s.vss_mode,
                vss_helper: s.vss_helper,
            }
        }
    }

    impl From<WindowsSettings> for Windows {
        fn from(d: WindowsSettings) -> Self {
            Windows {
                vss_mode: d.vss_mode,
                vss_helper: d.vss_helper,
            }
        }
    }
}

/// Register or unregister the app for OS autostart (SPEC s13) via the autostart
/// plugin's `ManagerExt`. A plugin error maps to `internal.bug` (the OS refused
/// the registry / LaunchAgent / .desktop write).
fn apply_autostart(app: &AppHandle, enable: bool) -> CommandResult<()> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    let result = if enable {
        manager.enable()
    } else {
        manager.disable()
    };
    result.map_err(|e| {
        CommandError::with_code(
            ErrorCode::InternalBug,
            format!(
                "failed to {} autostart: {e}",
                if enable { "enable" } else { "disable" }
            ),
        )
    })?;
    tracing::info!(target: TARGET, enable, "autostart-on-login updated");
    Ok(())
}

/// Decide what (if anything) the boot-time autostart reconciliation must do.
///
/// `desired` is the persisted `global.auto_start_on_login` preference; `actual`
/// is what the OS autostart manager currently reports via `is_enabled()`.
/// Returns `Some(true)` to register, `Some(false)` to unregister, or `None` when
/// they already agree (no OS write needed). Pure + total so the reconciliation
/// logic is unit-testable without a live Tauri app / OS registry.
fn autostart_reconcile_action(desired: bool, actual: bool) -> Option<bool> {
    if desired == actual {
        None
    } else {
        Some(desired)
    }
}

/// Boot-time reconciliation of the OS autostart registration with the persisted
/// `global.auto_start_on_login` preference (SPEC s13, issue #58).
///
/// [`apply_autostart`] only fires on a settings *change*, so a DB default of
/// `true` (migration 0005 / [`default_global`]) would never register the real OS
/// startup entry - the app would claim "auto-start ON" while Task Manager's
/// Startup tab (and the macOS LaunchAgent / Linux `.desktop` equivalent) showed
/// nothing. This runs once at startup: it reads the stored preference, compares
/// it to the autostart manager's actual `is_enabled()`, and enables/disables to
/// match so the two never drift.
///
/// Best-effort: a malformed settings read, an `is_enabled()` failure, or an OS
/// refusal is logged and swallowed - it must never abort boot.
pub async fn reconcile_autostart_on_boot(app: &AppHandle, state: &dyn StateRepo) {
    use tauri_plugin_autostart::ManagerExt;

    let desired = match load_group::<storage::Global>(state, KEY_GLOBAL).await {
        Ok(Some(g)) => GlobalSettings::from(g).auto_start_on_login,
        Ok(None) => default_global().auto_start_on_login,
        Err(err) => {
            tracing::warn!(
                target: TARGET,
                %err,
                "autostart boot-reconcile: could not read global settings; skipping"
            );
            return;
        }
    };

    let manager = app.autolaunch();
    let actual = match manager.is_enabled() {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                target: TARGET,
                %err,
                "autostart boot-reconcile: is_enabled() failed; skipping"
            );
            return;
        }
    };

    let Some(enable) = autostart_reconcile_action(desired, actual) else {
        tracing::debug!(
            target: TARGET,
            desired,
            "autostart boot-reconcile: OS state already matches preference"
        );
        return;
    };

    let result = if enable {
        manager.enable()
    } else {
        manager.disable()
    };
    match result {
        Ok(()) => tracing::info!(
            target: TARGET,
            enabled = enable,
            "autostart boot-reconcile: OS startup entry synced to persisted preference"
        ),
        Err(err) => tracing::warn!(
            target: TARGET,
            %err,
            enabled = enable,
            "autostart boot-reconcile: enable/disable failed (OS refused); leaving as-is"
        ),
    }
}

/// Apply a `tracing` max-level change at runtime (SPEC s22 `global.log_level`).
///
/// The process installs a plain `tracing_subscriber::fmt` subscriber at boot
/// (lib.rs) with no runtime-reloadable filter handle, so this cannot mutate the
/// LIVE subscriber's level in-process; what it CAN do honestly is export the
/// chosen level to `RUST_LOG` so it is the effective level on the next launch and
/// for any subsystem that reads the env. The persisted `global.log_level` is the
/// source of truth a future reload-handle wires to (it is recorded here so the
/// behaviour is not a silent no-op).
fn apply_log_level(level: &str) {
    // SAFETY note: `set_var` is process-global; we only ever write a validated
    // tracing level string, never untrusted bytes, and only from this command.
    std::env::set_var("RUST_LOG", level);
    tracing::info!(target: TARGET, level, "log level setting updated (effective for new launches / env-reading subsystems)");
}

/// Apply a locale change: set the Rust-side i18n locale + re-render the tray, and
/// notify the frontend so vue-i18n re-renders (DESIGN s8.7). Best-effort: a tray
/// rebuild / emit failure is logged, never fatal (the persisted locale is the
/// source of truth and the next launch picks it up regardless).
fn apply_locale(app: &AppHandle, locale: &str) {
    rust_i18n::set_locale(locale);
    // Rebuild the tray so its menu labels + tooltip pick up the new locale.
    if let Err(err) = crate::tray::rebuild(app) {
        tracing::warn!(target: TARGET, locale, %err, "failed to re-render tray after locale change");
    }
    // Tell the webview to switch vue-i18n's active locale.
    use tauri::Emitter;
    if let Err(err) = app.emit("settings:locale_changed", locale) {
        tracing::debug!(target: TARGET, locale, %err, "emit settings:locale_changed failed");
    }
    tracing::info!(target: TARGET, locale, "locale updated (tray re-rendered, frontend notified)");
}

/// Reconfigure EVERY running account's orchestrator with a config derived from
/// the freshly-stored settings (DESIGN s5.7: a settings change applies on the
/// next cycle). Best-effort per account; an account with no running orchestrator
/// picks the change up on next start.
async fn reconfigure_all(state: &State<'_, AppState>) {
    let config = load_orchestrator_config(state.state().as_ref())
        .await
        .unwrap_or_default();
    for (account_id, handle) in state.accounts() {
        handle.orchestrator.reconfigure(config.clone()).await;
        tracing::debug!(target: TARGET, account_id = %account_id, "orchestrator reconfigured after settings change");
    }
}

/// Build an [`OrchestratorConfig`] from the persisted SPEC s22 settings (the
/// `global` gates + cap + scan cadence, and `windows.vss_mode` on Windows).
///
/// Shared with [`crate::commands::sources`] (which reconfigures an account's
/// orchestrator after a source add/update so a concurrent settings edit is
/// honoured). On a read/parse failure the conservative
/// [`OrchestratorConfig::default`] is used rather than erroring - reconfigure is
/// a best-effort optimisation, never load-bearing for correctness.
pub async fn load_orchestrator_config(state: &dyn StateRepo) -> CommandResult<OrchestratorConfig> {
    let global: GlobalSettings = load_group::<storage::Global>(state, KEY_GLOBAL)
        .await?
        .map(Into::into)
        .unwrap_or_else(default_global);

    let vss_mode = if cfg!(windows) {
        load_group::<storage::Windows>(state, KEY_WINDOWS)
            .await?
            .map(|w| VssMode::from_str_lenient(&w.vss_mode))
            .unwrap_or_default()
    } else {
        VssMode::default()
    };

    let defaults = OrchestratorConfig::default();
    Ok(OrchestratorConfig {
        dry_run: defaults.dry_run,
        skip_on_battery: global.skip_on_battery,
        skip_on_metered: global.skip_on_metered,
        scan_interval_secs: u64::from(global.scan_interval_secs),
        bandwidth_cap_mbps: global.bandwidth_cap_mbps,
        pacer_ceilings: defaults.pacer_ceilings,
        vss_mode,
        schedule: schedule_settings_to_config(&global.schedule),
        pre_backup_hook: global.pre_backup_hook.clone(),
        post_backup_hook: global.post_backup_hook.clone(),
        hook_timeout_secs: global.hook_timeout_secs,
        metered_mode: if global.metered_mode == "throttle" {
            MeteredMode::Throttle
        } else {
            MeteredMode::Pause
        },
        metered_bandwidth_cap_mbps: global.metered_bandwidth_cap_mbps,
        default_concurrent_uploads: global.default_concurrent_uploads,
        adaptive_parallelism_enabled: global.adaptive_parallelism_enabled,
    })
}

/// Issue #34: normalise a webview-supplied custom-CA path, treating a blank /
/// whitespace-only string as `None` (system trust only). Kept in one place so
/// the save path and any future callers agree on "blank == unset".
fn normalize_ca_path(path: Option<PathBuf>) -> Option<PathBuf> {
    // Keep the path only when it is non-blank; an empty / whitespace-only string
    // (`to_string_lossy().trim()` also covers an empty `OsStr`) becomes `None`.
    path.filter(|p| !p.to_string_lossy().trim().is_empty())
}

/// Issue #34: load the persisted custom-root-CA setting into a
/// [`driven_tls::CustomCaConfig`] for the client-build sites (assembly's Drive +
/// network clients, the updater, the GitHub-releases + userinfo fetches, the
/// telemetry sink). A read/parse failure of the settings blob degrades to
/// "system trust only" (best-effort, like [`load_orchestrator_config`]); the
/// PEM file itself is only opened at client-build time, where a bad file fails
/// closed.
pub async fn load_custom_ca_config(
    state: &dyn StateRepo,
) -> CommandResult<driven_tls::CustomCaConfig> {
    let global: GlobalSettings = load_group::<storage::Global>(state, KEY_GLOBAL)
        .await?
        .map(Into::into)
        .unwrap_or_else(default_global);
    Ok(driven_tls::CustomCaConfig::from_path(normalize_ca_path(
        global.custom_root_ca_path,
    )))
}

/// Map the persisted [`ScheduleSettings`] DTO onto the core
/// [`ScheduleConfig`], clamping defensively: minutes saturate into
/// `0..=1439` and `days` is coerced to exactly seven booleans (missing days
/// default to allowed) so a malformed stored blob can never panic the gate.
fn schedule_settings_to_config(s: &ScheduleSettings) -> driven_core::types::ScheduleConfig {
    let mut days = [true; 7];
    for (i, slot) in days.iter_mut().enumerate() {
        if let Some(d) = s.days.get(i) {
            *slot = *d;
        }
    }
    driven_core::types::ScheduleConfig {
        enabled: s.enabled,
        start_minute: s.start_minute.min(1439) as u16,
        end_minute: s.end_minute.min(1439) as u16,
        days,
        utc_offset_minutes: s.utc_offset_minutes.clamp(-1440, 1440) as i16,
    }
}

// ---------------------------------------------------------------------------
// export_diagnostic_bundle (SPEC s11.6, s18)
// ---------------------------------------------------------------------------

/// `export_diagnostic_bundle(token)` - write a redacted diagnostic ZIP
/// (SPEC s11.6, s18).
///
/// C1 (SPEC s11.6.1): the destination is NOT a webview-supplied string - it is
/// the concrete `.zip` save path bound to the backend-minted `token` from
/// `pick_save_zip_dialog`. The token is taken (single-use) and resolved to that
/// path; a missing / unknown / expired token is REJECTED (the webview cannot
/// inject a path). C2: the bound path is a FILE, so the ZIP is written AT that
/// file (not over a directory). [`validate_writable_dest`] confines the write to
/// the dialog-approved directory (no `..`, no symlink-at-leaf) and
/// [`atomic_write`] writes the ZIP atomically (SPEC s11.6.1 step 5).
///
/// The bundle (SPEC s18) carries `version.txt`, `os.txt`, a REDACTED
/// `settings_redacted.json`, `schema.txt` (real PRAGMA user_version + table
/// counts), `activity_last_30d.csv`, `logs/`, `crashes/`, and
/// `redaction-policy.txt`. Every secret-bearing field (refresh tokens, recovery
/// phrases, keys, master key, account emails, drive folder names, local paths,
/// Drive file ids) is redacted or hashed before it enters the ZIP.
#[tauri::command]
pub async fn export_diagnostic_bundle(
    app: AppHandle,
    state: State<'_, AppState>,
    token: String,
) -> CommandResult<PathBuf> {
    // C1: resolve the save path from the backend-minted dialog token (single-use).
    // Reject any request without a matching token - the webview never supplies a
    // raw path here.
    let dest = state.take_dialog_token(&token).ok_or_else(|| {
        CommandError::with_code(
            ErrorCode::LocalIoError,
            "no matching dialog token for the export destination; pick a save location first",
        )
    })?;

    // C2: the token's path is a concrete FILE. Confine the write to its parent
    // directory (the dialog-approved root) and re-validate the leaf.
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            CommandError::with_code(
                ErrorCode::LocalIoError,
                "diagnostic bundle destination must include a directory",
            )
        })?;
    let dialog_token = DialogToken::for_root(parent.to_string_lossy().to_string());
    let confined = validate_writable_dest(&dest, &dialog_token)?;

    // Build the bundle bytes (redacted, SPEC s18) off the persisted state.
    let zip_bytes = build_diagnostic_zip(&app, state.state().as_ref()).await?;

    // SPEC s11.6.1 step 5: atomic write so a crash never leaves a half ZIP.
    atomic_write(&confined, &zip_bytes)?;
    tracing::info!(target: TARGET, dest = %confined.display(), bytes = zip_bytes.len(), "diagnostic bundle written");
    Ok(confined)
}

/// Assemble the REDACTED diagnostic-bundle ZIP bytes (SPEC s18).
async fn build_diagnostic_zip(app: &AppHandle, state: &dyn StateRepo) -> CommandResult<Vec<u8>> {
    let mut zip = ZipWriter::new();

    // R2-P1-4: build the context-rich redactor ONCE (source roots + home +
    // username) so every log / crash / activity message is scrubbed of the user's
    // actual paths, not just absolute-path-prefixed tokens.
    let redactor = Redactor::for_bundle(state).await;

    // version.txt + os.txt (SPEC s18).
    zip.add_file(
        "version.txt",
        app.package_info().version.to_string().as_bytes(),
    );
    zip.add_file("os.txt", os_descriptor().as_bytes());

    // settings_redacted.json (SPEC s18): the full settings document with every
    // secret-bearing field redacted / hashed.
    let settings = load_settings_dto(state).await?;
    let redacted = redact_settings(&settings);
    let settings_json = serde_json::to_vec_pretty(&redacted).map_err(|e| {
        CommandError::with_code(
            ErrorCode::InternalBug,
            format!("serialize redacted settings: {e}"),
        )
    })?;
    zip.add_file("settings_redacted.json", &settings_json);

    // schema.txt (SPEC s18): REAL PRAGMA user_version + table row counts. Both
    // are metadata-only (no file paths / ids), so no redaction is needed.
    let schema = build_schema_summary(state).await;
    zip.add_file("schema.txt", schema.as_bytes());

    // activity_last_30d.csv (SPEC s18): the activity_log for the last 30 days,
    // with the free-text message column passed through the redaction pipeline
    // (paths / drive ids / emails / tokens become stable per-bundle hashes).
    let activity_csv = build_activity_csv(state, &redactor).await;
    zip.add_file("activity_last_30d.csv", activity_csv.as_bytes());

    // logs/ + crashes/ (SPEC s18): the recent tracing output + crash dumps from
    // <config_dir>/driven/logs/, each passed through the redaction pipeline.
    add_logs_and_crashes(app, &mut zip, &redactor);

    // redaction-policy.txt (SPEC s18): tell the recipient the bundle's threat
    // model + exactly what was redacted.
    zip.add_file("redaction-policy.txt", REDACTION_POLICY.as_bytes());

    Ok(zip.finish())
}

/// SPEC s18: the maximum bytes of log content the bundle carries (the spec's
/// "last 50 MB of tracing output"). A per-file cap is applied so one huge log
/// cannot dominate; the newest files are preferred.
const MAX_LOG_BYTES: u64 = 50 * 1024 * 1024;

/// Build `activity_last_30d.csv` from the `activity_log` (SPEC s18).
///
/// Columns: `ts,event_type,level,source_id,file_count,bytes,message`. The
/// free-text `message` is passed through the [`Redactor`] (paths / drive ids /
/// emails / tokens -> stable per-bundle hashes); the `source_id` is hashed too
/// (it correlates to a local source). Best-effort: a query failure yields a
/// header-only CSV with an error note rather than failing the whole bundle.
async fn build_activity_csv(state: &dyn StateRepo, redactor: &Redactor) -> String {
    // M7-R3-P2 (recheck-3): the activity_log can exceed one page in a 30-day
    // window now that every successful upload writes a per-file `upload_done`
    // row (a large first backup is easily > 10k events). A single
    // `PageRequest::first(10_000)` silently dropped the rest of the required
    // history. The keyset-paged collector below walks ALL pages.
    //
    // CSV_PAGE_SIZE is the SPEC s18.8 per-page cap (10_000); CSV_MAX_ROWS bounds
    // the whole bundle (5M = the activity_log retention hard cap) so a runaway
    // log cannot produce an unbounded snapshot.
    const CSV_PAGE_SIZE: u32 = 10_000;
    const CSV_MAX_ROWS: usize = 5_000_000;
    build_activity_csv_paged(state, redactor, CSV_PAGE_SIZE, CSV_MAX_ROWS).await
}

/// Keyset-paged collector behind [`build_activity_csv`] (M7-R3-P2). Walks every
/// `activity_log` page in the 30-day window (newest-first, then `after_cursor`)
/// until `!has_more` or the row cap, so the bundle carries the full required
/// history rather than just the first page. `page_size` / `max_rows` are
/// parameters so a test can drive the multi-page loop with a small page.
async fn build_activity_csv_paged(
    state: &dyn StateRepo,
    redactor: &Redactor,
    page_size: u32,
    max_rows: usize,
) -> String {
    use driven_core::state::{ActivityFilter, PageRequest};
    use driven_core::time::{Clock, SystemClock};

    let mut out = String::new();
    out.push_str("ts,event_type,level,source_id,file_count,bytes,message\n");

    let now = SystemClock.now_ms();
    let thirty_days_ms: i64 = 30 * 24 * 60 * 60 * 1000;
    let since = now.saturating_sub(thirty_days_ms);
    let filter = ActivityFilter {
        since_ms: Some(since),
        ..Default::default()
    };

    let mut cursor: Option<(i64, i64)> = None;
    let mut written: usize = 0;
    loop {
        let page = match cursor {
            None => PageRequest::first(page_size),
            Some((ts, id)) => PageRequest::after_cursor(ts, id, page_size),
        };
        match state.query_activity(filter.clone(), page).await {
            Ok(activity) => {
                let last = activity.rows.last().map(|r| (r.ts, r.id.0));
                for row in &activity.rows {
                    if written >= max_rows {
                        break;
                    }
                    let level = format!("{:?}", row.level);
                    let source = row
                        .source_id
                        .map(|s| format!("source_{}", stable_hash(&s.to_string())))
                        .unwrap_or_default();
                    let file_count = row.file_count.map(|c| c.to_string()).unwrap_or_default();
                    let bytes = row.bytes.map(|b| b.to_string()).unwrap_or_default();
                    let message = row
                        .message
                        .as_deref()
                        .map(|m| redactor.redact_text(m))
                        .unwrap_or_default();
                    out.push_str(&format!(
                        "{},{},{},{},{},{},{}\n",
                        row.ts,
                        csv_field(&row.event_type),
                        csv_field(&level),
                        csv_field(&source),
                        file_count,
                        bytes,
                        csv_field(&message),
                    ));
                    written += 1;
                }
                // Stop on history-exhausted, the row cap, or a degenerate page
                // with no cursor to advance (defence against a non-advancing
                // backend).
                if !activity.has_more || written >= max_rows {
                    break;
                }
                match last {
                    Some((ts, id)) => cursor = Some((ts, id)),
                    None => break,
                }
            }
            Err(e) => {
                out.push_str(&format!("# activity query failed: {e}\n"));
                break;
            }
        }
    }
    out
}

/// Escape a CSV field: wrap in double quotes and double any embedded quote when
/// it contains a comma, quote, or newline (RFC 4180).
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// SPEC s18: add `logs/` (recent tracing output) + `crashes/` (crash dumps) from
/// `<config_dir>/driven/logs/`, each passed through the redaction pipeline.
///
/// Best-effort: a missing log dir / unreadable file is logged + skipped (a
/// partial bundle is still useful). `crash-*.txt` files go under `crashes/`;
/// every other file goes under `logs/`. The cumulative log bytes are bounded by
/// [`MAX_LOG_BYTES`], newest-first.
fn add_logs_and_crashes(app: &AppHandle, zip: &mut ZipWriter, redactor: &Redactor) {
    use tauri::Manager;
    let log_dir = match app.path().app_config_dir() {
        Ok(dir) => dir.join("driven").join("logs"),
        Err(e) => {
            tracing::debug!(target: TARGET, error = %e, "diagnostic bundle: cannot resolve log dir; omitting logs/");
            return;
        }
    };
    let entries = match std::fs::read_dir(&log_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(target: TARGET, dir = %log_dir.display(), error = %e, "diagnostic bundle: log dir unreadable; omitting logs/");
            return;
        }
    };

    // Collect (path, modified, is_crash) so we can prefer the newest files and
    // bound the cumulative log bytes.
    let mut files: Vec<(std::path::PathBuf, std::time::SystemTime, bool)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let is_crash = name.starts_with("crash-");
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        files.push((path, modified, is_crash));
    }
    // Newest first so the byte budget keeps the most recent logs.
    files.sort_by_key(|f| std::cmp::Reverse(f.1));

    let mut log_bytes_used: u64 = 0;
    for (path, _modified, is_crash) in files {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let raw = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(target: TARGET, file = %path.display(), error = %e, "diagnostic bundle: skipping unreadable log file");
                continue;
            }
        };
        // Crashes are always included (small + high-value); plain logs respect
        // the cumulative byte budget.
        if !is_crash {
            let len = raw.len() as u64;
            if log_bytes_used.saturating_add(len) > MAX_LOG_BYTES {
                continue;
            }
            log_bytes_used = log_bytes_used.saturating_add(len);
        }
        let redacted = redactor.redact_text(&raw);
        let arcname = if is_crash {
            format!("crashes/{name}")
        } else {
            format!("logs/{name}")
        };
        zip.add_file(&arcname, redacted.as_bytes());
    }
}

/// R2-P1-4: the SPEC s18 redaction engine for log / activity / crash free-text.
///
/// Whereas the old implementation only redacted whitespace-delimited tokens that
/// START with an absolute path (so a `path=C:\Users\Pat Smith\f.pdf` field, a
/// quoted path, a path WITH SPACES, or a UNC path leaked), this redactor scrubs
/// over the WHOLE line and in ALL path shapes, plus the user's home dir,
/// username, and known source-root substrings. Order per line:
///
/// 1. EXACT known-substring replacement (longest-first): every backup source
///    root, the user home dir, and the username - each replaced wherever it
///    appears, INCLUDING inside a longer path or one with spaces.
/// 2. ABSOLUTE-PATH-RUN scrub: a positional scan that finds an absolute path
///    start (`X:\` / `X:/` Windows drive, `\\server\share` UNC, or a `/abs`
///    Unix path at a left boundary) and replaces the whole run - consuming
///    embedded SPACES (a `key=path with spaces` value, a quoted path) up to the
///    field / quote / line boundary - with a stable `<path:hash>`.
/// 3. TOKEN scrub of the residual: OAuth tokens, emails, and long Drive-id-shaped
///    opaque ids, each -> a stable hashed placeholder.
///
/// Each placeholder is a stable per-value hash so occurrences correlate WITHIN a
/// bundle without exposing the original. Best-effort (SPEC s18 caveat) but errs
/// toward OVER-redaction of anything path- or secret-shaped.
struct Redactor {
    /// Known backup source roots (from `backup_sources.local_path`), longest
    /// first, each lower-cased once for case-insensitive matching.
    source_roots: Vec<String>,
    /// The user's home dir (e.g. `C:\Users\Pat Smith`), if resolvable.
    home: Option<String>,
    /// The OS username, if resolvable.
    username: Option<String>,
}

impl Redactor {
    /// Build a context-rich redactor for the diagnostic bundle: load every
    /// source root from the state DB and resolve the home dir + username from the
    /// environment, so those exact substrings (which may contain spaces) are
    /// scrubbed even when a path scan would miss them. Best-effort: a source-list
    /// read failure yields an empty root set (the path-run + token scrubs still
    /// run); the home / username come from `HOME` / `USERPROFILE` / `USERNAME`.
    async fn for_bundle(state: &dyn StateRepo) -> Self {
        let mut source_roots: Vec<String> = match state.list_sources().await {
            Ok(rows) => rows
                .into_iter()
                .map(|s| s.local_path.to_lowercase())
                .filter(|p| !p.trim().is_empty())
                .collect(),
            Err(e) => {
                tracing::debug!(target: TARGET, error = %e, "redactor: could not load source roots; path-run + token scrub still apply");
                Vec::new()
            }
        };
        // Longest first so a nested root is replaced before its prefix.
        source_roots.sort_by_key(|b| std::cmp::Reverse(b.len()));
        source_roots.dedup();

        let home = std::env::var("USERPROFILE")
            .ok()
            .or_else(|| std::env::var("HOME").ok())
            .filter(|h| !h.trim().is_empty());
        let username = std::env::var("USERNAME")
            .ok()
            .or_else(|| std::env::var("USER").ok())
            .filter(|u| u.trim().len() >= 3); // avoid scrubbing 1-2 char noise

        Self {
            source_roots,
            home,
            username,
        }
    }

    /// A context-free redactor (no source roots / home / username) for callers
    /// without DB / env context. The path-run + token scrubs still apply. Used by
    /// the redaction unit tests; the bundle path always uses
    /// [`Self::for_bundle`].
    #[cfg(test)]
    fn context_free() -> Self {
        Self {
            source_roots: Vec::new(),
            home: None,
            username: None,
        }
    }

    /// Redact every line of `input` (newline-joined).
    fn redact_text(&self, input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        for line in input.lines() {
            out.push_str(&self.redact_line(line));
            out.push('\n');
        }
        out
    }

    /// Redact one line across all shapes (see [`Redactor`] doc): known
    /// substrings, then absolute-path runs, then residual tokens.
    fn redact_line(&self, line: &str) -> String {
        // 1) Known substrings (home dir + source roots), case-insensitive, longest
        //    first. The home dir is a prefix of many source roots, so source roots
        //    (already longest-first) are tried before the home dir.
        let mut work = line.to_string();
        for root in &self.source_roots {
            work = replace_ci(&work, root, &format!("<path:{}>", stable_hash(root)));
        }
        if let Some(home) = &self.home {
            work = replace_ci(&work, &home.to_lowercase(), "<home-redacted>");
        }
        if let Some(user) = &self.username {
            // Username scrubbed only as a whole word-ish run to avoid mangling
            // unrelated text; replace_ci is substring, so guard on length above.
            work = replace_ci(&work, &user.to_lowercase(), "<user-redacted>");
        }

        // 2) Absolute-path runs (handles spaces, quotes, key=value, UNC).
        let work = redact_absolute_path_runs(&work);

        // 3) Residual token scrub (tokens / emails / drive ids), over the
        //    whitespace-delimited tokens that survived the path-run pass.
        work.split_inclusive(char::is_whitespace)
            .map(|chunk| {
                let trail_ws: String = chunk
                    .chars()
                    .rev()
                    .take_while(|c| c.is_whitespace())
                    .collect();
                let core = &chunk[..chunk.len() - trail_ws.len()];
                let redacted = redact_token(core);
                format!("{redacted}{}", trail_ws.chars().rev().collect::<String>())
            })
            .collect()
    }
}

/// Case-insensitive substring replace of EVERY occurrence of `needle` in
/// `haystack` with `replacement`. Used for the known home / source-root scrub so
/// a path with spaces (which the path-run scanner could under-consume) is removed
/// wherever it appears. Empty `needle` is a no-op.
///
/// R3-P1-3: this MUST return spans in the ORIGINAL `haystack`, NOT in
/// `haystack.to_lowercase()`. Unicode case folding can change a string's byte
/// length (e.g. some characters lowercase to a multi-byte sequence), so offsets
/// found in the lowercased haystack do not line up with the original bytes -
/// slicing the original with them mis-redacts or PANICS on a non-char boundary.
/// Instead we walk the ORIGINAL char boundaries and, at each one, attempt a
/// case-insensitive char-by-char match of the (already-lowercased) `needle`. The
/// returned spans are therefore always valid original-string byte ranges.
///
/// CONTRACT: `needle` MUST be pre-lowercased by the caller (every call site
/// passes a `.to_lowercase()`d string - source roots are stored lowercased,
/// home/username are lowercased at the call site). The haystack is lowercased on
/// the fly, char by char.
fn replace_ci(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    // Original char boundaries (byte offsets) plus the end sentinel, so a match
    // span is always sliced on a real char boundary of `haystack`.
    let starts: Vec<usize> = haystack
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(haystack.len()))
        .collect();

    let mut out = String::with_capacity(haystack.len());
    let mut last = 0usize; // byte offset (original) of unflushed text start
    let mut idx = 0usize; // index into `starts` (current scan position)
    while idx + 1 < starts.len() {
        let start = starts[idx];
        if let Some(end) = ci_match_at(haystack, start, needle) {
            // Flush the un-matched original text, then the replacement.
            out.push_str(&haystack[last..start]);
            out.push_str(replacement);
            last = end;
            // Resume scanning at the original char boundary == `end`. `end` is a
            // valid char boundary (it is the start of some char or the string
            // end), so it appears in `starts`; advance `idx` to it.
            while idx + 1 < starts.len() && starts[idx] < end {
                idx += 1;
            }
        } else {
            idx += 1;
        }
    }
    out.push_str(&haystack[last..]);
    out
}

/// If a case-insensitive occurrence of `needle` (which the caller has ALREADY
/// lowercased) begins at byte offset `start` (a char boundary) of `haystack`,
/// return the ORIGINAL-string byte offset just past the match; else `None`.
///
/// Compares char-by-char on lowercased chars so a case difference (and a
/// length-changing case fold) never produces a wrong byte span: we advance
/// through `haystack`'s real char boundaries and through `needle`'s chars in
/// lockstep, lowercasing each haystack char on the fly. The `needle` may be
/// shorter or longer in bytes than the matched original run; the returned end is
/// always a valid `haystack` char boundary.
fn ci_match_at(haystack: &str, start: usize, needle: &str) -> Option<usize> {
    let mut hay_chars = haystack[start..].chars();
    let mut needle_chars = needle.chars();
    let mut consumed = 0usize; // bytes of `haystack` matched so far

    loop {
        let nc = match needle_chars.next() {
            None => return Some(start + consumed), // needle fully matched
            Some(c) => c,
        };
        let hc = hay_chars.next()?; // haystack exhausted mid-needle -> no match
                                    // Lowercase each haystack char and compare against the (pre-lowercased)
                                    // needle char run. `to_lowercase` can yield >1 char; compare the full
                                    // expansion, pulling extra needle chars as needed.
        let mut hc_lower = hc.to_lowercase();
        let first = hc_lower.next()?;
        if first != nc {
            return None;
        }
        for extra in hc_lower {
            match needle_chars.next() {
                Some(n) if n == extra => {}
                _ => return None,
            }
        }
        consumed += hc.len_utf8();
    }
}

/// R2-P1-4: scan `line` for ABSOLUTE PATH RUNS and replace each with a stable
/// `<path:hash>`.
///
/// A path run starts at a Windows drive (`X:\`/`X:/`), a UNC prefix (`\\`), or a
/// Unix absolute (`/...`) appearing at a LEFT BOUNDARY (line start, whitespace,
/// `=`, a quote, `(`/`[`/`<`/`:`) so a mid-URL `/` or a `C:` not at a boundary is
/// not mis-detected.
///
/// Embedded SPACES are consumed ONLY when the run is delimited so the end is
/// unambiguous - i.e. the path is QUOTED (ends at the matching quote) or it
/// follows a `key=` (ends at the next whitespace token that looks like a new
/// `key=value` field). This handles the `path=C:\Users\Pat Smith\f.pdf` and
/// `"C:\Users\Pat Smith\f.pdf"` leaks WITHOUT swallowing trailing prose for a
/// bare unquoted path (which stops at the first whitespace). A configured source
/// root WITH spaces is handled separately by the known-substring scrub, so a bare
/// spaced path is rare; over-consuming prose would be worse than stopping early.
fn redact_absolute_path_runs(line: &str) -> String {
    let bytes = line.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut i = 0usize;
    while i < n {
        let prev = if i > 0 { Some(bytes[i - 1]) } else { None };
        let opened_quote = matches!(prev, Some(b'"') | Some(b'\''));
        let after_equals = prev == Some(b'=');
        let left_boundary = i == 0
            || matches!(
                prev,
                Some(b' ' | b'\t' | b'=' | b'"' | b'\'' | b'(' | b'[' | b'<' | b':')
            );
        if left_boundary {
            if let Some(run_len) = path_run_len(&bytes[i..], opened_quote, after_equals) {
                let run = &line[i..i + run_len];
                out.push_str(&format!("<path:{}>", stable_hash(run)));
                i += run_len;
                continue;
            }
        }
        // Not a path start here: copy this byte. `line` is UTF-8; copy whole char.
        let ch_len = utf8_char_len(bytes[i]);
        out.push_str(&line[i..(i + ch_len).min(n)]);
        i += ch_len;
    }
    out
}

/// If `s` begins with an absolute path, return the byte length of the path run,
/// else `None`.
///
/// - `inside_quote`: the slice begins right after an opening quote -> the run
///   ends at the matching closing quote and INCLUDES embedded spaces.
/// - `after_equals`: the slice begins right after `=` (a `key=path` value) -> the
///   run INCLUDES embedded spaces, ending at the next whitespace token that
///   contains `=` (a new field) or at end-of-line.
/// - otherwise (a bare unquoted path): the run stops at the FIRST whitespace, so
///   trailing prose / an adjacent email token is not swallowed.
fn path_run_len(s: &[u8], inside_quote: bool, after_equals: bool) -> Option<usize> {
    let n = s.len();
    // Detect the start shape.
    let is_win_abs =
        n >= 3 && s[0].is_ascii_alphabetic() && s[1] == b':' && (s[2] == b'\\' || s[2] == b'/');
    let is_unc = n >= 2 && s[0] == b'\\' && s[1] == b'\\';
    // A bare `/` is a Unix absolute path only if followed by a path segment char
    // (so a lone `/` or ` / ` is not redacted).
    let is_unix_abs = n >= 2 && s[0] == b'/' && is_path_char(s[1]);
    if !(is_win_abs || is_unc || is_unix_abs) {
        return None;
    }
    // Spaces are only consumed when the run has a clear end delimiter.
    let consume_spaces = inside_quote || after_equals;

    let mut i = 0usize;
    while i < n {
        let b = s[i];
        if inside_quote && (b == b'"' || b == b'\'') {
            break; // closing quote ends the run.
        }
        if b == b' ' || b == b'\t' {
            if !consume_spaces {
                break; // bare path: stop at the first whitespace.
            }
            if inside_quote {
                // Inside quotes a space is always part of the path (the closing
                // quote, handled above, is the only terminator).
                i += 1;
                continue;
            }
            // after_equals: include the space only if the next whitespace token
            // is NOT a new `key=value` field (and is non-empty).
            let rest = &s[i + 1..];
            let mut j = 0usize;
            while j < rest.len() && (rest[j] == b' ' || rest[j] == b'\t') {
                j += 1;
            }
            let mut k = j;
            while k < rest.len() && rest[k] != b' ' && rest[k] != b'\t' {
                k += 1;
            }
            let next_tok = &rest[j..k];
            if next_tok.is_empty() || next_tok.contains(&b'=') {
                break; // path value ended (new field / trailing space).
            }
            i += 1;
            continue;
        }
        // A comma / closing bracket / angle ends an unquoted run.
        if !inside_quote && matches!(b, b',' | b')' | b']' | b'>') {
            break;
        }
        i += 1;
    }
    if i == 0 {
        None
    } else {
        Some(i)
    }
}

/// Whether `b` is plausibly part of a path segment (not a separator we use to
/// bound a run). Conservative: alnum, common filename punctuation, and the path
/// separators themselves.
fn is_path_char(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'/' | b'\\'
                | b'.'
                | b'_'
                | b'-'
                | b'~'
                | b'$'
                | b'%'
                | b'+'
                | b'@'
                | b'#'
                | b'&'
                | b'('
                | b')'
        )
}

/// UTF-8 leading-byte -> char byte length (1..=4); defaults to 1 for a stray
/// continuation byte (we only copy bytes, never split a char incorrectly because
/// path detection only triggers on ASCII path markers).
fn utf8_char_len(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead >> 5 == 0b110 {
        2
    } else if lead >> 4 == 0b1110 {
        3
    } else if lead >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// Separators that delimit a VALUE inside a single whitespace token, so a secret
/// embedded in a `key=value`, `"key":"value"`, or `key:value` shape can be
/// isolated and redacted (R3-P1-2). The token has already been split on
/// whitespace, so whitespace is not in this set.
const VALUE_SEPARATORS: &[char] = &[
    '=', ':', '"', '\'', ',', '{', '}', '[', ']', '(', ')', '<', '>', ';', '&', '?',
];

/// Redact one whitespace-delimited token. Absolute paths are handled by the
/// whole-line path-run scrub (R2-P1-4) before this runs, so this no longer needs
/// the bare-path token rule.
///
/// R3-P1-2: a secret often rides INSIDE a `key=value` / JSON shape - e.g.
/// `refresh_token=1//abc`, `"access_token":"ya29.xyz"`, `file_id=<long-id>` -
/// where the WHOLE whitespace token does NOT start with `ya29.` / `1//`, so a
/// prefix-anchored check on the token would leak the value into the (shareable)
/// diagnostic bundle. This therefore splits the token on value separators
/// (`=`, `:`, quotes, brackets, ...) and redacts each VALUE segment that matches
/// a secret pattern, re-emitting the separators verbatim so the surrounding
/// structure (the `key=` part) is preserved for debugging.
fn redact_token(tok: &str) -> String {
    if tok.is_empty() {
        return tok.to_string();
    }
    let mut out = String::with_capacity(tok.len());
    let mut segment = String::new();
    for ch in tok.chars() {
        if VALUE_SEPARATORS.contains(&ch) {
            if !segment.is_empty() {
                out.push_str(&redact_value(&segment));
                segment.clear();
            }
            out.push(ch);
        } else {
            segment.push(ch);
        }
    }
    if !segment.is_empty() {
        out.push_str(&redact_value(&segment));
    }
    out
}

/// Redact a single VALUE segment (already isolated from its `key=` / quoting by
/// [`redact_token`]) if it looks secret, else return it unchanged.
///
/// Handles: emails (`@` + a later `.`), OAuth access tokens (`ya29.` prefix),
/// OAuth refresh tokens (`1//` prefix), and long opaque Drive file/object ids
/// (>= 24 url-safe chars). The token prefixes are checked at the START of the
/// SEGMENT (not the whole whitespace token), so `refresh_token=1//abc` redacts
/// the `1//abc` segment while leaving `refresh_token=` intact.
fn redact_value(seg: &str) -> String {
    if seg.is_empty() {
        return seg.to_string();
    }
    // Email: contains '@' and a '.' after it.
    if let Some(at) = seg.find('@') {
        if seg[at + 1..].contains('.') {
            return format!("<email:{}>", stable_hash(seg));
        }
    }
    // OAuth tokens: Google access tokens start `ya29.`; refresh tokens `1//`.
    if seg.starts_with("ya29.") || seg.starts_with("1//") {
        return "<token-redacted>".to_string();
    }
    // Long opaque ids (Drive file ids are ~28-44 url-safe chars): redact a long
    // run of id-shaped characters.
    if seg.len() >= 24
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return format!("<fileid:{}>", stable_hash(seg));
    }
    seg.to_string()
}

/// A short OS descriptor for `os.txt` (SPEC s18). Compile-time `OS`/`ARCH`
/// constants - no host probe, no secret content.
fn os_descriptor() -> String {
    format!(
        "os={}\narch={}\nfamily={}\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY
    )
}

/// The SPEC s18 redacted settings document. Mirrors [`SettingsDto`] but drops /
/// hashes every secret-bearing field (the telemetry `install_id` is a stable
/// hashed id; the account-identifying fields the spec lists - emails, client
/// ids, drive folder names - do not live in the settings KV, so the only
/// redaction here is the install id).
#[derive(serde::Serialize)]
struct RedactedSettings {
    global: GlobalSettings,
    telemetry: RedactedTelemetry,
    updater: UpdaterSettings,
    ui: UiSettings,
    #[serde(skip_serializing_if = "Option::is_none")]
    windows: Option<WindowsSettings>,
}

/// Telemetry settings with the stable `install_id` replaced by a per-bundle
/// hash so a shared bundle never carries the raw install identifier (SPEC s18).
#[derive(serde::Serialize)]
struct RedactedTelemetry {
    enabled: bool,
    install_id: String,
    endpoint: String,
}

/// Redact the secret-bearing fields of a [`SettingsDto`] for the bundle
/// (SPEC s18): the telemetry install id becomes `installid_<hash>`.
fn redact_settings(s: &SettingsDto) -> RedactedSettings {
    RedactedSettings {
        global: s.global.clone(),
        telemetry: RedactedTelemetry {
            enabled: s.telemetry.enabled,
            install_id: format!("installid_{}", stable_hash(&s.telemetry.install_id)),
            endpoint: s.telemetry.endpoint.clone(),
        },
        updater: s.updater.clone(),
        ui: s.ui.clone(),
        windows: s.windows.clone(),
    }
}

/// A short, stable, NON-reversible hex hash of `input` for the bundle's redacted
/// ids (SPEC s18 `<hash>` placeholders). A 64-bit FNV-1a digest rendered as hex:
/// good enough to correlate occurrences WITHIN one bundle without exposing the
/// original value, with no crypto dep.
fn stable_hash(input: &str) -> String {
    // FNV-1a 64-bit.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Build the `schema.txt` summary (SPEC s18): the REAL `PRAGMA user_version` +
/// a row count per known table. Best-effort: a value that cannot be read is
/// rendered as `?` rather than failing the whole bundle (a partial schema
/// summary is still useful to a maintainer). The values are pure metadata (no
/// file paths / drive ids), so no redaction is needed.
async fn build_schema_summary(state: &dyn StateRepo) -> String {
    let mut out = String::new();
    out.push_str("# Driven state DB schema summary (SPEC s18)\n");
    // SPEC s18: the REAL schema version from `PRAGMA user_version` (now exposed
    // on the StateRepo trait).
    match state.schema_version().await {
        Ok(v) => out.push_str(&format!("user_version={v}\n")),
        Err(e) => out.push_str(&format!("user_version=? ({e})\n")),
    }
    // R2-P2-4: count EVERY known state table (not just accounts + backup_sources)
    // so the bundle is useful for corruption / debug cases. The list is the
    // migration-defined authoritative set; a table whose count cannot be read is
    // rendered `?` rather than failing the whole bundle.
    for table in driven_core::state::KNOWN_STATE_TABLES {
        match state.table_row_count(table).await {
            Ok(n) => out.push_str(&format!("{table}={n}\n")),
            Err(e) => out.push_str(&format!("{table}=? ({e})\n")),
        }
    }
    out
}

/// The SPEC s18 redaction-policy text shipped in the bundle so the recipient
/// understands its threat model.
const REDACTION_POLICY: &str = "\
Driven diagnostic bundle - redaction policy (SPEC s18)
======================================================

This bundle is REASONABLY SAFE TO SHARE WITH THE DRIVEN MAINTAINER. It is NOT
safe to publish to the internet.

What this bundle contains:
- version.txt  : the Driven build version.
- os.txt       : the OS / arch / family (compile-time constants).
- settings_redacted.json : your settings, with secrets removed (see below).
- schema.txt   : state-DB schema version (PRAGMA user_version) + table counts.
- activity_last_30d.csv : the last 30 days of the activity log, with the
  free-text message + source id passed through the redaction pipeline below.
- logs/        : recent tracing output, passed through the redaction pipeline.
- crashes/     : crash dumps (crash-*.txt), passed through the redaction pipeline.
- redaction-policy.txt : this file.

What is redacted / never included:
- OAuth refresh + access tokens (keychain-only; never read into this bundle; any
  token-shaped string in a log line is replaced with <token-redacted>).
- Account encryption master keys + per-source keys (keychain-only).
- BIP39 recovery phrases (never persisted; never collected).
- The telemetry install id is replaced by a stable per-value hash
  (installid_<hash>) so occurrences can be correlated without exposing the id.
- In logs / crashes / activity: local paths become <path:<hash>>, Drive file
  ids become <fileid:<hash>>, email addresses become <email:<hash>> - each a
  stable per-value hash so occurrences correlate without exposing the original.

Caveat: despite this policy, free-text you supplied (e.g. a display name) is not
guaranteed to be scrubbed if it appears in a field this policy did not anticipate.
Review the bundle before sharing if that concerns you.
";

// ---------------------------------------------------------------------------
// check_for_updates + list_releases (SPEC s11.6, s15; GitHub releases backend)
// ---------------------------------------------------------------------------

/// `check_for_updates()` - is a newer release available on the active channel?
/// (SPEC s11.6, s15.)
///
/// Queries the GitHub releases API (ROADMAP M6: real release source; the M9
/// Tauri `update.json` manifest is NOT used here - it does not exist until the
/// M9 release pipeline lands). Picks the newest release for the active channel
/// (`stable` skips pre-releases; `dev` includes them), parses its tag as semver,
/// and returns `Some(UpdateInfo)` only when it is STRICTLY newer than the running
/// build. Returns `None` when up to date / no eligible release.
#[tauri::command]
pub async fn check_for_updates(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<Option<UpdateInfo>> {
    let channel = active_channel(state.state().as_ref()).await?;
    let want_prerelease = channel == "dev";

    // The running version (the build's package version).
    let current = app.package_info().version.to_string();
    let current = semver::Version::parse(&current).map_err(|e| {
        CommandError::with_code(
            ErrorCode::InternalBug,
            format!("current version `{current}` is not semver: {e}"),
        )
    })?;

    let ca = load_custom_ca_config(state.state().as_ref()).await?;
    let releases = fetch_releases(1, &ca).await?;

    // Newest channel-eligible release with a parseable semver tag.
    let mut best: Option<(semver::Version, GithubRelease)> = None;
    for rel in releases {
        if !release_eligible(&rel, want_prerelease) {
            continue;
        }
        let Some(ver) = parse_tag(&rel.tag_name) else {
            continue;
        };
        let is_better = match &best {
            Some((best_ver, _)) => ver > *best_ver,
            None => true,
        };
        if is_better {
            best = Some((ver, rel));
        }
    }

    let Some((ver, rel)) = best else {
        return Ok(None);
    };
    if ver <= current {
        return Ok(None);
    }
    Ok(Some(UpdateInfo {
        version: ver.to_string(),
        notes: Some(rel.body).filter(|b| !b.is_empty()),
        published_at: rel.published_at,
        channel,
    }))
}

/// `list_releases(page)` - list published releases for the About tab's
/// release-notes viewer (SPEC s11.6).
///
/// Fetches a page of releases from the GitHub releases API (1-based `page`,
/// [`RELEASES_PER_PAGE`] per page), filters to the active channel (`stable`
/// drops pre-releases; `dev` includes them) and to non-draft releases, and maps
/// each to a [`ReleaseDto`].
#[tauri::command]
pub async fn list_releases(
    state: State<'_, AppState>,
    page: u32,
) -> CommandResult<Vec<ReleaseDto>> {
    let channel = active_channel(state.state().as_ref()).await?;
    let want_prerelease = channel == "dev";

    // The webview paginates from 1; GitHub's `page` is also 1-based. A `0` from a
    // buggy caller is clamped to the first page.
    let page = page.max(1);
    let ca = load_custom_ca_config(state.state().as_ref()).await?;
    let releases = fetch_releases(page, &ca).await?;

    let dtos = releases
        .into_iter()
        .filter(|r| release_eligible(r, want_prerelease))
        .map(release_to_dto)
        .collect();
    Ok(dtos)
}

/// Issue #34: validate a candidate custom-root-CA PEM file for the settings UI.
///
/// Returns the number of certificates the file contains (so the UI can show
/// "trusts N certificate(s)"), or a `internal.invalid_input` error carrying the
/// read/parse detail. This only READS the user-supplied path to parse it - it
/// returns a count, never file contents - and applies the exact same
/// fail-closed rules as the client-build path, so "valid here" implies "will not
/// fail-closed at build". A blank path is rejected (the UI only validates a
/// non-empty candidate).
#[tauri::command]
pub async fn validate_custom_ca(path: String) -> CommandResult<CustomCaValidation> {
    let normalized = normalize_ca_path(Some(PathBuf::from(path)));
    let Some(path) = normalized else {
        return Err(invalid_setting("no custom root CA path was provided"));
    };
    let count = driven_tls::validate_ca_file(&path)
        .map_err(|e| invalid_setting(format!("custom root CA file is not usable: {e}")))?;
    Ok(CustomCaValidation {
        // A PEM cannot realistically hold > u32::MAX certs; clamp defensively.
        cert_count: u32::try_from(count).unwrap_or(u32::MAX),
    })
}

/// Read the active updater channel (`stable` | `dev`) from the persisted
/// `updater` settings (SPEC s22), defaulting to `stable`.
async fn active_channel(state: &dyn StateRepo) -> CommandResult<String> {
    Ok(load_group::<storage::Updater>(state, KEY_UPDATER)
        .await?
        .map(|u| u.channel)
        .unwrap_or_else(|| default_updater().channel))
}

/// One release as returned by the GitHub releases API (the subset Driven needs).
#[derive(Debug, Clone, Deserialize)]
struct GithubRelease {
    /// The git tag (e.g. `v0.1.2` / `0.1.2`).
    tag_name: String,
    /// The release title; falls back to the tag when GitHub leaves it empty.
    #[serde(default)]
    name: Option<String>,
    /// The release notes body (markdown); may be empty.
    #[serde(default)]
    body: String,
    /// RFC3339 publish timestamp; `None` for an unpublished draft.
    #[serde(default)]
    published_at: Option<String>,
    /// The release page URL.
    #[serde(default)]
    html_url: String,
    /// Draft (unpublished) - excluded from both surfaces.
    #[serde(default)]
    draft: bool,
    /// Pre-release - included only on the `dev` channel.
    #[serde(default)]
    prerelease: bool,
}

/// Is `release` eligible for a channel that does (`want_prerelease`) or does not
/// want pre-releases? A draft is never eligible; a pre-release is eligible only
/// on the `dev` channel. Shared by both surfaces so the filter is identical.
fn release_eligible(release: &GithubRelease, want_prerelease: bool) -> bool {
    !release.draft && (want_prerelease || !release.prerelease)
}

/// Map a [`GithubRelease`] to the frozen [`ReleaseDto`] (About tab). The display
/// name falls back to the tag; an unpublished `published_at` becomes empty.
fn release_to_dto(r: GithubRelease) -> ReleaseDto {
    ReleaseDto {
        version: r.tag_name.clone(),
        name: r.name.filter(|n| !n.is_empty()).unwrap_or(r.tag_name),
        notes: r.body,
        published_at: r.published_at.unwrap_or_default(),
        url: r.html_url,
    }
}

/// Parse a release tag as semver, tolerating a leading `v` (`v0.1.2` ->
/// `0.1.2`). Returns `None` for a tag that is not semver (skipped by the
/// callers).
fn parse_tag(tag: &str) -> Option<semver::Version> {
    let trimmed = tag.strip_prefix('v').unwrap_or(tag);
    semver::Version::parse(trimmed).ok()
}

/// Fetch one page of releases from the GitHub releases API for [`GITHUB_REPO`].
///
/// Uses the shared workspace `reqwest` (rustls). The `json` reqwest feature is
/// not enabled workspace-wide, so the body is read as text + parsed with
/// `serde_json`. A network / non-2xx outcome maps to a SPEC s24 update code so
/// the About tab can show the right i18n message:
/// - a transport (connect/timeout/DNS) failure -> `update.endpoint_unreachable`;
/// - a non-2xx HTTP status -> `update.endpoint_unreachable` (the release source
///   is effectively unavailable).
async fn fetch_releases(
    page: u32,
    ca: &driven_tls::CustomCaConfig,
) -> CommandResult<Vec<GithubRelease>> {
    let url = format!(
        "https://api.github.com/repos/{GITHUB_REPO}/releases?per_page={RELEASES_PER_PAGE}&page={page}"
    );

    // R4-P2-5: bound the request so a blackholed GitHub endpoint cannot hang the
    // IPC command forever (no timeout = wait indefinitely). 10s connect, 30s
    // total. Issue #34: add the user's custom root CA additively (fail-closed).
    let builder = reqwest::Client::builder()
        .user_agent(GITHUB_USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30));
    let client = driven_tls::apply_custom_ca(builder, ca)
        .map_err(|e| {
            CommandError::with_code(
                ErrorCode::InternalBug,
                format!("custom root CA could not be applied: {e}"),
            )
        })?
        .build()
        .map_err(|e| {
            CommandError::with_code(ErrorCode::InternalBug, format!("build http client: {e}"))
        })?;

    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|e| {
            CommandError::with_code(
                ErrorCode::UpdateEndpointUnreachable,
                format!("could not reach the GitHub releases API: {e}"),
            )
        })?;

    if !resp.status().is_success() {
        return Err(CommandError::with_code(
            ErrorCode::UpdateEndpointUnreachable,
            format!(
                "GitHub releases API returned HTTP {}",
                resp.status().as_u16()
            ),
        ));
    }

    let body = resp.text().await.map_err(|e| {
        CommandError::with_code(
            ErrorCode::UpdateEndpointUnreachable,
            format!("could not read the GitHub releases response: {e}"),
        )
    })?;

    serde_json::from_str::<Vec<GithubRelease>>(&body).map_err(|e| {
        CommandError::with_code(
            ErrorCode::InternalBug,
            format!("could not parse the GitHub releases response: {e}"),
        )
    })
}

// ---------------------------------------------------------------------------
// SPEC s22 defaults (used when a KV group is absent from the table)
// ---------------------------------------------------------------------------

fn default_global() -> GlobalSettings {
    GlobalSettings {
        // Default ON (issue #58): launch Driven at login. Mirrors migration
        // 0005, which flips the seeded `global` blob's flag to `true`; this
        // code default applies only when the `global` group is entirely absent.
        auto_start_on_login: true,
        default_concurrent_uploads: None,
        // Adaptive parallelism ships default-on (DESIGN s11.4.7).
        adaptive_parallelism_enabled: true,
        bandwidth_cap_mbps: None,
        skip_on_battery: true,
        skip_on_metered: true,
        scan_interval_secs: 600,
        deep_verify_interval_secs: 604_800,
        io_priority: "low".to_string(),
        log_level: "info".to_string(),
        schedule: default_schedule(),
        pre_backup_hook: None,
        post_backup_hook: None,
        hook_timeout_secs: 60,
        metered_mode: "pause".to_string(),
        metered_bandwidth_cap_mbps: None,
        // Issue #34: system trust only by default (no custom corporate CA).
        custom_root_ca_path: None,
    }
}

/// The disabled schedule (V1 behaviour: sync at any time). All seven days are
/// pre-checked so a user who only flips `enabled` gets a sane "all day, every
/// day" window to narrow.
fn default_schedule() -> ScheduleSettings {
    ScheduleSettings {
        enabled: false,
        start_minute: 0,
        end_minute: 0,
        days: vec![true; 7],
        utc_offset_minutes: 0,
    }
}

fn default_telemetry() -> TelemetrySettings {
    TelemetrySettings {
        enabled: true,
        install_id: String::new(),
        endpoint: "https://driven.maxhogan.dev/telemetry/v1/ping".to_string(),
    }
}

fn default_updater() -> UpdaterSettings {
    UpdaterSettings {
        channel: "stable".to_string(),
        check_interval_secs: 21_600,
    }
}

fn default_ui() -> UiSettings {
    UiSettings {
        tray_left_click_opens: "activity".to_string(),
        locale: "en-US".to_string(),
        color_mode: "system".to_string(),
    }
}

fn default_windows() -> WindowsSettings {
    WindowsSettings {
        vss_mode: "auto".to_string(),
        vss_helper: false,
    }
}

// ---------------------------------------------------------------------------
// Minimal STORED-method ZIP writer (no compression).
// ---------------------------------------------------------------------------

/// A tiny ZIP writer producing a valid STORED (method 0, no compression)
/// archive: per entry a local file header followed by the raw bytes, then a
/// central directory and an end-of-central-directory record. Driven's diagnostic
/// bundle is a handful of small text/JSON files, so the no-compression
/// simplicity (and zero extra transitive deps vs the `zip` crate) is the right
/// trade. The format is the well-specified PKWARE APPNOTE; CRC32 is computed via
/// `crc32fast`.
struct ZipWriter {
    /// The growing archive buffer (local headers + data so far).
    buf: Vec<u8>,
    /// Central-directory records, appended after all local entries.
    central: Vec<u8>,
    /// One entry's bookkeeping for its central-directory record.
    entries: Vec<ZipEntryMeta>,
}

/// Per-entry metadata captured at `add_file` time for the central directory.
struct ZipEntryMeta {
    name: String,
    crc32: u32,
    size: u32,
    local_header_offset: u32,
}

impl ZipWriter {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            central: Vec::new(),
            entries: Vec::new(),
        }
    }

    /// Append one STORED file entry (`name` -> `data`).
    fn add_file(&mut self, name: &str, data: &[u8]) {
        let crc32 = crc32fast::hash(data);
        // `u32` is sufficient: the diagnostic bundle's files are tiny (KBs).
        let size = data.len() as u32;
        let local_header_offset = self.buf.len() as u32;
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len() as u16;

        // --- Local file header (signature 0x04034b50) ---
        self.buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        self.buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // general purpose flag
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // method 0 = stored
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // mod time
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // mod date
        self.buf.extend_from_slice(&crc32.to_le_bytes());
        self.buf.extend_from_slice(&size.to_le_bytes()); // compressed size
        self.buf.extend_from_slice(&size.to_le_bytes()); // uncompressed size
        self.buf.extend_from_slice(&name_len.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // extra field len
        self.buf.extend_from_slice(name_bytes);
        self.buf.extend_from_slice(data);

        self.entries.push(ZipEntryMeta {
            name: name.to_string(),
            crc32,
            size,
            local_header_offset,
        });
    }

    /// Emit the central directory + end-of-central-directory record and return
    /// the finished archive bytes.
    fn finish(mut self) -> Vec<u8> {
        let central_dir_offset = self.buf.len() as u32;

        for e in &self.entries {
            let name_bytes = e.name.as_bytes();
            let name_len = name_bytes.len() as u16;
            // --- Central directory file header (signature 0x02014b50) ---
            self.central
                .extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            self.central.extend_from_slice(&20u16.to_le_bytes()); // version made by
            self.central.extend_from_slice(&20u16.to_le_bytes()); // version needed
            self.central.extend_from_slice(&0u16.to_le_bytes()); // flag
            self.central.extend_from_slice(&0u16.to_le_bytes()); // method 0
            self.central.extend_from_slice(&0u16.to_le_bytes()); // mod time
            self.central.extend_from_slice(&0u16.to_le_bytes()); // mod date
            self.central.extend_from_slice(&e.crc32.to_le_bytes());
            self.central.extend_from_slice(&e.size.to_le_bytes()); // compressed
            self.central.extend_from_slice(&e.size.to_le_bytes()); // uncompressed
            self.central.extend_from_slice(&name_len.to_le_bytes());
            self.central.extend_from_slice(&0u16.to_le_bytes()); // extra len
            self.central.extend_from_slice(&0u16.to_le_bytes()); // comment len
            self.central.extend_from_slice(&0u16.to_le_bytes()); // disk number
            self.central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            self.central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            self.central
                .extend_from_slice(&e.local_header_offset.to_le_bytes());
            self.central.extend_from_slice(name_bytes);
        }

        let central_dir_size = self.central.len() as u32;
        let entry_count = self.entries.len() as u16;

        // Append the central directory, then the EOCD record.
        self.buf.extend_from_slice(&self.central);

        // --- End of central directory (signature 0x06054b50) ---
        self.buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // this disk
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // central dir disk
        self.buf.extend_from_slice(&entry_count.to_le_bytes()); // entries this disk
        self.buf.extend_from_slice(&entry_count.to_le_bytes()); // entries total
        self.buf.extend_from_slice(&central_dir_size.to_le_bytes());
        self.buf
            .extend_from_slice(&central_dir_offset.to_le_bytes());
        self.buf.extend_from_slice(&0u16.to_le_bytes()); // comment len

        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_core::state::sqlite::SqliteStateRepo;

    /// A temp-backed state repo with the SPEC s22 settings seeded (migrations run
    /// on open), for the round-trip tests (no real Drive / keychain touched). The
    /// returned `PathBuf` is the temp dir, cleaned up by the caller. Uses a
    /// hand-rolled temp dir so src-tauri needs no `tempfile` dev-dep.
    async fn seeded_repo() -> (SqliteStateRepo, PathBuf) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("driven-settings-test-{nonce}-{:p}", &nonce));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let repo = SqliteStateRepo::open(&dir.join("state.db"))
            .await
            .expect("open seeded state repo");
        (repo, dir)
    }

    /// Best-effort cleanup of a test temp dir.
    fn cleanup(dir: PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn get_settings_reads_seeded_groups() {
        let (repo, dir) = seeded_repo().await;
        let dto = load_settings_dto(&repo).await.expect("load settings");
        // Seeded SPEC s22 defaults round-trip. Auto-start defaults ON after
        // migration 0005 (issue #58).
        assert!(dto.global.auto_start_on_login);
        assert_eq!(dto.global.scan_interval_secs, 600);
        assert_eq!(dto.updater.channel, "stable");
        assert_eq!(dto.ui.locale, "en-US");
        assert!(dto.telemetry.enabled);
        // `windows` is Some on Windows (defaulting to auto), None elsewhere.
        if cfg!(windows) {
            assert_eq!(
                dto.windows.as_ref().map(|w| w.vss_mode.as_str()),
                Some("auto")
            );
        } else {
            assert!(dto.windows.is_none());
        }
        cleanup(dir);
    }

    #[tokio::test]
    async fn migration_0005_defaults_autostart_on_for_new_installs() {
        // A freshly migrated DB (0002 seeds the `global` blob, 0005 flips the
        // flag) must report auto-start ON - the issue #58 headline default.
        let (repo, dir) = seeded_repo().await;
        let raw = repo
            .get_setting(KEY_GLOBAL)
            .await
            .expect("read global")
            .expect("global seeded");
        let stored: storage::Global = serde_json::from_value(raw).expect("parse stored global");
        assert!(
            stored.auto_start_on_login,
            "migration 0005 must flip the seeded global auto_start_on_login to true"
        );
        // And it surfaces through the DTO read path the IPC layer uses.
        let dto = load_settings_dto(&repo).await.expect("load settings");
        assert!(dto.global.auto_start_on_login);
        cleanup(dir);
    }

    #[test]
    fn default_global_defaults_autostart_on() {
        // The code default (used when the `global` group is entirely absent)
        // mirrors migration 0005 so an unseeded read still defaults ON.
        assert!(default_global().auto_start_on_login);
    }

    #[test]
    fn autostart_reconcile_action_only_acts_on_drift() {
        // No-op when the OS state already matches the preference.
        assert_eq!(autostart_reconcile_action(true, true), None);
        assert_eq!(autostart_reconcile_action(false, false), None);
        // Default ON but OS entry missing -> register (the issue #58 case the
        // boot reconciliation must fix).
        assert_eq!(autostart_reconcile_action(true, false), Some(true));
        // Preference off but OS entry present -> unregister.
        assert_eq!(autostart_reconcile_action(false, true), Some(false));
    }

    #[tokio::test]
    async fn update_settings_merge_round_trips_a_single_field() {
        let (repo, dir) = seeded_repo().await;
        // Patch just one global field; the merge must persist it and leave the
        // rest of the group intact.
        let patch_global = crate::commands::dtos::GlobalSettingsPatch {
            scan_interval_secs: Some(900),
            skip_on_battery: Some(false),
            ..Default::default()
        };

        // Apply the global-group merge directly (the side-effecting command
        // needs an AppHandle; the persisted merge is what the round-trip tests).
        let mut cur: GlobalSettings = load_group::<storage::Global>(&repo, KEY_GLOBAL)
            .await
            .unwrap()
            .map(Into::into)
            .unwrap();
        if let Some(v) = patch_global.scan_interval_secs {
            cur.scan_interval_secs = v;
        }
        if let Some(v) = patch_global.skip_on_battery {
            cur.skip_on_battery = v;
        }
        store_group(&repo, KEY_GLOBAL, &storage::Global::from(cur))
            .await
            .unwrap();

        let dto = load_settings_dto(&repo).await.unwrap();
        assert_eq!(
            dto.global.scan_interval_secs, 900,
            "patched field persisted"
        );
        assert!(!dto.global.skip_on_battery, "patched field persisted");
        // Untouched fields keep their seeded defaults.
        assert!(dto.global.skip_on_metered, "untouched field unchanged");
        assert_eq!(dto.global.log_level, "info", "untouched field unchanged");
        cleanup(dir);
    }

    #[test]
    fn vss_helper_status_degrades_only_on_windows_when_unelevated() {
        // Off Windows: never supported, never degraded.
        let off = compute_vss_helper_status(false, false, false, false, false, false, false);
        assert!(!off.supported);
        assert!(!off.locked_file_backup_degraded);

        // Windows + elevated: supported, not degraded (in-process VSS works).
        let elevated = compute_vss_helper_status(true, true, false, false, false, false, false);
        assert!(elevated.supported);
        assert!(elevated.elevated);
        assert!(!elevated.locked_file_backup_degraded);

        // Windows + un-elevated + no helper: supported but DEGRADED (locked files
        // skipped).
        let degraded = compute_vss_helper_status(true, false, false, false, false, false, false);
        assert!(degraded.supported);
        assert!(!degraded.elevated);
        assert!(degraded.locked_file_backup_degraded);

        // The helper-enabled preference is surfaced verbatim.
        assert!(
            compute_vss_helper_status(true, false, true, false, false, false, false).helper_enabled
        );
    }

    /// Issue #25: truthful liveness. A LAUNCHABLE helper (bundled sidecar present,
    /// no prior launch failure) lifts the degrade even before the lazy launch, and
    /// an already-LAUNCHED helper does too - so an un-elevated Windows app with the
    /// helper in play is NOT reported degraded.
    #[test]
    fn vss_helper_status_not_degraded_when_helper_launchable_or_alive() {
        // Un-elevated + helper launchable (not yet launched): NOT degraded.
        let launchable = compute_vss_helper_status(true, false, true, false, true, false, false);
        assert!(!launchable.elevated);
        assert!(launchable.helper_launchable);
        assert!(!launchable.helper_alive);
        assert!(
            !launchable.locked_file_backup_degraded,
            "a launchable helper means locked-file backup is available on demand"
        );

        // Un-elevated + helper already launched: NOT degraded.
        let alive = compute_vss_helper_status(true, false, true, true, true, false, false);
        assert!(alive.helper_alive);
        assert!(!alive.locked_file_backup_degraded);

        // Un-elevated + helper enabled but NOT launchable (sidecar missing / prior
        // launch failed): DEGRADED - no snapshot path available.
        let stuck = compute_vss_helper_status(true, false, true, false, false, false, false);
        assert!(stuck.helper_enabled);
        assert!(!stuck.helper_launchable);
        assert!(
            stuck.locked_file_backup_degraded,
            "enabled but unlaunchable helper cannot create a snapshot -> degraded"
        );

        // Issue #25 (launch-UX): a PENDING launch (awaiting UAC) surfaces
        // launch_pending; a DECLINED launch surfaces launch_declined + degrades.
        let pending = compute_vss_helper_status(true, false, true, false, true, true, false);
        assert!(pending.launch_pending);
        assert!(
            !pending.locked_file_backup_degraded,
            "a pending launch is still launchable -> not degraded"
        );
        let declined = compute_vss_helper_status(true, false, true, false, false, false, true);
        assert!(declined.launch_declined);
        assert!(
            declined.locked_file_backup_degraded,
            "a declined helper cannot snapshot -> degraded"
        );
    }

    #[tokio::test]
    async fn windows_vss_helper_setting_round_trips() {
        let (repo, dir) = seeded_repo().await;
        // Merge just the vss_helper flag on the windows group.
        let mut cur: WindowsSettings = load_group::<storage::Windows>(&repo, KEY_WINDOWS)
            .await
            .unwrap()
            .map(Into::into)
            .unwrap_or_else(default_windows);
        assert!(!cur.vss_helper, "default is off");
        cur.vss_helper = true;
        store_group(&repo, KEY_WINDOWS, &storage::Windows::from(cur))
            .await
            .unwrap();

        let back: WindowsSettings = load_group::<storage::Windows>(&repo, KEY_WINDOWS)
            .await
            .unwrap()
            .map(Into::into)
            .unwrap();
        assert!(back.vss_helper, "vss_helper persisted");
        // vss_mode is untouched.
        assert_eq!(back.vss_mode, "auto");

        // Issue #25: `load_vss_helper_enabled` (the boot-assembly reader) reflects
        // the persisted flag on Windows; off Windows VSS is unsupported so it is
        // always false regardless of the stored value.
        let enabled = load_vss_helper_enabled(&repo).await;
        assert_eq!(enabled, cfg!(windows));
        cleanup(dir);
    }

    #[tokio::test]
    async fn load_orchestrator_config_reflects_global_gates() {
        let (repo, dir) = seeded_repo().await;
        // Flip the battery gate + scan cadence in the global group.
        let mut cur: GlobalSettings = load_group::<storage::Global>(&repo, KEY_GLOBAL)
            .await
            .unwrap()
            .map(Into::into)
            .unwrap();
        cur.skip_on_battery = false;
        cur.scan_interval_secs = 300;
        cur.bandwidth_cap_mbps = Some(50);
        store_group(&repo, KEY_GLOBAL, &storage::Global::from(cur))
            .await
            .unwrap();

        let config = load_orchestrator_config(&repo).await.unwrap();
        assert!(!config.skip_on_battery);
        assert_eq!(config.scan_interval_secs, 300);
        assert_eq!(config.bandwidth_cap_mbps, Some(50));
        assert!(
            config.skip_on_metered,
            "untouched gate keeps its seeded value"
        );
        cleanup(dir);
    }

    #[tokio::test]
    async fn malformed_settings_value_surfaces_db_corrupt() {
        let (repo, dir) = seeded_repo().await;
        // Write a value that does not match the GlobalSettings schema.
        repo.set_setting(
            KEY_GLOBAL,
            &serde_json::json!({ "not": "a settings group" }),
        )
        .await
        .unwrap();
        let err = load_settings_dto(&repo)
            .await
            .expect_err("malformed must error");
        assert_eq!(err.code, ErrorCode::StateDbCorrupt);
        cleanup(dir);
    }

    #[test]
    fn parse_tag_tolerates_v_prefix_and_rejects_garbage() {
        assert_eq!(parse_tag("v0.1.2"), Some(semver::Version::new(0, 1, 2)));
        assert_eq!(parse_tag("0.1.2"), Some(semver::Version::new(0, 1, 2)));
        assert_eq!(parse_tag("nightly"), None);
        assert_eq!(parse_tag(""), None);
    }

    #[test]
    fn version_compare_decides_newer_release() {
        let current = semver::Version::new(0, 1, 0);
        // A strictly newer tag is an update.
        assert!(parse_tag("v0.2.0").unwrap() > current);
        // The same version is NOT an update.
        assert!(parse_tag("0.1.0").unwrap() <= current);
        // An older tag is NOT an update.
        assert!(parse_tag("v0.0.9").unwrap() <= current);
    }

    #[test]
    fn channel_filter_excludes_prerelease_on_stable_includes_on_dev() {
        let prerelease = GithubRelease {
            tag_name: "v0.2.0-rc1".to_string(),
            name: None,
            body: String::new(),
            published_at: None,
            html_url: String::new(),
            draft: false,
            prerelease: true,
        };
        let stable = GithubRelease {
            prerelease: false,
            ..prerelease.clone()
        };
        let draft = GithubRelease {
            draft: true,
            prerelease: false,
            ..prerelease.clone()
        };
        // stable channel (want_prerelease=false): pre-release excluded, stable in.
        assert!(!release_eligible(&prerelease, false));
        assert!(release_eligible(&stable, false));
        // dev channel (want_prerelease=true): pre-release included.
        assert!(release_eligible(&prerelease, true));
        // A draft is never eligible on either channel.
        assert!(!release_eligible(&draft, false));
        assert!(!release_eligible(&draft, true));
    }

    #[test]
    fn release_to_dto_falls_back_name_to_tag() {
        let r = GithubRelease {
            tag_name: "v1.0.0".to_string(),
            name: None,
            body: "notes".to_string(),
            published_at: Some("2026-01-01T00:00:00Z".to_string()),
            html_url: "https://example/r".to_string(),
            draft: false,
            prerelease: false,
        };
        let dto = release_to_dto(r);
        assert_eq!(dto.version, "v1.0.0");
        assert_eq!(dto.name, "v1.0.0", "empty name falls back to the tag");
        assert_eq!(dto.notes, "notes");
        assert_eq!(dto.published_at, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn redact_settings_hashes_install_id() {
        let dto = SettingsDto {
            global: default_global(),
            telemetry: TelemetrySettings {
                enabled: true,
                install_id: "super-secret-install-id".to_string(),
                endpoint: "https://e".to_string(),
            },
            updater: default_updater(),
            ui: default_ui(),
            windows: None,
            bundle_small_files: false,
        };
        let red = redact_settings(&dto);
        assert!(red.telemetry.install_id.starts_with("installid_"));
        assert!(
            !red.telemetry.install_id.contains("super-secret-install-id"),
            "the raw install id must never appear in the redacted bundle"
        );
        // The same input hashes stably.
        assert_eq!(
            stable_hash("super-secret-install-id"),
            stable_hash("super-secret-install-id")
        );
    }

    #[tokio::test]
    async fn schema_summary_includes_real_user_version() {
        // C3 (SPEC s18): schema.txt must carry the REAL PRAGMA user_version, not
        // the old "not exposed" placeholder.
        let (repo, dir) = seeded_repo().await;
        let summary = build_schema_summary(&repo).await;
        assert!(
            summary.contains("user_version="),
            "schema.txt must record user_version"
        );
        assert!(
            !summary.contains("not exposed"),
            "user_version must be the real value, not a placeholder"
        );
        // The migrated DB has a non-default user_version; assert it parses to an
        // integer (>= 0).
        let line = summary
            .lines()
            .find(|l| l.starts_with("user_version="))
            .unwrap();
        let val = line.trim_start_matches("user_version=");
        assert!(
            val.parse::<i64>().is_ok(),
            "user_version must be an integer, got `{val}`"
        );
        cleanup(dir);
    }

    #[tokio::test]
    async fn activity_csv_has_header_and_redacts_message() {
        // C3 (SPEC s18): activity_last_30d.csv exists with the expected header and
        // its free-text message column is passed through the redaction pipeline.
        use driven_core::state::{ActivityLevel, NewActivity};
        let (repo, dir) = seeded_repo().await;
        // Write an activity row whose message embeds a Unix path + an email; both
        // must be hashed in the CSV. Use a recent ts so it falls in the 30-day
        // window the CSV collects.
        use driven_core::time::{Clock, SystemClock};
        let now = SystemClock.now_ms();
        repo.write_activity(NewActivity {
            ts: now,
            source_id: None,
            level: ActivityLevel::Info,
            event_type: "upload_done".to_string(),
            file_count: Some(3),
            bytes: Some(99),
            message: Some("uploaded /home/secret/file.txt for user@example.com".to_string()),
        })
        .await
        .unwrap();

        let csv = build_activity_csv(&repo, &Redactor::context_free()).await;
        assert!(
            csv.starts_with("ts,event_type,level,source_id,file_count,bytes,message\n"),
            "CSV must start with the SPEC s18 header"
        );
        assert!(csv.contains("upload_done"), "the event row is present");
        assert!(
            !csv.contains("/home/secret/file.txt"),
            "the raw path must be redacted out of the CSV message"
        );
        assert!(
            !csv.contains("user@example.com"),
            "the raw email must be redacted out of the CSV message"
        );
        assert!(
            csv.contains("<path:") && csv.contains("<email:"),
            "the redaction pipeline must replace path + email with hashed placeholders"
        );
        cleanup(dir);
    }

    #[tokio::test]
    async fn activity_csv_pages_through_all_rows_not_just_the_first_page() {
        // M7-R3-P2 (recheck-3): the CSV must carry EVERY activity row in the
        // 30-day window, not just the first page. With a per-file `upload_done`
        // row per upload the log easily exceeds one page; the old single
        // `PageRequest::first(10_000)` silently dropped the rest. Drive the
        // keyset loop with a tiny page size so a handful of rows spans several
        // pages, and assert all rows land in the CSV.
        use driven_core::state::{ActivityLevel, NewActivity};
        use driven_core::time::{Clock, SystemClock};
        let (repo, dir) = seeded_repo().await;
        let now = SystemClock.now_ms();

        // Write 25 recent rows (inside the 30-day window). Distinct messages so
        // each can be located in the output.
        let total_rows = 25_usize;
        for i in 0..total_rows {
            repo.write_activity(NewActivity {
                ts: now - i as i64, // strictly decreasing so ordering is stable
                source_id: None,
                level: ActivityLevel::Info,
                event_type: "upload_done".to_string(),
                file_count: Some(1),
                bytes: Some(i as u64),
                message: Some(format!("row-marker-{i}")),
            })
            .await
            .unwrap();
        }

        // page_size = 4 forces ~7 keyset pages over the 25 rows.
        let csv = build_activity_csv_paged(&repo, &Redactor::context_free(), 4, 1_000_000).await;

        // Every row's marker is present exactly once -> the loop walked all
        // pages and dropped nothing. The message is the LAST CSV column, so each
        // row ends `...,row-marker-{i}\n`; match the marker WITH its trailing
        // newline so `row-marker-1` is not a substring hit inside `row-marker-12`.
        for i in 0..total_rows {
            let marker = format!("row-marker-{i}\n");
            assert_eq!(
                csv.matches(&marker).count(),
                1,
                "row {i} must appear exactly once in the paged CSV"
            );
        }
        // Exactly `total_rows` data rows emitted (one `upload_done` per row in
        // the event_type column - an unambiguous per-row count that, unlike a raw
        // newline count, is not confused by a multi-line quoted message field).
        assert_eq!(
            csv.matches("upload_done").count(),
            total_rows,
            "every data row emitted exactly once across all pages"
        );

        // The row cap is honoured: capping at 10 rows yields exactly 10 data rows.
        let capped = build_activity_csv_paged(&repo, &Redactor::context_free(), 4, 10).await;
        assert_eq!(
            capped.matches("upload_done").count(),
            10,
            "exactly max_rows data rows when the cap is hit"
        );
        cleanup(dir);
    }

    #[test]
    fn redact_log_text_scrubs_tokens_paths_emails_and_ids() {
        // C3 (SPEC s18): the log/crash redaction pipeline.
        let input = "refresh 1//0gAbCdEf access ya29.aBcDeF path C:\\Users\\me\\secret.txt \
                     email alice@example.com id 1A2b3C4d5E6f7G8h9I0jKlMnOpQr";
        let out = Redactor::context_free().redact_text(input);
        assert!(!out.contains("1//0gAbCdEf"), "refresh token redacted");
        assert!(!out.contains("ya29.aBcDeF"), "access token redacted");
        assert!(
            out.contains("<token-redacted>"),
            "tokens become placeholder"
        );
        assert!(
            !out.contains("C:\\Users\\me\\secret.txt"),
            "windows path redacted"
        );
        assert!(out.contains("<path:"), "path becomes a hashed placeholder");
        assert!(!out.contains("alice@example.com"), "email redacted");
        assert!(
            out.contains("<email:"),
            "email becomes a hashed placeholder"
        );
        assert!(
            !out.contains("1A2b3C4d5E6f7G8h9I0jKlMnOpQr"),
            "long opaque id (drive-file-id shaped) redacted"
        );
        assert!(
            out.contains("<fileid:"),
            "long id becomes a hashed placeholder"
        );
    }

    #[test]
    fn redact_log_text_leaves_ordinary_text_alone() {
        let input = "scan complete 12 files 3 dirs ok";
        let out = Redactor::context_free().redact_text(input);
        assert_eq!(out.trim_end(), input, "ordinary log text is unchanged");
    }

    #[test]
    fn redact_tokens_inside_key_equals_and_json_values() {
        // R3-P1-2: a secret riding INSIDE a key=value / JSON value leaks if only
        // the WHOLE whitespace token is checked for the `ya29.` / `1//` prefix.
        // key=value shape (refresh token, access token, drive file id).
        let kv = "refresh_token=1//0gAbCdEfGh access_token=ya29.aBcDeFgHiJ \
                  file_id=1A2b3C4d5E6f7G8h9I0jKlMnOpQr op=upload";
        let out = Redactor::context_free().redact_line(kv);
        assert!(
            !out.contains("1//0gAbCdEfGh"),
            "kv refresh token redacted: {out}"
        );
        assert!(
            !out.contains("ya29.aBcDeFgHiJ"),
            "kv access token redacted: {out}"
        );
        assert!(
            !out.contains("1A2b3C4d5E6f7G8h9I0jKlMnOpQr"),
            "kv drive file id redacted: {out}"
        );
        assert!(
            out.contains("<token-redacted>"),
            "token placeholder present: {out}"
        );
        assert!(
            out.contains("<fileid:"),
            "fileid placeholder present: {out}"
        );
        // The key names + the adjacent op field survive (structure preserved).
        assert!(out.contains("refresh_token="), "key name preserved: {out}");
        assert!(out.contains("op=upload"), "adjacent field survives: {out}");

        // JSON-like shape: quoted keys + quoted values.
        let json = r#"{"access_token":"ya29.JsOnAcCeSs","refresh_token":"1//jSoNrEfReSh"}"#;
        let out = Redactor::context_free().redact_line(json);
        assert!(
            !out.contains("ya29.JsOnAcCeSs"),
            "json access token redacted: {out}"
        );
        assert!(
            !out.contains("1//jSoNrEfReSh"),
            "json refresh token redacted: {out}"
        );
        assert!(
            out.contains("<token-redacted>"),
            "json token placeholder present: {out}"
        );
        // The structural keys survive.
        assert!(out.contains("access_token"), "json key preserved: {out}");
        assert!(out.contains("refresh_token"), "json key preserved: {out}");
    }

    #[test]
    fn redact_non_ascii_path_and_needle_never_panics_and_redacts() {
        // R3-P1-3: a needle / haystack whose case fold changes byte length must
        // not mis-slice or PANIC. The dotted-capital-I and German sharp-s are
        // classic length-changing case folds. The known-substring scrub
        // (replace_ci) must find the needle on a real CHAR boundary. Non-ASCII
        // chars are written as \u{} escapes so this source file stays ASCII;
        // they decode to e=U+00E9, E=U+00C9 (accented e), and sharp-s=U+00DF.
        // jose_lower = "jos\u{e9}" (jose with accented e); strasse = "stra\u{df}e".
        let jose_lower = "jos\u{e9}";
        let jose_upper = "JOS\u{c9}";
        let strasse = "stra\u{df}e";
        // Needle is pre-lowercased by the call site; emulate that here.
        let needle = format!("c:\\users\\{jose_lower}\\{strasse}").to_lowercase();
        let r = Redactor {
            source_roots: vec![needle.clone()],
            home: None,
            username: None,
        };
        // The haystack uses MIXED case (incl. the upper-case forms) so the CI
        // matcher is exercised; the original (case-preserving) bytes are sliced.
        let line = format!("scanning C:\\Users\\{jose_upper}\\STRASSE for changes");
        // First prove the legacy-bug input does not panic and yields a result.
        let out = r.redact_line(&line);
        // STRASSE upper-cases the sharp-s differently, so that run may not match;
        // assert only no-panic + a sane (non-empty) result here.
        assert!(!out.is_empty(), "redaction produced output: {out}");

        // An exact-case occurrence MUST be redacted to a <path:...> placeholder.
        let exact = format!("path C:\\Users\\{jose_lower}\\{strasse} end");
        let out = r.redact_line(&exact);
        assert!(
            !out.contains(jose_lower),
            "non-ascii source root redacted (no panic): {out}"
        );
        assert!(out.contains("<path:"), "hashed placeholder present: {out}");
        assert!(out.contains("end"), "trailing text survives: {out}");

        // replace_ci directly: a length-changing case fold (dotted capital I,
        // U+0130, lowercases to two chars) on a non-char boundary must never
        // panic and must return original-span slices.
        let hay = "PRE_\u{130}stanbul_POST"; // dotted capital I (multi-byte fold)
        let result = replace_ci(hay, &"_istanbul_".to_lowercase(), "<X>");
        assert!(
            result.contains("PRE") && result.contains("POST"),
            "no panic: {result}"
        );

        // A plain ASCII case-insensitive match still works on original spans.
        let result = replace_ci("Hello WORLD hello", "hello", "<H>");
        assert_eq!(
            result, "<H> WORLD <H>",
            "ASCII CI replace on original spans"
        );
    }

    #[test]
    fn deep_verify_interval_validator_rejects_zero_and_over_cap_accepts_valid() {
        // R3-P2-2: the per-source deep_verify_interval validator (shared with
        // update_source) rejects 0 (constant churn) and an over-cap value, and
        // accepts a value within the duration cap.
        let err = validate_deep_verify_interval(0)
            .expect_err("zero deep-verify interval must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        let err = validate_deep_verify_interval(u32::MAX)
            .expect_err("u32::MAX deep-verify interval must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        let err = validate_deep_verify_interval(DEEP_VERIFY_MIN - 1)
            .expect_err("just-below-min must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        let err = validate_deep_verify_interval(DEEP_VERIFY_MAX + 1)
            .expect_err("just-above-max must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        // A valid value (the 7-day default) is accepted; the bounds are inclusive.
        validate_deep_verify_interval(604_800).expect("7-day default is valid");
        validate_deep_verify_interval(DEEP_VERIFY_MIN).expect("min bound is valid");
        validate_deep_verify_interval(DEEP_VERIFY_MAX).expect("max bound is valid");
    }

    // R2-P1-4: whole-line / all-path-shape redaction tests (Windows shapes).

    #[test]
    fn redact_key_equals_path_with_spaces_is_scrubbed() {
        // The exact leak the finding cites: `key=C:\Users\Pat Smith\Taxes\f.pdf`.
        // The whole path (INCLUDING the spaces) must be redacted, and an adjacent
        // log field must survive.
        let input = "uploading path=C:\\Users\\Pat Smith\\Taxes\\file.pdf op=upload";
        let out = Redactor::context_free().redact_line(input);
        assert!(
            !out.contains("Pat Smith"),
            "the path with spaces must be fully redacted: {out}"
        );
        assert!(
            !out.contains("file.pdf"),
            "the filename must be redacted: {out}"
        );
        assert!(
            out.contains("<path:"),
            "becomes a hashed placeholder: {out}"
        );
        assert!(
            out.contains("op=upload"),
            "the adjacent field must survive: {out}"
        );
    }

    #[test]
    fn redact_quoted_path_with_spaces_is_scrubbed() {
        let input = "file \"C:\\Users\\Pat Smith\\My Docs\\taxes.xlsx\" done";
        let out = Redactor::context_free().redact_line(input);
        assert!(!out.contains("Pat Smith"), "quoted path redacted: {out}");
        assert!(
            !out.contains("taxes.xlsx"),
            "quoted filename redacted: {out}"
        );
        assert!(out.contains("<path:"), "hashed placeholder: {out}");
        // The closing context survives.
        assert!(out.contains("done"), "trailing text survives: {out}");
    }

    #[test]
    fn redact_unc_path_is_scrubbed() {
        let input = "share \\\\server\\share\\private\\report.docx end";
        let out = Redactor::context_free().redact_line(input);
        assert!(!out.contains("server"), "UNC host redacted: {out}");
        assert!(!out.contains("report.docx"), "UNC filename redacted: {out}");
        assert!(out.contains("<path:"), "hashed placeholder: {out}");
        assert!(out.contains("end"), "trailing text survives: {out}");
    }

    #[tokio::test]
    async fn redact_known_source_root_substring_is_scrubbed() {
        // R2-P1-4: a configured source root (with spaces) is scrubbed wherever it
        // appears, even embedded in a longer string the path scanner might miss.
        use driven_core::state::{AccountRow, SourceRow};
        use driven_core::types::{AccountId, AccountState, SourceId};
        let (repo, dir) = seeded_repo().await;
        let account_id = AccountId::new_v4();
        repo.upsert_account(&AccountRow {
            id: account_id,
            email: "u@example.com".to_string(),
            display_name: None,
            state: AccountState::Ok,
            encryption_master_key_id: None,
            created_at: 0,
            last_synced_at: None,
        })
        .await
        .unwrap();
        let root = "D:\\Backups\\Family Photos";
        repo.upsert_source(&SourceRow {
            id: SourceId::new_v4(),
            account_id,
            display_name: "photos".to_string(),
            enabled: true,
            local_path: root.to_string(),
            drive_folder_id: String::new(),
            drive_id: None,
            drive_folder_path: String::new(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore: true,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            mtime_granularity_ns: None,
            created_at: 0,
        })
        .await
        .unwrap();

        let redactor = Redactor::for_bundle(&repo).await;
        // The source root appears mid-message (the path scanner alone, without a
        // left boundary at line start, still works, but this also proves the
        // known-substring scrub catches the exact configured root with spaces).
        let out = redactor.redact_line("scanning D:\\Backups\\Family Photos for changes");
        assert!(
            !out.contains("Family Photos"),
            "the configured source root must be redacted: {out}"
        );
        assert!(out.contains("<path:"), "hashed placeholder: {out}");
        cleanup(dir);
    }

    #[test]
    fn settings_validators_reject_out_of_range_and_invalid_enum() {
        // R2-P2-3: the backend bounds numeric settings and validates enums,
        // returning the stable `internal.invalid_input` code.
        // Numeric out-of-range.
        let err = check_range(
            "scan_interval_secs",
            0,
            SCAN_INTERVAL_MIN,
            SCAN_INTERVAL_MAX,
        )
        .expect_err("zero scan interval must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        let err = check_range(
            "bandwidth_cap_mbps",
            BANDWIDTH_CAP_MAX + 1,
            BANDWIDTH_CAP_MIN,
            BANDWIDTH_CAP_MAX,
        )
        .expect_err("huge bandwidth cap must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        // In-range is accepted.
        check_range(
            "scan_interval_secs",
            600,
            SCAN_INTERVAL_MIN,
            SCAN_INTERVAL_MAX,
        )
        .expect("default scan interval is valid");

        // Invalid enum.
        let err = check_enum("log_level", "verbose", LOG_LEVELS)
            .expect_err("an invalid log level must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        let err = check_enum("channel", "nightly", UPDATE_CHANNELS)
            .expect_err("an invalid channel must be rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        let err =
            check_enum("vss_mode", "sometimes", VSS_MODES).expect_err("invalid vss_mode rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        // Valid enums accepted.
        check_enum("log_level", "info", LOG_LEVELS).expect("info is a valid level");
        check_enum("channel", "dev", UPDATE_CHANNELS).expect("dev is a valid channel");

        // Locale: well-formed accepted, garbage rejected.
        check_locale("en-US").expect("en-US is well formed");
        check_locale("zh-Hant-TW").expect("multi-subtag is well formed");
        let err = check_locale("../etc/passwd").expect_err("a path-shaped locale is rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
        let err = check_locale("").expect_err("an empty locale is rejected");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    // A single self-signed test CA (RSA-2048, CN "Driven Test CA 1"), embedded so
    // the custom-CA settings tests need no runtime cert generation.
    const TEST_CA_PEM: &str = concat!(
        "-----BEGIN CERTIFICATE-----\n",
        "MIIDFzCCAf+gAwIBAgIUB5Q41gPo/wu/gcL39WRKnSuXLUYwDQYJKoZIhvcNAQEL\n",
        "BQAwGzEZMBcGA1UEAwwQRHJpdmVuIFRlc3QgQ0EgMTAeFw0yNjA3MjAxNTQ4NTFa\n",
        "Fw0zNjA3MTcxNTQ4NTFaMBsxGTAXBgNVBAMMEERyaXZlbiBUZXN0IENBIDEwggEi\n",
        "MA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQDwFFtyR6a9TV01KCQVU68OlKGf\n",
        "YRiXaY+YWc6q0jql65FD7934nEBPNXaDEc/zsxUWqsioyW81gzgbK/RrE98cgSQC\n",
        "tm5fsMPvL8H6nhKQHMuJwBgo4LawGsLqZR2uvICTOPDFw3f7J+/INgNDpJQ+LgOb\n",
        "QqQtjcyHRFcRqhoWspOAdmc5NGKQ5eZxIAxvdK6P5wzbXUoW5xPi6TOLWeuQAn90\n",
        "Bai+mZ0TfnxMauvfC5Mf96K9Y/CRkulRqnddT1KVbmeMhv2ilcOd20rVRu5mq9tb\n",
        "FHmFfsCnbxs0JZA3OC0Fd6lCGgXR4yXxQZWH97WAzZOWVzYE9igGRZ/S38U9AgMB\n",
        "AAGjUzBRMB0GA1UdDgQWBBR/xbCt2uzNY9bEXNd4nydqypUveDAfBgNVHSMEGDAW\n",
        "gBR/xbCt2uzNY9bEXNd4nydqypUveDAPBgNVHRMBAf8EBTADAQH/MA0GCSqGSIb3\n",
        "DQEBCwUAA4IBAQAK1E2Kewr22T/UvhppVdzEtzHFMi4psji31MlA2PfRVR5vhUFz\n",
        "rAaZIBjG7E/3i+LeEKXJd6MZZ6+e0HFo+IGHSEMCLi9DvA+uAQhBflFI8uDBX8rb\n",
        "ewjWzBB4j9JElIuVvUUlhzuWV9DfMGwWyX+8lpnVmpU5vjbb4C0/uSelu6EdoMYE\n",
        "diyL/TNANqgBb+0vuAdO8ua5FPMjerNyIUSZSli9xxaHv82XJC+poD11nwBo8Tsh\n",
        "s5w3VBWjhX/HCnoyVqioMbagxiBz4FzWoJPQjNnDb5LlMmFzGrHSekuem1D9Ol2P\n",
        "TcSAr7WHM8cnvHrbKpGrZGfuL9wI7cnaDPSd\n",
        "-----END CERTIFICATE-----\n",
    );

    #[test]
    fn normalize_ca_path_treats_blank_as_none() {
        // Issue #34: blank / whitespace = system trust only; a real path survives.
        assert_eq!(normalize_ca_path(None), None);
        assert_eq!(normalize_ca_path(Some(PathBuf::from(""))), None);
        assert_eq!(normalize_ca_path(Some(PathBuf::from("   "))), None);
        assert_eq!(
            normalize_ca_path(Some(PathBuf::from("/etc/corp/ca.pem"))),
            Some(PathBuf::from("/etc/corp/ca.pem"))
        );
    }

    #[test]
    fn global_serde_defaults_custom_ca_to_none_for_a_pre_field_blob() {
        // Issue #34: a `global` blob persisted BEFORE the custom_root_ca_path
        // field (no such key) must still deserialise, defaulting to None (system
        // trust only) - not error.
        let mut value =
            serde_json::to_value(storage::Global::from(default_global())).expect("serialize");
        value
            .as_object_mut()
            .expect("object")
            .remove("custom_root_ca_path");
        let restored: storage::Global =
            serde_json::from_value(value).expect("pre-field blob deserialises");
        assert_eq!(restored.custom_root_ca_path, None);
    }

    #[tokio::test]
    async fn validate_custom_ca_reports_count_and_rejects_bad_input() {
        // Issue #34: the settings-UI validation command reports the cert count on
        // a good PEM and the stable invalid-input code on a blank / garbage path.
        let dir = std::env::temp_dir().join(format!("driven-ca-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mk tmp dir");
        let good = dir.join("ca.pem");
        std::fs::write(&good, TEST_CA_PEM).expect("write good pem");
        let garbage = dir.join("garbage.pem");
        std::fs::write(&garbage, b"not a certificate at all\n").expect("write garbage");

        let ok = validate_custom_ca(good.to_string_lossy().into_owned())
            .await
            .expect("a valid PEM validates");
        assert_eq!(ok.cert_count, 1);

        let blank = validate_custom_ca("   ".to_string())
            .await
            .expect_err("blank path rejected");
        assert_eq!(blank.code, ErrorCode::InvalidInput);

        let bad = validate_custom_ca(garbage.to_string_lossy().into_owned())
            .await
            .expect_err("garbage rejected");
        assert_eq!(bad.code, ErrorCode::InvalidInput);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn schema_summary_counts_every_known_state_table() {
        // R2-P2-4: schema.txt must carry a count line for EVERY migration-defined
        // table, not just accounts + backup_sources.
        let (repo, dir) = seeded_repo().await;
        let summary = build_schema_summary(&repo).await;
        for table in driven_core::state::KNOWN_STATE_TABLES {
            assert!(
                summary.contains(&format!("{table}=")),
                "schema.txt must count table `{table}`; got:\n{summary}"
            );
        }
        // A specific non-trivial table beyond the old two must be present.
        assert!(
            summary.contains("file_state=") && summary.contains("pending_ops="),
            "core tables file_state + pending_ops must be counted"
        );
        cleanup(dir);
    }

    #[test]
    fn redact_leaves_non_path_text_unchanged() {
        // R2-P1-4: ordinary non-path text (incl. relative-looking tokens and a
        // bare `/`) must be untouched.
        let input = "scan complete 12 files / 3 dirs ratio=0.5 ok";
        let out = Redactor::context_free().redact_line(input);
        assert_eq!(out, input, "ordinary text with a lone / is unchanged");
    }

    #[test]
    fn zip_writer_emits_a_valid_stored_archive() {
        let mut zip = ZipWriter::new();
        zip.add_file("a.txt", b"hello");
        zip.add_file("b/c.txt", b"world!!");
        let bytes = zip.finish();

        // Local file header + EOCD signatures present.
        assert_eq!(
            &bytes[0..4],
            &0x0403_4b50u32.to_le_bytes(),
            "first local header sig"
        );
        assert!(
            bytes.windows(4).any(|w| w == 0x0605_4b50u32.to_le_bytes()),
            "EOCD signature present"
        );
        assert!(
            bytes.windows(4).any(|w| w == 0x0201_4b50u32.to_le_bytes()),
            "central directory signature present"
        );
        // The EOCD entry-count (last fields) reflects two entries. The EOCD is
        // the final 22 bytes (no archive comment); entries-total is at offset +10.
        let eocd = &bytes[bytes.len() - 22..];
        let entries_total = u16::from_le_bytes([eocd[10], eocd[11]]);
        assert_eq!(entries_total, 2, "two entries recorded in the EOCD");

        // The raw stored data is present uncompressed.
        assert!(
            bytes.windows(5).any(|w| w == b"hello"),
            "stored data must be present verbatim (no compression)"
        );
    }
}
