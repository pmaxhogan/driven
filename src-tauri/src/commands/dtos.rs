//! IPC DTOs (SPEC s11.1 / s11.2 / s11.6).
//!
//! Hand-written serde structs that mirror the TypeScript interfaces in
//! `ui/src/ipc/types.ts`. Per the M6 decision (see `design/CODEX_NOTES.md` M6
//! section) Driven hand-writes the typed IPC surface rather than generating it
//! with tauri-specta: each DTO derives `Serialize`/`Deserialize` and renders
//! `camelCase` over the wire so the Rust and TS shapes line up by convention,
//! with no specta annotations, no `xtask`, and no CI codegen step.
//!
//! These DTOs are the contract three parallel M6 implementers code against;
//! the command bodies that construct them are `todo!()` in M6 scaffold and are
//! filled in by the accounts / sources / settings implementers respectively.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use driven_core::types::AccountId;

// -----------------------------------------------------------------------------
// Accounts (SPEC s11.1)
// -----------------------------------------------------------------------------

/// A connected Google account as surfaced to the Accounts settings tab + the
/// wizard (SPEC s11.1 `AccountDto`). Mirrors the user-facing subset of
/// [`driven_core::state::AccountRow`]; the keychain master-key handle is NOT
/// exposed to the webview.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDto {
    /// `accounts.id` (UUID string over the wire).
    pub id: String,
    /// `accounts.email`.
    pub email: String,
    /// `accounts.display_name`.
    pub display_name: Option<String>,
    /// `accounts.state`: one of `ok` | `needs_reauth` | `disabled` (the
    /// serialized form of [`driven_core::types::AccountState`]).
    pub state: String,
    /// Whether this account has an encryption master key configured.
    pub encryption_enabled: bool,
    /// `accounts.created_at` (unix ms).
    pub created_at: i64,
    /// `accounts.last_synced_at` (unix ms), `None` until the first sync.
    pub last_synced_at: Option<i64>,
}

/// The opaque session id returned by `begin_add_account_wizard` and threaded
/// through the OAuth steps (SPEC s11.1 `AddAccountWizardSessionId`).
///
/// A newtype over a UUID string so a stale or forged session id surfaces as a
/// command error rather than silently resolving the wrong in-flight flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AddAccountWizardSessionId(pub String);

/// Alias matching the SPEC s11.1 `SessionId` argument name on the OAuth-step
/// commands (`submit_oauth_credentials`, `start_oauth_signin`, ...).
pub type SessionId = AddAccountWizardSessionId;

/// The Google consent URL the wizard opens in the system browser
/// (SPEC s11.1 `OAuthAuthUrl`). Returned by `start_oauth_signin` /
/// `reauth_account`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthAuthUrl {
    /// The fully-formed authorization URL (with PKCE challenge + state).
    pub auth_url: String,
}

/// The polled state of an in-flight OAuth sign-in (SPEC s11.1 `OAuthStatus`),
/// mirroring [`driven_drive::google::oauth::OAuthProgress`] plus terminal
/// success / failure for the webview poll loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum OAuthStatus {
    /// The consent browser tab is being opened.
    OpeningBrowser,
    /// Waiting on the loopback callback (user is consenting).
    AwaitingCallback,
    /// The auth code is being exchanged for tokens.
    ExchangingCode,
    /// Tokens were obtained; the wizard may proceed to `finish_add_account`.
    Complete,
    /// The flow failed with a stable SPEC s24 error code (i18n key).
    Failed {
        /// The dotted SPEC s24 error code (e.g. `auth.invalid_grant`).
        code: String,
    },
}

// -----------------------------------------------------------------------------
// Sources (SPEC s11.2)
// -----------------------------------------------------------------------------

/// A backup source as surfaced to the Sources settings tab (SPEC s11.2
/// `SourceDto`). Mirrors the user-facing subset of
/// [`driven_core::state::SourceRow`]; the wrapped per-source key is NOT
/// exposed to the webview.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceDto {
    /// `backup_sources.id`.
    pub id: String,
    /// Owning `accounts.id`.
    pub account_id: String,
    /// `backup_sources.display_name`.
    pub display_name: String,
    /// `backup_sources.enabled`.
    pub enabled: bool,
    /// Absolute local path of the source root.
    pub local_path: String,
    /// Drive destination folder id.
    pub drive_folder_id: String,
    /// Cached Drive destination display path.
    pub drive_folder_path: String,
    /// Whether per-source encryption is on.
    pub encryption_enabled: bool,
    /// Whether `.gitignore` rules are honoured during scan.
    pub respect_gitignore: bool,
    /// User include globs.
    pub include_patterns: Vec<String>,
    /// User exclude globs.
    pub exclude_patterns: Vec<String>,
    /// Deep-verify cadence in seconds.
    pub deep_verify_interval_secs: u32,
    /// Wall-time of last completed full scan; `None` until the first scan.
    pub last_full_scan_at: Option<i64>,
    /// `backup_sources.created_at`.
    pub created_at: i64,
}

