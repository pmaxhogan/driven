//! IPC DTOs (SPEC s11.1 / s11.2 / s11.6).
//!
//! Hand-written serde structs that mirror the TypeScript interfaces in
//! `ui/src/ipc/types.ts`. Per the M6 decision (see `design/CODEX_NOTES.md` M6
//! section) Driven hand-writes the typed IPC surface rather than generating it
//! with tauri-specta: each DTO derives `Serialize`/`Deserialize` and renders
//! `camelCase` over the wire so the Rust and TS shapes line up by convention,
//! with no specta annotations, no `xtask`, and no CI codegen step.
//!
//! These DTOs are the contract the M6 accounts / sources / settings command
//! bodies construct; every command has a real, fully-wired body (no scaffold
//! stubs remain).

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};

use driven_core::types::AccountId;

/// serde helper for an `Option<Option<T>>` "double option" patch field, which
/// must distinguish three inbound JSON states that plain serde cannot. An ABSENT
/// key stays `None` (leave unchanged); a present `null` becomes `Some(None)`
/// (reset to the default - auto / unlimited / cleared); a present value becomes
/// `Some(Some(v))` (set). Without this, serde collapses `null` to the OUTER
/// `None`, so "reset" is indistinguishable from "no change" and the UI can never
/// clear the field back to its special value (the bug this fixes). Pair with
/// `#[serde(default, deserialize_with = "double_option")]` on the field.
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

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

/// The result of `reauth_account` (A3): the consent URL to open PLUS the
/// server-side session id the UI threads back through `poll_oauth_status` /
/// `oauth:complete` + `finish_add_account` to COMPLETE the re-consent onto the
/// EXISTING account (no duplicate account is created). Mirrors
/// `begin_add_account_wizard` + `start_oauth_signin` combined, since reauth runs
/// the whole flow in one backend call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReauthSession {
    /// The server-side wizard session id (scoped to the existing account).
    pub session_id: String,
    /// The Google consent URL the UI opens in the system browser.
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
    /// R4-P1-2 (DATA-SAFETY): `true` when this is the FIRST encrypted source that
    /// is still awaiting a recovery-phrase ack (it is persisted DISABLED and has a
    /// durable `recovery_phrase_acks` record). The UI uses this to DISABLE the
    /// enable toggle (with an explanatory tooltip) - the user must finish the
    /// reveal+ack step before the source can be enabled. The BACKEND
    /// (`update_source`) is the real guard; this only drives the affordance.
    /// Defaults to `false` for every non-pending source.
    #[serde(default)]
    pub pending_recovery_ack: bool,
}

/// Request body for `add_source` (SPEC s11.2 `AddSourceRequest`).
///
/// C1 (SPEC s11.6.1): the local path is NOT trusted from the webview - it is the
/// folder bound to `local_path_token` (a one-shot token from the backend's
/// `pick_folder_dialog`). The backend resolves the token to the path the USER
/// chose; a request without a matching token is REJECTED. `local_path` is kept
/// only as a display echo and is NOT used as the authoritative path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddSourceRequest {
    /// Owning account id (UUID string).
    pub account_id: AccountId,
    /// User-chosen display name.
    pub display_name: String,
    /// The one-shot dialog token (from `pick_folder_dialog`) proving the local
    /// path was chosen via a backend-owned native dialog (C1, SPEC s11.6.1).
    pub local_path_token: String,
    /// Display echo of the dialog-chosen local path (NOT authoritative - the
    /// backend uses the path bound to `local_path_token`).
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

