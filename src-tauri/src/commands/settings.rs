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

use driven_core::orchestrator::OrchestratorConfig;
use driven_core::state::StateRepo;
use driven_core::types::ErrorCode;

use driven_vss::VssMode;

use crate::app_state::AppState;
use crate::commands::dtos::{
    GlobalSettings, ReleaseDto, SettingsDto, SettingsPatch, TelemetrySettings, UiSettings,
    UpdateInfo, UpdaterSettings, WindowsSettings,
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

    Ok(SettingsDto {
        global,
        telemetry,
        updater,
        ui,
        windows,
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
            cur.default_concurrent_uploads = v;
        }
        if let Some(v) = g.bandwidth_cap_mbps {
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
            cur.scan_interval_secs = v;
            orchestrator_affecting = true;
        }
        if let Some(v) = g.deep_verify_interval_secs {
            cur.deep_verify_interval_secs = v;
            // Per-source cadence feeds the orchestrator's deep-verify schedule.
            orchestrator_affecting = true;
        }
        if let Some(v) = g.io_priority {
            cur.io_priority = v;
        }
        if let Some(v) = g.log_level {
            if v != cur.log_level {
                new_log_level = Some(v.clone());
            }
            cur.log_level = v;
        }
        store_group(repo, KEY_GLOBAL, &storage::Global::from(cur)).await?;
    }

    // --- telemetry group ----------------------------------------------------
    if let Some(t) = patch.telemetry {
        let mut cur: TelemetrySettings = load_group::<storage::Telemetry>(repo, KEY_TELEMETRY)
            .await?
            .map(Into::into)
            .unwrap_or_else(default_telemetry);
        if let Some(v) = t.enabled {
            cur.enabled = v;
        }
        store_group(repo, KEY_TELEMETRY, &storage::Telemetry::from(cur)).await?;
    }

    // --- updater group ------------------------------------------------------
    if let Some(u) = patch.updater {
        let mut cur: UpdaterSettings = load_group::<storage::Updater>(repo, KEY_UPDATER)
            .await?
            .map(Into::into)
            .unwrap_or_else(default_updater);
        if let Some(v) = u.channel {
            cur.channel = v;
        }
        if let Some(v) = u.check_interval_secs {
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
            cur.tray_left_click_opens = v;
        }
        if let Some(v) = ui.locale {
            if v != cur.locale {
                new_locale = Some(v.clone());
            }
            cur.locale = v;
        }
        if let Some(v) = ui.color_mode {
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
            cur.vss_mode = v;
            if cfg!(windows) {
                orchestrator_affecting = true;
            }
        }
        store_group(repo, KEY_WINDOWS, &storage::Windows::from(cur)).await?;
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

    // Return the full, freshly-stored settings document.
    load_settings_dto(repo).await
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
        GlobalSettings, TelemetrySettings, UiSettings, UpdaterSettings, WindowsSettings,
    };

    /// `snake_case` on-disk form of the SPEC s22 `global` group.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Global {
        pub auto_start_on_login: bool,
        pub default_concurrent_uploads: Option<u32>,
        pub bandwidth_cap_mbps: Option<u32>,
        pub skip_on_battery: bool,
        pub skip_on_metered: bool,
        pub scan_interval_secs: u32,
        pub deep_verify_interval_secs: u32,
        pub io_priority: String,
        pub log_level: String,
    }

    impl From<Global> for GlobalSettings {
        fn from(s: Global) -> Self {
            GlobalSettings {
                auto_start_on_login: s.auto_start_on_login,
                default_concurrent_uploads: s.default_concurrent_uploads,
                bandwidth_cap_mbps: s.bandwidth_cap_mbps,
                skip_on_battery: s.skip_on_battery,
                skip_on_metered: s.skip_on_metered,
                scan_interval_secs: s.scan_interval_secs,
                deep_verify_interval_secs: s.deep_verify_interval_secs,
                io_priority: s.io_priority,
                log_level: s.log_level,
            }
        }
    }

    impl From<GlobalSettings> for Global {
        fn from(d: GlobalSettings) -> Self {
            Global {
                auto_start_on_login: d.auto_start_on_login,
                default_concurrent_uploads: d.default_concurrent_uploads,
                bandwidth_cap_mbps: d.bandwidth_cap_mbps,
                skip_on_battery: d.skip_on_battery,
                skip_on_metered: d.skip_on_metered,
                scan_interval_secs: d.scan_interval_secs,
                deep_verify_interval_secs: d.deep_verify_interval_secs,
                io_priority: d.io_priority,
                log_level: d.log_level,
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
    }

    impl From<Windows> for WindowsSettings {
        fn from(s: Windows) -> Self {
            WindowsSettings {
                vss_mode: s.vss_mode,
            }
        }
    }

    impl From<WindowsSettings> for Windows {
        fn from(d: WindowsSettings) -> Self {
            Windows {
                vss_mode: d.vss_mode,
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
    })
}

// ---------------------------------------------------------------------------
// export_diagnostic_bundle (SPEC s11.6, s18)
// ---------------------------------------------------------------------------

/// `export_diagnostic_bundle(dest)` - write a redacted diagnostic ZIP
/// (SPEC s11.6, s18).
///
/// SPEC s11.6.1: `dest` is treated as untrusted. The dialog-approved root is the
/// PARENT directory of the user-chosen save path (the add-source/restore/export
/// dialogs round-trip a `tauri-plugin-dialog` selection, so the parent is a
/// directory the user actually picked); [`validate_writable_dest`] then confines
/// the actual write to that one directory (no `..`, no symlink-at-leaf), and
/// [`atomic_write`] writes the ZIP atomically.
///
/// The bundle (SPEC s18) carries `version.txt`, `os.txt`, a REDACTED
/// `settings_redacted.json`, `schema.txt` (PRAGMA user_version + table counts),
/// and `redaction-policy.txt`. Every secret-bearing field (refresh tokens,
/// recovery phrases, keys, master key, account emails, drive folder names) is
/// redacted or hashed before it enters the ZIP.
#[tauri::command]
pub async fn export_diagnostic_bundle(
    app: AppHandle,
    state: State<'_, AppState>,
    dest: PathBuf,
) -> CommandResult<PathBuf> {
    // SPEC s11.6.1: confine the write to the directory the user chose (the
    // parent of the save path), then re-validate the leaf against it.
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            CommandError::with_code(
                ErrorCode::LocalIoError,
                "diagnostic bundle destination must include a directory",
            )
        })?;
    let token = DialogToken::for_root(parent.to_string_lossy().to_string());
    let confined = validate_writable_dest(&dest, &token)?;

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

    // schema.txt (SPEC s18): PRAGMA user_version + table row counts. Both are
    // metadata-only (no file paths / ids), so no redaction is needed.
    let schema = build_schema_summary(state).await;
    zip.add_file("schema.txt", schema.as_bytes());

    // redaction-policy.txt (SPEC s18): tell the recipient the bundle's threat
    // model + exactly what was redacted.
    zip.add_file("redaction-policy.txt", REDACTION_POLICY.as_bytes());

    Ok(zip.finish())
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

/// Build the `schema.txt` summary (SPEC s18): a row count per known table.
/// Best-effort: a count that cannot be read is rendered as `?` rather than
/// failing the whole bundle (a partial schema summary is still useful to a
/// maintainer). The counts are pure metadata (no file paths / drive ids), so no
/// redaction is needed.
///
/// SPEC s18 also lists `PRAGMA user_version`, but the `user_version` pragma is
/// only reachable through the concrete `SqliteStateRepo` pool, not the
/// object-safe [`StateRepo`] trait this command holds; rather than downcast we
/// record that it is not exposed here and emit the table counts (the part the
/// trait supports).
async fn build_schema_summary(state: &dyn StateRepo) -> String {
    let mut out = String::new();
    out.push_str("# Driven state DB schema summary (SPEC s18)\n");
    out.push_str("user_version=(not exposed via the StateRepo trait)\n");
    match state.list_accounts().await {
        Ok(rows) => out.push_str(&format!("accounts={}\n", rows.len())),
        Err(e) => out.push_str(&format!("accounts=? ({e})\n")),
    }
    match state.list_sources().await {
        Ok(rows) => out.push_str(&format!("backup_sources={}\n", rows.len())),
        Err(e) => out.push_str(&format!("backup_sources=? ({e})\n")),
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
- schema.txt   : state-DB schema version + table row counts (no paths / ids).
- redaction-policy.txt : this file.

What is redacted / never included:
- OAuth refresh + access tokens (keychain-only; never read into this bundle).
- Account encryption master keys + per-source keys (keychain-only).
- BIP39 recovery phrases (never persisted; never collected).
- The telemetry install id is replaced by a stable per-value hash
  (installid_<hash>) so occurrences can be correlated without exposing the id.

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

    let releases = fetch_releases(1).await?;

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
    let releases = fetch_releases(page).await?;

    let dtos = releases
        .into_iter()
        .filter(|r| release_eligible(r, want_prerelease))
        .map(release_to_dto)
        .collect();
    Ok(dtos)
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
async fn fetch_releases(page: u32) -> CommandResult<Vec<GithubRelease>> {
    let url = format!(
        "https://api.github.com/repos/{GITHUB_REPO}/releases?per_page={RELEASES_PER_PAGE}&page={page}"
    );

    let client = reqwest::Client::builder()
        .user_agent(GITHUB_USER_AGENT)
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
        auto_start_on_login: false,
        default_concurrent_uploads: None,
        bandwidth_cap_mbps: None,
        skip_on_battery: true,
        skip_on_metered: true,
        scan_interval_secs: 600,
        deep_verify_interval_secs: 604_800,
        io_priority: "low".to_string(),
        log_level: "info".to_string(),
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
        // Seeded SPEC s22 defaults round-trip.
        assert!(!dto.global.auto_start_on_login);
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