/// Request body for `add_source` (SPEC s11.2 `AddSourceRequest`).
///
/// `local_path` MUST be a dialog-derived path (SPEC s11.6.1): the webview
/// cannot inject an arbitrary local path; the add-source wizard rounds-trips a
/// `tauri-plugin-dialog` selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddSourceRequest {
    /// Owning account id (UUID string).
    pub account_id: AccountId,
    /// User-chosen display name.
    pub display_name: String,
    /// Dialog-derived absolute local path (SPEC s11.6.1).
    pub local_path: PathBuf,
    /// Drive destination folder id (from `pick_drive_folder`).
    pub drive_folder_id: String,
    /// Cached Drive destination display path.
    pub drive_folder_path: String,
    /// Whether to enable per-source encryption.
    pub encryption_enabled: bool,
    /// Whether to honour `.gitignore`.
    pub respect_gitignore: bool,
    /// Include globs.
    pub include_patterns: Vec<String>,
    /// Exclude globs.
    pub exclude_patterns: Vec<String>,
}

/// Patch body for `update_source` (SPEC s11.2 `SourcePatch`). Every field is
/// optional: `None` leaves the corresponding column unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcePatch {
    /// Rename the source.
    pub display_name: Option<String>,
    /// Enable / disable the source.
    pub enabled: Option<bool>,
    /// Toggle `.gitignore` handling.
    pub respect_gitignore: Option<bool>,
    /// Replace the include globs.
    pub include_patterns: Option<Vec<String>>,
    /// Replace the exclude globs.
    pub exclude_patterns: Option<Vec<String>>,
    /// Change the deep-verify cadence.
    pub deep_verify_interval_secs: Option<u32>,
}

/// One entry in a Drive folder listing returned by `pick_drive_folder`
/// (SPEC s11.2). A folder the user can descend into or select as a
/// destination.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveFolderEntry {
    /// Drive folder id.
    pub id: String,
    /// Folder display name.
    pub name: String,
}

/// The result of `pick_drive_folder` (SPEC s11.2 `DriveFolderListing`): the
/// children of the requested folder plus breadcrumb context for the picker UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveFolderListing {
    /// The folder whose children these are; `None` at the My Drive root.
    pub current_folder_id: Option<String>,
    /// Cached display path of the current folder (for breadcrumbs).
    pub current_folder_path: String,
    /// Child folders the user can descend into / select.
    pub folders: Vec<DriveFolderEntry>,
}

/// Request body for `preview_exclusions` (SPEC s11.2 `ExclusionPreviewRequest`).
///
/// `local_path` MUST be a dialog-derived path (SPEC s11.6.1) - the preview
/// walks the local tree, so the same untrusted-path rule applies as
/// `add_source`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExclusionPreviewRequest {
    /// Dialog-derived absolute local path to preview (SPEC s11.6.1).
    pub local_path: PathBuf,
    /// Whether `.gitignore` rules are honoured.
    pub respect_gitignore: bool,
    /// Candidate include globs.
    pub include_patterns: Vec<String>,
    /// Candidate exclude globs.
    pub exclude_patterns: Vec<String>,
}

/// The result of `preview_exclusions` (SPEC s11.2 `ExclusionPreview`): a
/// bounded sample of which files the current rules would include vs exclude,
/// so the wizard can show the user the effect before committing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExclusionPreview {
    /// Count of files that would be backed up under these rules.
    pub included_count: u64,
    /// Count of files that would be excluded under these rules.
    pub excluded_count: u64,
    /// Total bytes of the included files.
    pub included_bytes: u64,
    /// A bounded sample of included relative paths (for display).
    pub included_sample: Vec<String>,
    /// A bounded sample of excluded relative paths (for display).
    pub excluded_sample: Vec<String>,
    /// `true` if the sample was truncated (more files than the sample cap).
    pub truncated: bool,
}

// -----------------------------------------------------------------------------
// Settings & misc (SPEC s11.6, s22)
// -----------------------------------------------------------------------------

/// The full settings snapshot returned by `get_settings` (SPEC s11.6),
/// mirroring the SPEC s22 KV schema (the `global`, `telemetry`, `updater`,
/// `windows`, and `ui` keys) flattened into one DTO for the Rules / About
/// tabs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDto {
    /// SPEC s22 `global` settings group.
    pub global: GlobalSettings,
    /// SPEC s22 `telemetry` settings group.
    pub telemetry: TelemetrySettings,
    /// SPEC s22 `updater` settings group.
    pub updater: UpdaterSettings,
    /// SPEC s22 `ui` settings group.
    pub ui: UiSettings,
    /// SPEC s22 `windows` settings group; `None` on non-Windows hosts.
    pub windows: Option<WindowsSettings>,
}