/// The result of `add_source` (SPEC s11.2; B3 recovery-phrase reveal).
///
/// Carries the created [`SourceDto`] AND the one-time BIP39 recovery phrase
/// (`recovery_phrase`) - present ONLY when this source's encryption opt-in
/// GENERATED the account master key (the first encrypted source for the
/// account). The phrase is returned as a ONE-TIME VALUE (NOT a fire-and-forget
/// event the UI might miss), so the wizard can display it via
/// RecoveryPhraseReveal AFTER the source/key exists and gate Finish on the user
/// acknowledging they saved it. `None` for an unencrypted source or a subsequent
/// encrypted source (the account already has a phrase). The 24 words are never
/// persisted; the UI shows them once then drops them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddSourceResult {
    /// The newly created source.
    pub source: SourceDto,
    /// The one-time 24-word BIP39 recovery phrase, present only when this opt-in
    /// generated the account master key. `None` otherwise.
    pub recovery_phrase: Option<Vec<String>>,
    /// M9c D4 (M6 R4-P1-1, DATA-SAFETY): `true` when this source was persisted
    /// DISABLED and is awaiting a recovery-phrase ack - i.e. it is the first
    /// encrypted source for the account (it generated the master key). The source
    /// will NOT be backed up (it is excluded from the scheduler + manual sync)
    /// until `reveal_recovery_phrase` + `ack_recovery_phrase_saved` enable it, so
    /// no encrypted data is created before the recovery phrase is durably saveable.
    /// `false` for an unencrypted source or a subsequent encrypted source.
    pub pending_recovery_ack: bool,
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
/// R1-P1-2 (SPEC s11.6.1): the preview WALKS the local tree, so its root must
/// never be a raw webview-supplied path (a compromised renderer could enumerate
/// arbitrary readable directories). The root is resolved one of two safe ways:
/// - a NEW candidate source: `local_path_token` is the one-shot dialog token
///   `pick_folder_dialog` minted; the backend PEEKS (non-consuming, so the later
///   `add_source` keeps its single use) the path bound to it;
/// - an EXISTING source: `source_id` is the source's id; the backend resolves
///   `backup_sources.local_path` from SQLite.
///
/// Exactly one of the two must be present; a request with neither (or a token
/// that does not map to a backend dialog) is REJECTED.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExclusionPreviewRequest {
    /// One-shot dialog token for a NEW candidate source's folder (from
    /// `pick_folder_dialog`); resolved via a non-consuming peek (R1-P1-2).
    pub local_path_token: Option<String>,
    /// An EXISTING source's id; its `local_path` is resolved from SQLite
    /// (R1-P1-2). Mutually exclusive with `local_path_token`.
    pub source_id: Option<String>,
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
// Dialog tokens (SPEC s11.6.1 - C1)
// -----------------------------------------------------------------------------

/// The result of a backend-owned native dialog (C1, SPEC s11.6.1): the path the
/// USER chose plus the one-shot `token` the backend minted for it. The webview
/// passes the token (and path) to the matching write command
/// (`add_source` / `export_diagnostic_bundle`), which validates the token maps
/// to exactly that path - so the webview can never inject an arbitrary path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PickedPath {
    /// The path the native dialog returned (a folder, or a save-file path).
    pub path: String,
    /// The opaque one-shot dialog token bound to `path`.
    pub token: String,
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
    /// V2 small-file bundling on/off (issue #35 item d). A standalone advanced
    /// toggle, NOT part of any SPEC s22 group blob: it is backed by the
    /// `bundle_small_files` settings KV key the core planner reads directly, so
    /// it never rides a group round-trip. `false` (the frozen v1.0.0 behaviour)
    /// unless the user turns it on. The bundling thresholds stay backend-only KV.
    pub bundle_small_files: bool,
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
    /// V2 schedule window (DESIGN s17): when enabled, sync is gated to the
    /// configured local-time window.
    pub schedule: ScheduleSettings,
    /// V2 pre/post backup shell hooks (DESIGN s17). `null` = no hook.
    pub pre_backup_hook: Option<String>,
    /// See [`Self::pre_backup_hook`]; runs after a backup cycle.
    pub post_backup_hook: Option<String>,
    /// How long a hook command may run before it is killed, in seconds.
    pub hook_timeout_secs: u32,
    /// V2 metered pause-or-throttle: `pause` | `throttle` (DESIGN s17).
    pub metered_mode: String,
    /// Bandwidth cap (Mbps) used while metered in `throttle` mode; `null`
    /// falls back to `bandwidthCapMbps`.
    pub metered_bandwidth_cap_mbps: Option<u32>,
}

/// V2 schedule-window settings (DESIGN s17). Mirrors
/// [`driven_core::types::ScheduleConfig`]; the times are local wall-clock
/// minutes and `utc_offset_minutes` is `-new Date().getTimezoneOffset()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleSettings {
    /// When `false`, sync runs at any time (the default / V1 behaviour).
    pub enabled: bool,
    /// Minutes after local midnight the allowed window opens, `0..=1439`.
    pub start_minute: u32,
    /// Minutes after local midnight the allowed window closes, `0..=1439`.
    /// `end < start` wraps past midnight; `end == start` allows the whole day.
    pub end_minute: u32,
    /// Seven booleans, `0 = Sunday ..= 6 = Saturday`, marking the local days
    /// the window is active on.
    pub days: Vec<bool>,
    /// Minutes to add to UTC to reach local time (e.g. `-480` for PST).
    pub utc_offset_minutes: i32,
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
    /// Route VSS snapshots through the least-privilege elevated helper (DESIGN
    /// s5.3.1) so the main app stays un-elevated. `false` (default) keeps the
    /// historical behaviour (VSS needs the whole app launched as Administrator).
    #[serde(default)]
    pub vss_helper: bool,
}

/// Status of least-privilege locked-file backup (DESIGN s5.3.1), surfaced to the
/// Settings banner. All fields are `false` off Windows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VssHelperStatus {
    /// VSS (and thus locked-file backup) is a Windows-only capability.
    pub supported: bool,
    /// The main app is currently running elevated (Administrator) - the
    /// historical way to get VSS without the helper.
    pub elevated: bool,
    /// The user has opted into the least-privilege helper (`windows.vss_helper`).
    pub helper_enabled: bool,
    /// Locked-file backup is currently DEGRADED: on Windows, exclusively-locked
    /// files (Outlook PSTs, live databases, VM disks) are being skipped because
    /// Volume Shadow Copy is unavailable (the app is not elevated and no
    /// least-privilege helper is active).
    pub locked_file_backup_degraded: bool,
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
    /// Toggle V2 small-file bundling (issue #35 item d). `None` = leave
    /// unchanged; `Some(v)` writes the `bundle_small_files` settings KV key the
    /// core reads. See [`SettingsDto::bundle_small_files`].
    pub bundle_small_files: Option<bool>,
}

/// Partial SPEC s22 `global` settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalSettingsPatch {
    /// See [`GlobalSettings::auto_start_on_login`].
    pub auto_start_on_login: Option<bool>,
    /// See [`GlobalSettings::default_concurrent_uploads`]. `double_option` so an
    /// inbound `null` is `Some(None)` ("reset to auto"), distinct from an absent
    /// key `None` ("leave unchanged").
    #[serde(default, deserialize_with = "double_option")]
    pub default_concurrent_uploads: Option<Option<u32>>,
    /// See [`GlobalSettings::bandwidth_cap_mbps`]. `double_option`: `null` =
    /// `Some(None)` ("reset to unlimited").
    #[serde(default, deserialize_with = "double_option")]
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
    /// See [`GlobalSettings::schedule`]. Present = replace the whole schedule.
    pub schedule: Option<ScheduleSettings>,
    /// See [`GlobalSettings::pre_backup_hook`]. `double_option`: `null` =
    /// `Some(None)` clears it.
    #[serde(default, deserialize_with = "double_option")]
    pub pre_backup_hook: Option<Option<String>>,
    /// See [`GlobalSettings::post_backup_hook`]. `double_option`: `null` =
    /// `Some(None)` clears it.
    #[serde(default, deserialize_with = "double_option")]
    pub post_backup_hook: Option<Option<String>>,
    /// See [`GlobalSettings::hook_timeout_secs`].
    pub hook_timeout_secs: Option<u32>,
    /// See [`GlobalSettings::metered_mode`].
    pub metered_mode: Option<String>,
    /// See [`GlobalSettings::metered_bandwidth_cap_mbps`]. `double_option`:
    /// `null` = `Some(None)` clears it.
    #[serde(default, deserialize_with = "double_option")]
    pub metered_bandwidth_cap_mbps: Option<Option<u32>>,
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
    /// See [`WindowsSettings::vss_helper`].
    pub vss_helper: Option<bool>,
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

// -----------------------------------------------------------------------------
// Activity (SPEC s11.4)
// -----------------------------------------------------------------------------

/// Filter body for `query_activity` (SPEC s11.4 `ActivityFilter`).
///
/// Every field is optional; an empty filter matches every row, and present
/// fields combine with logical AND. The values arrive from the (untrusted)
/// webview and are validated + bounded by the command body BEFORE they reach the
/// query (SPEC s11.6.1: scalar-only filters, no raw paths). Mirrors
/// [`driven_core::state::ActivityFilter`] over the camelCase wire.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityFilterDto {
    /// Limit to a single source (UUID string); a malformed id is rejected.
    pub source_id: Option<String>,
    /// Lower-bound timestamp (Unix ms), inclusive.
    pub since_ms: Option<i64>,
    /// Upper-bound timestamp (Unix ms), exclusive.
    pub before_ms: Option<i64>,
    /// Minimum severity: `info` | `warn` | `error`; an unknown value is
    /// rejected (SPEC s11.6.1: validate enum filters).
    pub min_level: Option<String>,
    /// Event-type discriminants to include; empty = all. Each is bounded in
    /// length and count so a hostile renderer cannot build a giant IN-list.
    #[serde(default)]
    pub event_types: Vec<String>,
}