/// SPEC s22 `global` settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalSettings {
    /// Launch Driven on login.
    pub auto_start_on_login: bool,
    /// `null` = auto-pick concurrency; else a user override `1..=32`.
    pub default_concurrent_uploads: Option<u32>,
    /// `null` = unlimited; else the cap in megabits/sec.
    pub bandwidth_cap_mbps: Option<u32>,
    /// Skip sync while on battery.
    pub skip_on_battery: bool,
    /// Skip sync while on a metered network.
    pub skip_on_metered: bool,
    /// Scan cadence in seconds.
    pub scan_interval_secs: u32,
    /// Deep-verify cadence in seconds.
    pub deep_verify_interval_secs: u32,
    /// `normal` | `low` | `idle`.
    pub io_priority: String,
    /// `tracing` log level.
    pub log_level: String,
}

/// SPEC s22 `telemetry` settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySettings {
    /// Whether anonymous usage stats are sent.
    pub enabled: bool,
    /// The stable install id.
    pub install_id: String,
    /// The ingest endpoint.
    pub endpoint: String,
}

/// SPEC s22 `updater` settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdaterSettings {
    /// `stable` | `dev`.
    pub channel: String,
    /// Update-check cadence in seconds.
    pub check_interval_secs: u32,
}

/// SPEC s22 `ui` settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSettings {
    /// What a left-click on the tray icon opens.
    pub tray_left_click_opens: String,
    /// The active locale (BCP-47 tag).
    pub locale: String,
    /// `system` | `light` | `dark`.
    pub color_mode: String,
}

/// SPEC s22 `windows` settings (Windows-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowsSettings {
    /// `auto` | `always` | `never` (VSS snapshot policy).
    pub vss_mode: String,
}

/// Patch body for `update_settings` (SPEC s11.6 `SettingsPatch`). Each group is
/// optional; within a present group every field is optional too, so the UI can
/// PATCH a single toggle without round-tripping the whole settings document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    /// Partial `global` group.
    pub global: Option<GlobalSettingsPatch>,
    /// Partial `telemetry` group.
    pub telemetry: Option<TelemetrySettingsPatch>,
    /// Partial `updater` group.
    pub updater: Option<UpdaterSettingsPatch>,
    /// Partial `ui` group.
    pub ui: Option<UiSettingsPatch>,
    /// Partial `windows` group (Windows-only).
    pub windows: Option<WindowsSettingsPatch>,
}

/// Partial SPEC s22 `global` settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalSettingsPatch {
    /// See [`GlobalSettings::auto_start_on_login`].
    pub auto_start_on_login: Option<bool>,
    /// See [`GlobalSettings::default_concurrent_uploads`]. Note: `Some(None)`
    /// vs `None` distinguishes "set to auto" from "leave unchanged".
    pub default_concurrent_uploads: Option<Option<u32>>,
    /// See [`GlobalSettings::bandwidth_cap_mbps`].
    pub bandwidth_cap_mbps: Option<Option<u32>>,
    /// See [`GlobalSettings::skip_on_battery`].
    pub skip_on_battery: Option<bool>,
    /// See [`GlobalSettings::skip_on_metered`].
    pub skip_on_metered: Option<bool>,
    /// See [`GlobalSettings::scan_interval_secs`].
    pub scan_interval_secs: Option<u32>,
    /// See [`GlobalSettings::deep_verify_interval_secs`].
    pub deep_verify_interval_secs: Option<u32>,
    /// See [`GlobalSettings::io_priority`].
    pub io_priority: Option<String>,
    /// See [`GlobalSettings::log_level`].
    pub log_level: Option<String>,
}

/// Partial SPEC s22 `telemetry` settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySettingsPatch {
    /// See [`TelemetrySettings::enabled`].
    pub enabled: Option<bool>,
}

/// Partial SPEC s22 `updater` settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdaterSettingsPatch {
    /// See [`UpdaterSettings::channel`].
    pub channel: Option<String>,
    /// See [`UpdaterSettings::check_interval_secs`].
    pub check_interval_secs: Option<u32>,
}

/// Partial SPEC s22 `ui` settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSettingsPatch {
    /// See [`UiSettings::tray_left_click_opens`].
    pub tray_left_click_opens: Option<String>,
    /// See [`UiSettings::locale`].
    pub locale: Option<String>,
    /// See [`UiSettings::color_mode`].
    pub color_mode: Option<String>,
}

/// Partial SPEC s22 `windows` settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowsSettingsPatch {
    /// See [`WindowsSettings::vss_mode`].
    pub vss_mode: Option<String>,
}

/// An available update surfaced by `check_for_updates` (SPEC s11.6
/// `UpdateInfo`), also the payload of the `updater:available` /
/// `updater:downloaded` events (SPEC s11.7).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    /// The available version (semver).
    pub version: String,
    /// The release notes / changelog body (markdown), if published.
    pub notes: Option<String>,
    /// The release publish date (RFC3339), if published.
    pub published_at: Option<String>,
    /// The channel the update is from (`stable` | `dev`).
    pub channel: String,
}

/// One published release listed by `list_releases` (SPEC s11.6 `ReleaseDto`),
/// for the About tab's release-notes viewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseDto {
    /// The release version (semver / tag name).
    pub version: String,
    /// The release name / title.
    pub name: String,
    /// The release notes body (markdown).
    pub notes: String,
    /// The release publish date (RFC3339).
    pub published_at: String,
    /// The release page URL.
    pub url: String,
}