/// KEYSET page selector for `query_activity` (SPEC s11.4 `PageRequest`,
/// R2-P1-2), mirroring [`driven_core::state::PageRequest`]. The activity_log is
/// actively prepended to, so the webview pages by CURSOR (the oldest loaded
/// `(ts, id)`) instead of a shifting OFFSET. `beforeTs` / `beforeId` are both
/// present (a continuation page) or both absent (the first, newest page). The
/// command bounds `limit` to `1..=MAX_ACTIVITY_PAGE_LIMIT` before the query.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageRequestDto {
    /// Exclusive upper-bound timestamp (the oldest loaded row's `ts`), or
    /// `None` for the first page. Must be set together with `beforeId`.
    pub before_ts: Option<i64>,
    /// Exclusive upper-bound id (the oldest loaded row's `id`), breaking `ts`
    /// ties. Must be set together with `beforeTs`.
    pub before_id: Option<i64>,
    /// Max rows per page (bounded by the command, SPEC s11.6.1).
    pub limit: u32,
}

/// One KEYSET page of activity returned by `query_activity` (SPEC s11.4
/// `ActivityPage`, R2-P1-2): newest-first rows plus the cursor metadata so the
/// webview can accumulate pages client-side and know whether more remain.
///
/// `has_more` is `true` when rows older than this page's last row still match;
/// `next_before_ts` / `next_before_id` carry the `(ts, id)` of the LAST (oldest)
/// row in this page, which the webview passes as the cursor for the next page.
/// Both are `None` when the page is empty (no cursor to advance). The live tail
/// dedups prepended `activity:new` entries against the accumulated pages by id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityPageDto {
    /// The matching entries for this page, newest-first.
    pub entries: Vec<driven_core::types::ActivityEntry>,
    /// Total matching rows across the whole filter (for the "shown of total"
    /// count); independent of the cursor.
    pub total: u64,
    /// The page size used for this query (the bounded limit).
    pub limit: u32,
    /// `true` if at least one more (older) page exists after this one.
    pub has_more: bool,
    /// The `ts` of the LAST (oldest) row in this page - the cursor for the next
    /// page. `None` for an empty page.
    pub next_before_ts: Option<i64>,
    /// The `id` of the LAST (oldest) row in this page - the cursor for the next
    /// page. `None` for an empty page.
    pub next_before_id: Option<i64>,
}

/// One per-status file count for the Activity header (M7-P2-5; DESIGN s8.3
/// "file count by status"). Mirrors [`driven_core::state::FileStatusCount`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStatusCountDto {
    /// The `file_state.status` discriminant (`synced` | `pending` | `corrupt`
    /// | `locked` | `error` | `excluded_orphan`).
    pub status: String,
    /// Number of `file_state` rows in this status across all sources.
    pub count: u64,
}

/// The Activity dashboard header aggregates returned by `activity_summary`
/// (M7-P2-5; DESIGN s8.3): bytes uploaded today / this week, file count by
/// status, and the current throughput window. Mirrors
/// [`driven_core::state::ActivitySummary`] over the camelCase wire; the UI
/// formats bytes / rate with `Intl.NumberFormat` per DESIGN s8.7.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivitySummaryDto {
    /// Bytes uploaded since the start of today (local midnight).
    pub bytes_today: u64,
    /// Bytes uploaded since the start of this week.
    pub bytes_week: u64,
    /// File count grouped by status; statuses with no rows are omitted.
    pub file_status_counts: Vec<FileStatusCountDto>,
    /// Bytes observed in the recent throughput window.
    pub throughput_window_bytes: u64,
    /// Length of the throughput window in milliseconds (so the UI derives a
    /// bytes/sec rate).
    pub throughput_window_ms: u64,
}

// -----------------------------------------------------------------------------
// Restore (SPEC s11.5; DESIGN s8.4)
// -----------------------------------------------------------------------------

/// One node in the Restore browser tree (SPEC s11.5 `RemoteEntryDto`; DESIGN
/// s8.4). Either a FOLDER the user can descend into or a FILE they can restore.
/// Derived from `file_state` (the LOCAL authoritative metadata) under a plaintext
/// prefix - never a Drive call (ROADMAP M8: avoid hitting Drive for navigation).
/// For an encrypted source the name shown here is already the DECRYPTED plaintext
/// component, because `file_state.relative_path` stores the plaintext path (SPEC
/// s2); the ciphertext path never reaches the webview.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEntryDto {
    /// The plaintext relative path of this node under the source root (the stable
    /// id the webview passes back as the next prefix / restore selection).
    pub relative_path: String,
    /// The display name (the last path component).
    pub name: String,
    /// `true` for a folder (descendable), `false` for a restorable file.
    pub is_dir: bool,
    /// File size in bytes; `0` for a folder.
    pub size: u64,
    /// `file_state.status` discriminant for a file (`synced` | `pending` | ...);
    /// `None` for a folder.
    pub status: Option<String>,
    /// `true` if this file has been uploaded (has a `drive_file_id`) and is
    /// therefore restorable; always `false` for a folder.
    pub restorable: bool,
}

/// The result of `list_remote_tree` (SPEC s11.5; DESIGN s8.4): the immediate
/// children of a folder plus a `truncated` flag so the webview can tell the user
/// the listing was capped (M8-P2-1). Before this the command returned a bare
/// `Vec<RemoteEntryDto>` and silently dropped children past [`MAX_TREE_NODES`],
/// so a user could believe a large folder was fully shown. The cap itself is
/// unchanged (the range scan stays bounded); the flag SURFACES it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTreeDto {
    /// The immediate children (folders first, then files), capped at
    /// [`MAX_TREE_NODES`](crate::commands::restore::MAX_TREE_NODES).
    pub entries: Vec<RemoteEntryDto>,
    /// `true` when the folder has MORE immediate children than the cap, so the
    /// UI shows a "showing first N; refine your search" notice.
    pub truncated: bool,
}

/// One Restore search hit (SPEC s11.5 `FileSearchHit`; DESIGN s8.4). Mirrors
/// [`driven_core::state::FileSearchHit`] over the camelCase wire. The path is the
/// plaintext relative path (decrypted display for encrypted sources, per SPEC s2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchHitDto {
    /// The source the match belongs to (UUID string).
    pub source_id: String,
    /// The matched plaintext relative path.
    pub relative_path: String,
    /// The file's `file_state.status` discriminant.
    pub status: String,
    /// `true` if the file is uploaded (restorable).
    pub restorable: bool,
}

/// One file the webview selected to restore (SPEC s11.5 `RestoreItem`). The
/// `(source_id, relative_path)` pair is the `file_state` primary key; the backend
/// re-reads the authoritative row (drive id, size, encryption) from SQLite - the
/// webview never supplies the Drive id or a local path (SPEC s11.6.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestoreItem {
    /// The source the file belongs to (UUID string).
    pub source_id: String,
    /// The plaintext relative path under the source root (the file_state key).
    pub relative_path: String,
}

/// Issue #36: one retained point-in-time version of a file, for the Restore
/// version-history view. Mirrors [`driven_core::state::FileVersionRow`] over the
/// camelCase wire, minus the internal Drive id. The version was the file's
/// current content during `[created_at, superseded_at)` (Unix ms).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileVersionDto {
    /// Plaintext size in bytes of this version.
    pub size: u64,
    /// Wall-time (Unix ms) this version first became the current backup.
    pub created_at: i64,
    /// Wall-time (Unix ms) it was superseded by the next version.
    pub superseded_at: i64,
    /// `true` once the old Drive object has been moved to trash (it remains
    /// restorable by date until Drive purges its trash).
    pub trashed: bool,
}

/// The opaque id of a spawned restore job (SPEC s11.5 `RestoreJobId`). A
/// transparent UUID-string newtype so a stale / forged id surfaces as a command
/// error rather than resolving the wrong job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct RestoreJobId(pub String);

/// Terminal / in-progress state of one file within a restore job (SPEC s11.7).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestoreFileState {
    /// Queued, not started.
    Pending,
    /// Downloading + (for encrypted sources) stream-decrypting to disk.
    Restoring,
    /// Written to disk + verified (blake3 matched the stored plaintext hash).
    Done,
    /// Failed; `error_code` on the file entry carries the SPEC s24 code.
    Failed,
    /// Cancelled before this file finished (M8-P1-1): the user cancelled the job
    /// while this file was pending / mid-stream. Any partial temp was deleted, so
    /// no half-written file is left on disk.
    Cancelled,
}

/// Per-file progress within a restore job (SPEC s11.7 `RestoreJobStatus`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestoreFileProgress {
    /// The plaintext relative path being restored (the display + identity).
    pub relative_path: String,
    /// This file's lifecycle state.
    pub state: RestoreFileState,
    /// Plaintext bytes written to disk so far.
    pub bytes_done: u64,
    /// Total plaintext bytes expected (the file_state size).
    pub bytes_total: u64,
    /// The stable SPEC s24 error code (i18n key) when `state == Failed`; `None`
    /// otherwise.
    pub error_code: Option<String>,
}

/// The full status of a restore job, streamed on `restore:progress` and returned
/// by `get_restore_job` (SPEC s11.5 / s11.7 `RestoreJobStatus`). Carries overall
/// progress (bytes + file counts), the current file, a per-file breakdown, and a
/// terminal `done` flag so the webview can render live progress + a done state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestoreJobStatus {
    /// The job id this status belongs to.
    pub job_id: String,
    /// Total files in the job.
    pub total_files: u32,
    /// Files that have finished successfully.
    pub completed_files: u32,
    /// Files that have failed.
    pub failed_files: u32,
    /// Total plaintext bytes expected across every file.
    pub total_bytes: u64,
    /// Total plaintext bytes written to disk so far.
    pub bytes_done: u64,
    /// The relative path of the file currently being restored, if any.
    pub current_file: Option<String>,
    /// `true` once the job has reached a terminal state - every file is done /
    /// failed / cancelled. Set for a normal completion AND a cancellation, so the
    /// webview re-enables its controls on either.
    pub done: bool,
    /// `true` when the job reached its terminal state because the user CANCELLED
    /// it (M8-P1-1; SPEC s11.7 CANCELLED terminal state). Distinct from `done`:
    /// `done && !cancelled` is a normal finish; `done && cancelled` means the job
    /// was stopped early and any in-flight temp file was deleted (no partial).
    #[serde(default)]
    pub cancelled: bool,
    /// Per-file progress entries.
    pub files: Vec<RestoreFileProgress>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // #7: the `double_option` patch fields must distinguish ABSENT / null / value
    // so the UI can reset a nullable setting (concurrent uploads, bandwidth cap,
    // hooks, metered cap) back to its default. Plain serde collapsed an inbound
    // `null` to the outer `None`, making "reset to auto/unlimited/cleared"
    // indistinguishable from "leave unchanged" - so the field could never be
    // cleared from the webview.
    #[test]
    fn global_patch_double_option_distinguishes_absent_null_and_value() {
        // ABSENT key -> None ("leave unchanged").
        let absent: GlobalSettingsPatch = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.default_concurrent_uploads, None);
        assert_eq!(absent.bandwidth_cap_mbps, None);
        assert_eq!(absent.pre_backup_hook, None);
        assert_eq!(absent.metered_bandwidth_cap_mbps, None);

        // Present `null` -> Some(None) ("reset to auto / unlimited / cleared").
        let cleared: GlobalSettingsPatch = serde_json::from_str(
            r#"{
                "defaultConcurrentUploads": null,
                "bandwidthCapMbps": null,
                "preBackupHook": null,
                "postBackupHook": null,
                "meteredBandwidthCapMbps": null
            }"#,
        )
        .unwrap();
        assert_eq!(cleared.default_concurrent_uploads, Some(None));
        assert_eq!(cleared.bandwidth_cap_mbps, Some(None));
        assert_eq!(cleared.pre_backup_hook, Some(None));
        assert_eq!(cleared.post_backup_hook, Some(None));
        assert_eq!(cleared.metered_bandwidth_cap_mbps, Some(None));

        // Present value -> Some(Some(v)) ("set to v").
        let set: GlobalSettingsPatch = serde_json::from_str(
            r#"{"defaultConcurrentUploads": 8, "preBackupHook": "./hook.sh"}"#,
        )
        .unwrap();
        assert_eq!(set.default_concurrent_uploads, Some(Some(8)));
        assert_eq!(set.pre_backup_hook, Some(Some("./hook.sh".to_string())));
        // Untouched double-option keys stay None ("leave unchanged").
        assert_eq!(set.bandwidth_cap_mbps, None);
    }
}
