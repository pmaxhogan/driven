//! In-app updater (SPEC s15.2, ROADMAP M9).
//!
//! Two responsibilities:
//!
//! 1. RUNTIME CHANNEL SELECTION. The Tauri updater natively substitutes only
//!    `{{target}}`, `{{arch}}`, and `{{current_version}}` - **`{{channel}}` is
//!    NOT a valid placeholder** (SPEC s15.1). Driven's static-server layout uses
//!    `{{target}}` (OS) + `{{arch}}` and NO `{{current_version}}` segment (the
//!    manifest carries the latest version; the updater compares its running
//!    version to it). The channel (`stable` vs `dev`)
//!    therefore lives in the URL PATH and is chosen AT RUNTIME via
//!    `app.updater_builder().endpoints(vec![<channel URL>])`. [`build_updater`]
//!    is that wrapper. The active channel is read from / written to the SPEC s22
//!    `updater.channel` settings group (the same group the M6 Settings layer
//!    persists), so there is no ad-hoc state.
//!
//! 2. PERIODIC CHECK. On startup AND every [`CHECK_INTERVAL`] (6h) a background
//!    task ([`spawn_periodic_check`]) checks the active channel and, on an
//!    available update, surfaces it PER CHANNEL (DESIGN s9.4, R6-P2-1): the
//!    STABLE channel records the pending update + emits `updater:available` (the
//!    manual in-app banner asks the user to apply), while the DEV channel applies
//!    SILENTLY - it downloads + installs the staged update and raises a tray
//!    notification (the staged update applies on the next restart). The task is a
//!    tokio `interval` loop (NOT a busy /
//!    sleep-poll loop) that `select!`s on a shutdown watch, and its handle +
//!    shutdown sender are registered on [`AppState`] so the app-quit drain joins
//!    it with NO orphan (mirrors the M5 per-account no-orphan bookkeeping).
//!
//! IPC commands (SPEC s15.2, mirrored into `ui/src/ipc/*`):
//! - [`check_for_update`] - a manual check; returns the available update or
//!   `None`, and records the pending update + emits `updater:available`.
//! - [`install_update`] - stages + applies the pending update via
//!   `download_and_install` (emitting `updater:download_progress` +
//!   `updater:downloaded`), then relaunches via `tauri-plugin-process`
//!   (`app.restart()`).
//! - [`get_update_channel`] / [`set_update_channel`] - the channel toggle.
//!
//! TEST SEAM (no live endpoint in tests): the network-free decision logic
//! (channel parse, the per-channel URL, the `Update` -> `UpdateInfo` mapping,
//! and the available-update dispatch in [`dispatch_check_outcome`]) is split
//! into pure functions the unit tests exercise directly. `build_updater` /
//! `check_for_update` are the only parts that touch the real plugin, and the
//! tests never call them, so nothing here hits `driven.maxhogan.dev`.

use std::time::Duration;

use tauri::{AppHandle, Manager, State};
use tauri_plugin_updater::UpdaterExt;

use driven_core::state::StateRepo;
use driven_core::types::ErrorCode;

use crate::app_state::AppState;
use crate::commands::dtos::UpdateInfo;
use crate::commands::{CommandError, CommandResult};

/// Tracing target for the updater layer.
const TARGET: &str = "driven::app::updater";

/// The periodic update-check cadence (SPEC s15.2 / ROADMAP M9: "on startup AND
/// every 6h"). A `tokio::time::interval`, NOT a sleep/poll loop.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// The SPEC s22 settings KV key whose `channel` field holds the active updater
/// channel. Must match the key the M6 Settings layer uses (settings.rs
/// `KEY_UPDATER`).
const KEY_UPDATER: &str = "updater";

/// The stable-channel update-manifest URL (SPEC s15.1/s15.2). `{{target}}` (OS:
/// `windows`/`darwin`/`linux`) and `{{arch}}` (`x86_64`/`aarch64`/...) are
/// substituted by the Tauri updater; the channel (`stable`) is in the PATH,
/// never a placeholder. The path carries NO `{{current_version}}` segment (the
/// manifest itself carries the latest version; including the installed version
/// made a 0.1.0 app fetch /0.1.0/ while the 0.1.1 release wrote /0.1.1/, so
/// updates were never discovered - R1-P1-1). This MUST stay byte-identical to
/// `scripts/generate-update-json.mjs`'s output layout, tauri.conf.json, and the
/// release/dev-channel workflow deploy paths.
const STABLE_ENDPOINT: &str =
    "https://driven.maxhogan.dev/updates/stable/{{target}}/{{arch}}/update.json";
/// The dev-channel update-manifest URL (SPEC s15.2). Pre-release / opt-in. Same
/// layout as [`STABLE_ENDPOINT`], differing only in the channel path segment.
const DEV_ENDPOINT: &str =
    "https://driven.maxhogan.dev/updates/dev/{{target}}/{{arch}}/update.json";

// ---------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------

/// The update channel (SPEC s15.2). Persisted as the `updater.channel` settings
/// string; defaults to [`Channel::Stable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Stable releases only (skips pre-releases). The default.
    Stable,
    /// The opt-in developer channel (includes pre-releases).
    Dev,
}

impl Channel {
    /// The persisted `updater.channel` string form (`stable` | `dev`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Dev => "dev",
        }
    }

    /// Parse a persisted channel string, defaulting to [`Channel::Stable`] for an
    /// unknown / absent value (the safe default - a corrupt setting never silently
    /// opts the user into pre-releases).
    #[must_use]
    pub fn from_str_lenient(s: &str) -> Channel {
        match s {
            "dev" => Channel::Dev,
            _ => Channel::Stable,
        }
    }

    /// The channel's update-manifest endpoint URL (SPEC s15.2). The channel is in
    /// the PATH; `{{target}}` / `{{arch}}` are the Tauri placeholders (no
    /// `{{current_version}}` segment - R1-P1-1).
    #[must_use]
    pub fn endpoint_url(self) -> &'static str {
        match self {
            Channel::Stable => STABLE_ENDPOINT,
            Channel::Dev => DEV_ENDPOINT,
        }
    }
}

/// The minimal `updater` settings group shape this module reads/writes. Mirrors
/// the on-disk `snake_case` form the M6 settings layer persists (settings.rs
/// `storage::Updater`); kept local so the updater module does not depend on the
/// settings command internals. Extra fields (e.g. `check_interval_secs`) are
/// preserved on a round-trip because we read the raw JSON object and only mutate
/// the `channel` key.
async fn read_channel(state: &dyn StateRepo) -> CommandResult<Channel> {
    let value = state
        .get_setting(KEY_UPDATER)
        .await
        .map_err(CommandError::from)?;
    let channel = value
        .as_ref()
        .and_then(|v| v.get("channel"))
        .and_then(|c| c.as_str())
        .map(Channel::from_str_lenient)
        .unwrap_or(Channel::Stable);
    Ok(channel)
}

/// Persist the active channel into the `updater.channel` settings field,
/// PRESERVING any sibling fields (e.g. `check_interval_secs`) already stored.
async fn write_channel(state: &dyn StateRepo, channel: Channel) -> CommandResult<()> {
    // Start from the existing group (so we keep `check_interval_secs` etc.), or a
    // fresh object if none is stored yet, and set only `channel`.
    let mut group = match state
        .get_setting(KEY_UPDATER)
        .await
        .map_err(CommandError::from)?
    {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    group.insert(
        "channel".to_string(),
        serde_json::Value::String(channel.as_str().to_string()),
    );
    // Default `check_interval_secs` if it was never seeded, so the stored group
    // stays a complete, well-typed document.
    group
        .entry("check_interval_secs".to_string())
        .or_insert_with(|| serde_json::Value::from(CHECK_INTERVAL.as_secs()));
    state
        .set_setting(KEY_UPDATER, &serde_json::Value::Object(group))
        .await
        .map_err(CommandError::from)
}

// ---------------------------------------------------------------------------
// build_updater (SPEC s15.2)
// ---------------------------------------------------------------------------

/// Build a runtime updater pointed at `channel`'s endpoint (SPEC s15.2).
///
/// `app.updater_builder().endpoints(vec![<channel URL>])?.build()?`: the
/// per-invocation endpoints override the static `tauri.conf.json` default so the
/// channel chosen at runtime (from settings) takes effect. A malformed URL or a
/// builder failure maps to `update.endpoint_unreachable` (the updater could not
/// be constructed for that endpoint) rather than panicking.
fn build_updater(
    app: &AppHandle,
    channel: Channel,
    ca_certs: Vec<reqwest_updater::Certificate>,
) -> CommandResult<tauri_plugin_updater::Updater> {
    let url = channel.endpoint_url().parse::<tauri::Url>().map_err(|e| {
        CommandError::with_code(
            ErrorCode::UpdateEndpointUnreachable,
            format!(
                "invalid update endpoint URL for channel {}: {e}",
                channel.as_str()
            ),
        )
    })?;
    let mut builder = app.updater_builder().endpoints(vec![url]).map_err(|e| {
        CommandError::with_code(
            ErrorCode::UpdateEndpointUnreachable,
            format!("could not set update endpoint: {e}"),
        )
    })?;
    // Issue #34: add the user's custom root CA to the plugin's OWN download
    // client (the signed-manifest + binary fetch). `configure_client` is
    // infallible, so the fallible PEM load already happened in `run_check` (fail-
    // closed); here we only add the pre-parsed certs. ADDITIVE - reqwest 0.13
    // routes `add_root_certificate` through
    // `rustls_platform_verifier::Verifier::new_with_extra_roots(extra, platform)`
    // (reqwest-0.13 async_impl/client.rs), i.e. the OS/enterprise platform roots
    // PLUS our extra roots; the built-in roots are never disabled and TLS
    // verification is never bypassed (the plugin does not call `tls_certs_only`).
    if !ca_certs.is_empty() {
        builder = builder.configure_client(move |client_builder| {
            let mut cb = client_builder;
            for cert in &ca_certs {
                cb = cb.add_root_certificate(cert.clone());
            }
            cb
        });
    }
    builder.build().map_err(|e| {
        CommandError::with_code(
            ErrorCode::UpdateEndpointUnreachable,
            format!("could not build updater: {e}"),
        )
    })
}

/// Issue #34: parse the configured custom root CA into reqwest-**0.13**
/// certificates for the updater plugin's client (fail-closed). The workspace +
/// `driven-tls` are on reqwest 0.12, but `tauri-plugin-updater` is on 0.13, so
/// the plugin's `add_root_certificate` needs a 0.13 `Certificate` - we re-parse
/// the same PEM here rather than mixing incompatible reqwest types. Returns an
/// empty Vec when no CA is configured (the plugin keeps its default trust).
fn load_updater_ca_certs(
    ca: &driven_tls::CustomCaConfig,
) -> CommandResult<Vec<reqwest_updater::Certificate>> {
    let Some(path) = ca.path() else {
        return Ok(Vec::new());
    };
    let pem = std::fs::read(path).map_err(|e| {
        CommandError::with_code(
            ErrorCode::UpdateEndpointUnreachable,
            format!("custom root CA file could not be read for the updater: {e}"),
        )
    })?;
    let certs = reqwest_updater::Certificate::from_pem_bundle(&pem).map_err(|e| {
        CommandError::with_code(
            ErrorCode::UpdateEndpointUnreachable,
            format!("custom root CA file is not valid PEM for the updater: {e}"),
        )
    })?;
    if certs.is_empty() {
        // Fail closed: a configured CA that trusts nothing is a config error.
        return Err(CommandError::with_code(
            ErrorCode::UpdateEndpointUnreachable,
            "custom root CA file contained no certificates".to_string(),
        ));
    }
    Ok(certs)
}

// ---------------------------------------------------------------------------
// Pure decision logic (the unit-test seam - no network)
// ---------------------------------------------------------------------------

/// Map a checked [`tauri_plugin_updater::Update`] to the frozen [`UpdateInfo`]
/// DTO (SPEC s11.6 / s11.7). Pure: no network, no plugin call. The `date` is the
/// RFC3339 string the manifest carried (if any); `body` is the release notes.
#[must_use]
fn build_update_info(update: &tauri_plugin_updater::Update, channel: Channel) -> UpdateInfo {
    UpdateInfo {
        version: update.version.clone(),
        notes: update.body.clone().filter(|b| !b.is_empty()),
        // `Update.date` is an `Option<OffsetDateTime>`; its `Display` is an
        // ISO-8601-shaped timestamp the UI parses with `new Date(..)` (falling
        // back to the raw string if unparseable). We avoid a direct `time`
        // dependency by using `Display` rather than a format-description call.
        published_at: update.date.map(|d| d.to_string()),
        channel: channel.as_str().to_string(),
    }
}

/// Pure: build an [`UpdateInfo`] from a peeked pending-update snapshot (R2-P1-3).
/// `snapshot` is `(version, notes, published_at, channel_str)` as
/// [`AppState::peek_pending_update`] returns it; the channel string is normalized
/// through [`Channel::from_str_lenient`] so a corrupt tag reports `stable` rather
/// than leaking garbage. Pure + unit-tested (no AppHandle / network).
#[must_use]
fn pending_info_from_snapshot(
    snapshot: (String, Option<String>, Option<String>, String),
) -> UpdateInfo {
    let (version, notes, published_at, channel_str) = snapshot;
    UpdateInfo {
        version,
        notes: notes.filter(|b| !b.is_empty()),
        published_at,
        channel: Channel::from_str_lenient(&channel_str).as_str().to_string(),
    }
}

/// Pure: the channel whose name `updater:downloaded` reports for a pending
/// update tagged with `channel_str` (R1-P2-3). Returns the canonical channel
/// string (`stable` | `dev`) - a dev update reports `dev`, not the old
/// hardcoded `stable`. Lenient parse means a corrupt tag falls back to stable.
#[must_use]
fn downloaded_channel(channel_str: &str) -> String {
    Channel::from_str_lenient(channel_str).as_str().to_string()
}

/// Pure: whether `install_update` should RESTORE the pending update after the
/// `download_and_install` result `is_err` (R1-P2-2). On any error we restore so
/// the banner's next Install retries; on success the value stays taken (the app
/// relaunches). Factored out so the keep-pending policy is unit-tested without a
/// live download / a real `Update`.
#[must_use]
fn should_restore_pending(install_was_err: bool) -> bool {
    install_was_err
}

/// What a check / channel switch should do to the recorded pending update
/// (R5-P2-1). Factored out so the clear-vs-keep policy is unit-tested without a
/// real `AppState` / a constructible `tauri_plugin_updater::Update`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    /// Record the freshly-found update as the pending one.
    SetFound,
    /// Clear any recorded pending update (it is stale / no longer applicable).
    Clear,
}

/// Pure: the [`PendingAction`] for a completed check (R5-P2-1). An AVAILABLE
/// update (`update_found = true`) is recorded; an UpToDate result (`false`)
/// CLEARS any prior pending update so `get_pending_update_info` /
/// `install_update` never refer to an already-installed or superseded update.
#[must_use]
fn pending_action_for_check(update_found: bool) -> PendingAction {
    if update_found {
        PendingAction::SetFound
    } else {
        PendingAction::Clear
    }
}

/// Pure: a successful channel switch ALWAYS clears the pending update (R5-P2-1) -
/// any update recorded on the old channel is invalid on the new one. Kept as a
/// named predicate so the "channel switch clears pending" invariant is explicit
/// and unit-asserted rather than implied by an inline `set_pending_update(None)`.
#[must_use]
fn pending_action_for_channel_switch() -> PendingAction {
    PendingAction::Clear
}

/// R6-P2-1: how a PERIODIC check should surface an available update, decided per
/// channel (DESIGN s9.4). `Stable` shows the manual in-app banner (the user is
/// asked to apply, default 24h deferral); `Dev` applies SILENTLY with a tray
/// notification (the dev channel is for power users who want the freshest build).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AvailableUpdateDelivery {
    /// Stable: record the pending update + emit `updater:available` so the in-app
    /// banner asks the user to apply (the existing manual path).
    ManualBanner,
    /// Dev: download + install the update SILENTLY and raise a tray notification;
    /// the staged update applies on the next Driven restart (DESIGN s9.4).
    SilentInstall,
}

/// Pure (R6-P2-1): the [`AvailableUpdateDelivery`] for an available update on a
/// PERIODIC check, per DESIGN s9.4. `Dev` -> silent install + tray notify; every
/// other channel -> the manual banner. Factored out so the per-channel routing is
/// unit-tested without a real `AppHandle` / a constructible `Update`.
#[must_use]
fn delivery_for_periodic_available(channel: Channel) -> AvailableUpdateDelivery {
    match channel {
        Channel::Dev => AvailableUpdateDelivery::SilentInstall,
        Channel::Stable => AvailableUpdateDelivery::ManualBanner,
    }
}

/// R8-P1-2 (SPEC s15 / DESIGN s9.4): whether an auto-update install is permitted
/// in-app on this OS, or must be deferred to a MANUAL DMG reinstall. Unsigned
/// macOS V1 cannot reliably apply an in-app update, so BOTH the periodic dev
/// silent-install path and the manual `install_update` command must short-circuit
/// to surfacing manual availability instead of calling `download_and_install`.
/// Windows + Linux install in-app unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallDisposition {
    /// Install the update in-app via `download_and_install` (Windows / Linux).
    Install,
    /// macOS: do NOT install in-app; surface the manual DMG-reinstall path
    /// (release/download URL) + a tray/event note instead.
    ManualOnMacos,
}

/// Pure (R8-P1-2): the [`InstallDisposition`] for the running OS. Takes the
/// "is this macOS" verdict as a parameter (production passes
/// `cfg!(target_os = "macos")`) so BOTH arms are reachable + unit-tested on
/// every host, and there is no cfg-gated dead code for 3-OS clippy.
#[must_use]
fn install_disposition(os_is_macos: bool) -> InstallDisposition {
    if os_is_macos {
        InstallDisposition::ManualOnMacos
    } else {
        InstallDisposition::Install
    }
}

/// The outcome of an update check, as the pure dispatch path sees it.
#[derive(Debug, Clone)]
enum CheckOutcome {
    /// A newer signed release is available on the active channel.
    Available(UpdateInfo),
    /// The running build is up to date (no eligible newer release).
    UpToDate,
}

/// Dispatch a check outcome to the side effects (SPEC s15.2): on
/// [`CheckOutcome::Available`] notify the webview (`updater:available`) and
/// return the info so the caller records the pending update; on
/// [`CheckOutcome::UpToDate`] do nothing and return `None`.
///
/// The notify closure is a parameter (not a direct `app.emit`) so a unit test
/// can assert the emit fires for an available update WITHOUT a real `AppHandle`
/// or the live endpoint. Production passes a closure that calls
/// [`crate::events::emit_updater_available`].
fn dispatch_check_outcome<F>(outcome: CheckOutcome, mut notify: F) -> Option<UpdateInfo>
where
    F: FnMut(&UpdateInfo),
{
    match outcome {
        CheckOutcome::Available(info) => {
            notify(&info);
            Some(info)
        }
        CheckOutcome::UpToDate => None,
    }
}

/// Run a real check against `channel`'s endpoint and turn the plugin result into
/// a [`CheckOutcome`] (the ONLY place that touches the network). Returns the
/// outcome plus the raw `Update` (so the caller can stash it as pending for a
/// later install). A check/transport failure maps to
/// `update.endpoint_unreachable`.
async fn run_check(
    app: &AppHandle,
    channel: Channel,
) -> CommandResult<(CheckOutcome, Option<tauri_plugin_updater::Update>)> {
    // Issue #34: resolve + parse the custom root CA (fail-closed) so the plugin's
    // download client trusts a corporate TLS-inspection CA.
    let ca_certs = match app.try_state::<AppState>() {
        Some(state) => {
            let ca = crate::commands::settings::load_custom_ca_config(state.state().as_ref())
                .await
                .unwrap_or_default();
            load_updater_ca_certs(&ca)?
        }
        None => Vec::new(),
    };
    let updater = build_updater(app, channel, ca_certs)?;
    let result = updater.check().await.map_err(|e| {
        CommandError::with_code(
            ErrorCode::UpdateEndpointUnreachable,
            format!("update check failed: {e}"),
        )
    })?;
    match result {
        Some(update) => {
            let info = build_update_info(&update, channel);
            Ok((CheckOutcome::Available(info), Some(update)))
        }
        None => Ok((CheckOutcome::UpToDate, None)),
    }
}

// ---------------------------------------------------------------------------
// Periodic check task (SPEC s15.2 - startup + every 6h, no orphan)
// ---------------------------------------------------------------------------

/// Spawn the periodic update-check task (SPEC s15.2): an immediate check on
/// startup, then one every [`CHECK_INTERVAL`] (6h) via a tokio `interval`. The
/// task `select!`s on a shutdown watch so an explicit Quit stops it promptly
/// (rather than waiting out the interval), and its handle + shutdown sender are
/// registered on [`AppState`] so the quit drain joins it with NO orphan.
///
/// Each tick reads the CURRENT channel from settings (so a channel toggle takes
/// effect on the next tick without a restart), runs a check, and on an available
/// update records the pending update + emits `updater:available`. A check error
/// (e.g. offline) is logged and the loop continues - a transient network failure
/// must not kill the periodic checker.
pub fn spawn_periodic_check(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        tracing::warn!(target: TARGET, "AppState not managed; updater periodic check not started");
        return;
    };
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let app_handle = app.clone();

    // `tokio::spawn` (not `tauri::async_runtime::spawn`) so the returned handle is
    // a `tokio::task::JoinHandle`, matching the no-orphan drain in lib.rs (which
    // `select!`s + aborts a `tokio` handle). Spawned from inside the setup
    // `block_on`, so a reactor is active.
    let task = tokio::spawn(async move {
        // Fire the first check immediately, then on the interval. `interval`'s
        // first tick completes instantly, so the loop body runs once up front.
        let mut ticker = tokio::time::interval(CHECK_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                // Shutdown wins so quit is prompt.
                res = shutdown_rx.changed() => {
                    match res {
                        Ok(()) if *shutdown_rx.borrow() => break,
                        Ok(()) => {}
                        Err(_) => break, // sender dropped
                    }
                }
                _ = ticker.tick() => {
                    periodic_check_once(&app_handle).await;
                }
            }
        }
        tracing::debug!(target: TARGET, "updater periodic check task exited");
    });

    state.set_updater_task(task, shutdown_tx);
    tracing::info!(target: TARGET, interval_secs = CHECK_INTERVAL.as_secs(), "updater periodic check started");
}

/// One periodic-check iteration: read the active channel, run a check, and on an
/// available update surface it PER CHANNEL (R6-P2-1, DESIGN s9.4) - STABLE records
/// the pending update + emits `updater:available` (manual banner), DEV downloads +
/// installs silently + tray-notifies. All failures are logged, never propagated
/// (the loop must survive a transient network error).
async fn periodic_check_once(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let channel = match read_channel(state.state().as_ref()).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(target: TARGET, error = %e, "updater: could not read channel; skipping check");
            return;
        }
    };
    match run_check(app, channel).await {
        Ok((CheckOutcome::Available(info), Some(update))) => {
            // R6-P2-1 (DESIGN s9.4): the DEV channel applies updates SILENTLY with a
            // tray notification; STABLE shows the manual in-app banner. Route per
            // channel.
            match delivery_for_periodic_available(channel) {
                AvailableUpdateDelivery::SilentInstall => {
                    // R8-P1-2 (SPEC s15 / DESIGN s9.4): the dev silent-install path
                    // must NOT call `download_and_install` on macOS (unsigned V1: the
                    // in-app updater is unreliable there). On macOS, surface MANUAL
                    // availability instead - record the pending update + emit
                    // `updater:available` (About.vue's macOS guard then shows the DMG
                    // download link, not an in-app Install button) and raise a tray
                    // note. Windows/Linux install silently as before.
                    match install_disposition(cfg!(target_os = "macos")) {
                        InstallDisposition::Install => {
                            // Dev (non-macOS): do NOT stash a pending update / emit the
                            // manual banner - download + install now, then tray-notify.
                            // The staged update applies on the next Driven restart (we
                            // do not force a restart mid-work; see CODEX_NOTES M9 fix
                            // round 6). Clear any stale pending update from a prior
                            // check so the banner never shows.
                            state.set_pending_update(None);
                            tracing::info!(target: TARGET, version = %info.version, "dev update available; installing silently (periodic check)");
                            silent_install_dev_update(app, update, &info).await;
                        }
                        InstallDisposition::ManualOnMacos => {
                            // macOS: record the pending update + its channel (so the
                            // About surface can show it) and emit `updater:available`
                            // + a tray note pointing at the manual DMG. NEVER reaches
                            // `download_and_install`.
                            state.set_pending_update(Some((update, channel.as_str().to_string())));
                            let app_for_emit = app.clone();
                            let _ = dispatch_check_outcome(
                                CheckOutcome::Available(info.clone()),
                                |info| {
                                    if let Err(e) =
                                        crate::events::emit_updater_available(&app_for_emit, info)
                                    {
                                        tracing::debug!(target: TARGET, error = %e, "emit updater:available failed");
                                    }
                                },
                            );
                            tracing::info!(target: TARGET, version = %info.version, "dev update available on macOS; surfacing MANUAL DMG reinstall (in-app updater disabled on unsigned macOS V1)");
                            crate::tray::notify_manual_update_available(app, &info.version);
                        }
                    }
                }
                AvailableUpdateDelivery::ManualBanner => {
                    // Stable: record the raw Update + its channel so install_update
                    // can apply it directly + emit the real channel on downloaded,
                    // and emit `updater:available` so the in-app banner asks the user.
                    state.set_pending_update(Some((update, channel.as_str().to_string())));
                    let app_for_emit = app.clone();
                    let _ = dispatch_check_outcome(CheckOutcome::Available(info.clone()), |info| {
                        if let Err(e) = crate::events::emit_updater_available(&app_for_emit, info) {
                            tracing::debug!(target: TARGET, error = %e, "emit updater:available failed");
                        }
                    });
                    tracing::info!(target: TARGET, version = %info.version, channel = %info.channel, "update available (periodic check, manual banner)");
                }
            }
        }
        Ok((CheckOutcome::UpToDate, _)) | Ok((_, None)) => {
            // R5-P2-1: UpToDate clears any stale pending update from an earlier
            // check (e.g. already installed, or left over from before a channel
            // switch) so get_pending_update_info / install_update never refer to a
            // no-longer-available update.
            match pending_action_for_check(false) {
                PendingAction::Clear => state.set_pending_update(None),
                PendingAction::SetFound => {}
            }
            tracing::debug!(target: TARGET, channel = channel.as_str(), "no update available (periodic check)");
        }
        Err(e) => {
            tracing::debug!(target: TARGET, error = %e, channel = channel.as_str(), "updater periodic check failed (will retry next interval)");
        }
    }
}

/// R6-P2-1 (DESIGN s9.4): download + install a DEV-channel update SILENTLY, then
/// raise a tray notification. Reuses the same `download_and_install` plumbing the
/// manual `install_update` uses (emitting `updater:download_progress` +
/// `updater:downloaded` so any open UI still reflects progress), but does NOT
/// force a restart - the staged update applies on the next Driven restart, so a
/// power user is not interrupted mid-work. A download/verify failure is logged and
/// swallowed (the periodic loop must survive it; the next interval retries); it is
/// NOT surfaced as a banner (dev is the silent channel).
async fn silent_install_dev_update(
    app: &AppHandle,
    update: tauri_plugin_updater::Update,
    info: &UpdateInfo,
) {
    let channel = Channel::Dev;

    let progress_app = app.clone();
    let mut downloaded: u64 = 0;
    let on_chunk = move |chunk_len: usize, content_len: Option<u64>| {
        downloaded = downloaded.saturating_add(chunk_len as u64);
        if let Err(e) =
            crate::events::emit_updater_download_progress(&progress_app, downloaded, content_len)
        {
            tracing::debug!(target: TARGET, error = %e, "emit updater:download_progress failed");
        }
    };

    let done_app = app.clone();
    let done_info = build_update_info(&update, channel);
    let on_done = move || {
        if let Err(e) = crate::events::emit_updater_downloaded(&done_app, &done_info) {
            tracing::debug!(target: TARGET, error = %e, "emit updater:downloaded failed");
        }
    };

    // Issue #125: shut the elevated VSS helper broker down BEFORE the installer
    // runs (same reason as the manual `install_update` path). The dev-silent path
    // ran `download_and_install` from the periodic task WITHOUT the app-quit
    // sweep, so a live broker either blocked the install or was left orphaned when
    // the installer restarted the app - the exact QA repro. Disabling also blocks
    // a mid-download re-launch; re-armed on failure since the app keeps running.
    let vss_disabled = app
        .try_state::<AppState>()
        .map(|s| s.shutdown_vss_helper_for_update())
        .unwrap_or(false);

    match update.download_and_install(on_chunk, on_done).await {
        Ok(()) => {
            tracing::info!(target: TARGET, version = %info.version, "dev update installed silently; applies on next restart");
            crate::tray::notify_dev_update_installed(app, &info.version);
        }
        Err(e) => {
            if vss_disabled {
                if let Some(s) = app.try_state::<AppState>() {
                    s.rearm_vss_helper_after_failed_update();
                }
            }
            tracing::warn!(target: TARGET, error = %e, version = %info.version, "dev silent update install failed (will retry next interval)");
        }
    }
}

// ---------------------------------------------------------------------------
// IPC commands (SPEC s15.2)
// ---------------------------------------------------------------------------

/// `check_for_update()` - a MANUAL update check against the active channel's
/// signed manifest (SPEC s15.2). Distinct from the M6 `check_for_updates`
/// (which queries the GitHub releases API for the About tab's "is there a newer
/// release" answer): this hits the Tauri `update.json` manifest + records the
/// pending update so `install_update` can apply it, and emits `updater:available`
/// on an available update. Returns the [`UpdateInfo`] or `None` when up to date.
#[tauri::command]
pub async fn check_for_update(
    app: AppHandle,
    state: State<'_, AppState>,
) -> CommandResult<Option<UpdateInfo>> {
    let channel = read_channel(state.state().as_ref()).await?;
    let (outcome, update) = run_check(&app, channel).await?;
    if let Some(update) = update {
        state.set_pending_update(Some((update, channel.as_str().to_string())));
    } else {
        // Up to date: clear any stale pending update from an earlier check.
        state.set_pending_update(None);
    }
    let app_for_emit = app.clone();
    Ok(dispatch_check_outcome(outcome, |info| {
        if let Err(e) = crate::events::emit_updater_available(&app_for_emit, info) {
            tracing::debug!(target: TARGET, error = %e, "emit updater:available failed");
        }
    }))
}

/// `install_update()` - stage + apply the pending update and relaunch (SPEC
/// s15.2).
///
/// Takes the pending [`tauri_plugin_updater::Update`] (recorded by the most
/// recent check), `download_and_install`s it - emitting
/// `updater:download_progress { downloaded, total }` on every chunk and
/// `updater:downloaded` when the bytes are staged - then relaunches via
/// `tauri-plugin-process` (`app.restart()`, which does not return). A missing
/// pending update (no check found one) is rejected with a clear error; a
/// signature / download failure maps to the s24 update code surface.
#[tauri::command]
pub async fn install_update(app: AppHandle, state: State<'_, AppState>) -> CommandResult<()> {
    // R8-P1-2 (SPEC s15 / DESIGN s9.4): on macOS the in-app updater is unreliable
    // (unsigned V1), so `install_update` must NEVER reach `download_and_install`.
    // Short-circuit to the manual-DMG path - leave the pending update intact (so
    // the About surface can still show the DMG download link) and raise a tray
    // note. The UI renders the returned `update.manual_required_macos` code via
    // `t("errors.update.manual_required_macos.long")`. Windows/Linux install in
    // place unchanged.
    if install_disposition(cfg!(target_os = "macos")) == InstallDisposition::ManualOnMacos {
        if let Some((version, ..)) = state.peek_pending_update() {
            crate::tray::notify_manual_update_available(&app, &version);
        }
        return Err(CommandError::with_code(
            ErrorCode::UpdateManualRequiredMacos,
            "in-app update install is disabled on macOS; download and reinstall the latest release manually",
        ));
    }

    // Take the pending update + its channel. On a download/install FAILURE we
    // restore it (R1-P2-2) so the banner's next Install retries the SAME update
    // instead of failing "no pending update" until the user re-checks. On
    // SUCCESS the app relaunches, so leaving it taken is correct.
    let (update, channel_str) = state.take_pending_update().ok_or_else(|| {
        CommandError::with_code(
            ErrorCode::UpdateEndpointUnreachable,
            "no pending update to install; run a check first",
        )
    })?;
    // The display channel for the downloaded event (R1-P2-3).
    let channel = Channel::from_str_lenient(&downloaded_channel(&channel_str));

    // Issue #125: shut the elevated VSS helper broker down BEFORE the installer
    // runs. `download_and_install` executes the NSIS installer synchronously
    // (`/P /R`), which overwrites the bundled `driven-vss-helper.exe` sidecar -
    // but the stock installer only terminates the MAIN binary, so a live broker
    // holds its own exe open and the install fails with "Error opening file for
    // writing: ...driven-vss-helper.exe". Disabling (not a bare shutdown) also
    // blocks a still-running sync from re-launching the broker mid-download. This
    // ALSO stops the update path itself from seeding an orphan: the pre-fix path
    // let `/R` restart the app WITHOUT ever sweeping the helper. Re-armed below
    // if the install fails (the app keeps running).
    let vss_disabled = state.shutdown_vss_helper_for_update();

    // The progress callback accumulates downloaded bytes and emits
    // `updater:download_progress`. `content_length` arrives once the server
    // reports it; until then `total` is None.
    let progress_app = app.clone();
    let mut downloaded: u64 = 0;
    let on_chunk = move |chunk_len: usize, content_len: Option<u64>| {
        downloaded = downloaded.saturating_add(chunk_len as u64);
        if let Err(e) =
            crate::events::emit_updater_download_progress(&progress_app, downloaded, content_len)
        {
            tracing::debug!(target: TARGET, error = %e, "emit updater:download_progress failed");
        }
    };

    let done_app = app.clone();
    // R1-P2-3: emit `updater:downloaded` with the REAL channel the update came
    // from, not a hardcoded Stable.
    let done_info = build_update_info(&update, channel);
    let on_done = move || {
        if let Err(e) = crate::events::emit_updater_downloaded(&done_app, &done_info) {
            tracing::debug!(target: TARGET, error = %e, "emit updater:downloaded failed");
        }
    };

    // Install via `&update` so a failure leaves the value intact to restore.
    let install_result = update.download_and_install(on_chunk, on_done).await;
    if should_restore_pending(install_result.is_err()) {
        // R1-P2-2: restore the pending update (with its channel) so the user can
        // retry without re-checking.
        state.set_pending_update(Some((update, channel_str)));
        // Issue #125: the install failed and the app keeps running - re-arm the
        // VSS helper we disabled above so locked-file backup is not left silently
        // degraded for the rest of the session.
        if vss_disabled {
            state.rearm_vss_helper_after_failed_update();
        }
    }
    install_result.map_err(map_install_error)?;

    tracing::info!(target: TARGET, "update staged; relaunching");
    // Relaunch into the freshly-installed version (tauri-plugin-process). This
    // does not return on success.
    app.restart();
}

/// Map a `download_and_install` failure to the s24 update code surface (SPEC
/// s24): a signature-verification failure -> `update.signature_invalid`;
/// anything else (download / IO / staging) -> `update.endpoint_unreachable`. The
/// plugin's error `Display` carries the discriminating text.
fn map_install_error(e: tauri_plugin_updater::Error) -> CommandError {
    let msg = e.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("signature") || lower.contains("verify") || lower.contains("verification") {
        CommandError::with_code(ErrorCode::UpdateSignatureInvalid, msg)
    } else {
        CommandError::with_code(ErrorCode::UpdateEndpointUnreachable, msg)
    }
}

/// `get_pending_update_info()` - the pending available update, if any (R2-P1-3,
/// SPEC s15.2). The STARTUP periodic check (lib.rs setup) can find + record an
/// update + emit `updater:available` BEFORE the webview has attached its
/// listeners, so that one-shot event is lost. The app-root updater-store boot
/// (App.vue) calls this on startup to HYDRATE the banner from the recorded
/// pending update, so a missed startup emit still surfaces. Non-consuming (peek):
/// `install_update` still finds the pending update afterward. Returns `None` when
/// no check has recorded an update.
#[tauri::command]
pub async fn get_pending_update_info(
    state: State<'_, AppState>,
) -> CommandResult<Option<UpdateInfo>> {
    Ok(state.peek_pending_update().map(pending_info_from_snapshot))
}

/// `get_update_channel()` - the active updater channel (SPEC s15.2), as the
/// `stable` | `dev` string the UI toggle binds to.
#[tauri::command]
pub async fn get_update_channel(state: State<'_, AppState>) -> CommandResult<String> {
    Ok(read_channel(state.state().as_ref())
        .await?
        .as_str()
        .to_string())
}

/// `set_update_channel(channel)` - switch the active updater channel (SPEC
/// s15.2). Validates the value (`stable` | `dev`) and persists it into the
/// `updater.channel` settings field (preserving sibling fields). The next
/// periodic / manual check uses the new channel.
#[tauri::command]
pub async fn set_update_channel(
    state: State<'_, AppState>,
    channel: String,
) -> CommandResult<String> {
    let parsed = match channel.as_str() {
        "stable" => Channel::Stable,
        "dev" => Channel::Dev,
        other => {
            return Err(CommandError::with_code(
                ErrorCode::InvalidInput,
                format!("update channel must be `stable` or `dev` (got `{other}`)"),
            ))
        }
    };
    write_channel(state.state().as_ref(), parsed).await?;
    // R5-P2-1: a channel switch invalidates any pending update recorded on the
    // OLD channel - get_pending_update_info / install_update must not still offer
    // an old-channel update after the switch. Clear it; the next periodic /
    // manual check on the new channel records a fresh one if applicable.
    match pending_action_for_channel_switch() {
        PendingAction::Clear => state.set_pending_update(None),
        PendingAction::SetFound => {}
    }
    tracing::info!(target: TARGET, channel = parsed.as_str(), "update channel changed");
    Ok(parsed.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_core::state::sqlite::SqliteStateRepo;
    use std::cell::RefCell;

    /// A temp-backed state repo (migrations run on open) for the channel
    /// round-trip tests. No real Drive / keychain / network touched.
    async fn temp_repo() -> (SqliteStateRepo, std::path::PathBuf) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("driven-updater-test-{nonce}-{:p}", &nonce));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let repo = SqliteStateRepo::open(&dir.join("state.db"))
            .await
            .expect("open state repo");
        (repo, dir)
    }

    fn cleanup(dir: std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn channel_parse_and_default() {
        assert_eq!(Channel::from_str_lenient("stable"), Channel::Stable);
        assert_eq!(Channel::from_str_lenient("dev"), Channel::Dev);
        // Unknown / garbage defaults to Stable (never silently opts into dev).
        assert_eq!(Channel::from_str_lenient(""), Channel::Stable);
        assert_eq!(Channel::from_str_lenient("nightly"), Channel::Stable);
        assert_eq!(Channel::Stable.as_str(), "stable");
        assert_eq!(Channel::Dev.as_str(), "dev");
    }

    #[test]
    fn channel_endpoint_url_is_per_channel_and_has_no_channel_placeholder() {
        // SPEC s15.1: the channel is in the PATH; `{{channel}}` is NOT a valid
        // Tauri placeholder, so it must NOT appear in either URL. The valid
        // placeholders ({{target}} / {{current_version}}) DO appear.
        let stable = Channel::Stable.endpoint_url();
        let dev = Channel::Dev.endpoint_url();
        assert!(stable.contains("/updates/stable/"), "stable URL: {stable}");
        assert!(dev.contains("/updates/dev/"), "dev URL: {dev}");
        assert_ne!(stable, dev);
        for url in [stable, dev] {
            assert!(
                !url.contains("{{channel}}"),
                "no {{channel}} placeholder: {url}"
            );
            assert!(url.contains("{{target}}"), "has target placeholder: {url}");
            assert!(url.contains("{{arch}}"), "has arch placeholder: {url}");
            // R1-P1-1: the path MUST NOT carry the installed version - the
            // manifest carries the latest version instead, so the updater
            // actually discovers a newer release.
            assert!(
                !url.contains("{{current_version}}"),
                "no current_version segment (R1-P1-1): {url}"
            );
            assert!(
                url.ends_with("/update.json"),
                "ends with update.json: {url}"
            );
            // The URL must parse as a real URL (modulo the placeholders, which
            // are valid path chars).
            assert!(url.parse::<tauri::Url>().is_ok(), "parses: {url}");
        }
    }

    #[tokio::test]
    async fn channel_get_set_round_trips_through_settings() {
        // SPEC s15.2: the channel persists to / reads from the `updater.channel`
        // settings group, defaulting to Stable.
        let (repo, dir) = temp_repo().await;

        // Default (seeded migration) is stable.
        assert_eq!(read_channel(&repo).await.unwrap(), Channel::Stable);

        // Switch to dev and read it back.
        write_channel(&repo, Channel::Dev).await.unwrap();
        assert_eq!(read_channel(&repo).await.unwrap(), Channel::Dev);

        // The sibling field (check_interval_secs) is preserved across a channel
        // write (we only mutate `channel`).
        let raw = repo.get_setting(KEY_UPDATER).await.unwrap().unwrap();
        assert_eq!(raw.get("channel").and_then(|v| v.as_str()), Some("dev"));
        assert!(
            raw.get("check_interval_secs").is_some(),
            "check_interval_secs preserved: {raw}"
        );

        // Switch back to stable.
        write_channel(&repo, Channel::Stable).await.unwrap();
        assert_eq!(read_channel(&repo).await.unwrap(), Channel::Stable);

        cleanup(dir);
    }

    #[test]
    fn dispatch_available_outcome_emits_and_returns_info() {
        // SPEC s15.2: an AVAILABLE outcome must invoke the notify side effect
        // (production emits updater:available) AND return the info so the caller
        // records the pending update. This is the available-update emit path,
        // exercised WITHOUT a real AppHandle or the live endpoint.
        let info = UpdateInfo {
            version: "0.2.0".to_string(),
            notes: Some("Faster sync.".to_string()),
            published_at: Some("2026-06-24T00:00:00Z".to_string()),
            channel: "stable".to_string(),
        };
        let emitted: RefCell<Vec<UpdateInfo>> = RefCell::new(Vec::new());
        let returned = dispatch_check_outcome(CheckOutcome::Available(info.clone()), |i| {
            emitted.borrow_mut().push(i.clone());
        });
        // Emitted exactly once, with the same info, and returned for the caller.
        assert_eq!(emitted.borrow().len(), 1);
        assert_eq!(emitted.borrow()[0].version, "0.2.0");
        assert_eq!(returned.as_ref().map(|i| i.version.as_str()), Some("0.2.0"));
    }

    #[test]
    fn downloaded_event_carries_the_real_channel() {
        // R1-P2-3: the `updater:downloaded` payload reports the channel the
        // pending update actually came from - a dev update must report `dev`,
        // not the old hardcoded `stable`. A corrupt/empty tag falls back to
        // stable (never silently claims dev).
        assert_eq!(downloaded_channel("dev"), "dev");
        assert_eq!(downloaded_channel("stable"), "stable");
        assert_eq!(downloaded_channel(""), "stable");
        assert_eq!(downloaded_channel("garbage"), "stable");
    }

    #[test]
    fn pending_update_survives_a_failed_install_only() {
        // R1-P2-2: a FAILED `download_and_install` must restore the pending
        // update (so the banner's next Install retries); a SUCCESS leaves it
        // taken (the app relaunches). This is the keep-pending policy the
        // install command applies to the real download result.
        assert!(
            should_restore_pending(true),
            "failed install must restore pending"
        );
        assert!(
            !should_restore_pending(false),
            "successful install must NOT restore pending"
        );
    }

    #[test]
    fn pending_info_snapshot_maps_and_normalizes_channel() {
        // R2-P1-3: get_pending_update_info hydrates the app-root store from the
        // recorded pending update. The pure mapper builds the frozen UpdateInfo
        // from the owned peek snapshot, normalizes the channel string (a corrupt
        // tag reports `stable`, never garbage), and drops an empty notes body.
        let dev = pending_info_from_snapshot((
            "0.1.1-dev.5.abc1234".to_string(),
            Some("Dev build notes.".to_string()),
            Some("2026-06-24T00:00:00Z".to_string()),
            "dev".to_string(),
        ));
        assert_eq!(dev.version, "0.1.1-dev.5.abc1234");
        assert_eq!(dev.channel, "dev");
        assert_eq!(dev.notes.as_deref(), Some("Dev build notes."));
        assert_eq!(dev.published_at.as_deref(), Some("2026-06-24T00:00:00Z"));

        // Empty notes -> None; corrupt channel tag -> stable.
        let stable = pending_info_from_snapshot((
            "0.2.0".to_string(),
            Some(String::new()),
            None,
            "garbage".to_string(),
        ));
        assert_eq!(stable.channel, "stable");
        assert!(stable.notes.is_none());
        assert!(stable.published_at.is_none());
    }

    #[test]
    fn up_to_date_check_clears_pending_available_update_sets() {
        // R5-P2-1: periodic_check_once must CLEAR a prior pending update on an
        // UpToDate result (so a no-longer-available / already-installed update is
        // not offered), and SET the new one when a check finds an update. This is
        // the pure clear-vs-keep policy that branch routes through.
        assert_eq!(
            pending_action_for_check(false),
            PendingAction::Clear,
            "UpToDate (no update found) must clear any stale pending update"
        );
        assert_eq!(
            pending_action_for_check(true),
            PendingAction::SetFound,
            "an available update must be recorded as pending"
        );
    }

    #[test]
    fn channel_switch_always_clears_pending() {
        // R5-P2-1: set_update_channel must clear the pending update after a
        // successful channel write - an update recorded on the old channel must
        // not still be offered by get_pending_update_info / install_update after
        // the switch. The action is unconditional Clear.
        assert_eq!(
            pending_action_for_channel_switch(),
            PendingAction::Clear,
            "a channel switch invalidates any old-channel pending update"
        );
    }

    #[test]
    fn dev_channel_periodic_available_takes_silent_install_path() {
        // R6-P2-1 (DESIGN s9.4): a DEV-channel periodic check with an available
        // update applies SILENTLY (download/install + tray notify) - NOT the manual
        // in-app banner. STABLE keeps the manual banner path. This is the pure
        // per-channel routing `periodic_check_once` branches on.
        assert_eq!(
            delivery_for_periodic_available(Channel::Dev),
            AvailableUpdateDelivery::SilentInstall,
            "dev updates apply silently with a tray notification (DESIGN s9.4)"
        );
        assert_eq!(
            delivery_for_periodic_available(Channel::Stable),
            AvailableUpdateDelivery::ManualBanner,
            "stable updates use the manual in-app banner"
        );
    }

    #[test]
    fn macos_install_disposition_is_manual_other_oses_install() {
        // R8-P1-2 (SPEC s15 / DESIGN s9.4): on macOS BOTH the dev periodic silent
        // install AND the manual `install_update` command must take the
        // MANUAL-availability branch (never `download_and_install`); on every other
        // OS they install in-app. This is the pure per-OS routing both paths branch
        // on (production passes `cfg!(target_os = "macos")`).
        assert_eq!(
            install_disposition(true),
            InstallDisposition::ManualOnMacos,
            "macOS must NOT install in-app (unsigned V1: manual DMG reinstall)"
        );
        assert_eq!(
            install_disposition(false),
            InstallDisposition::Install,
            "Windows/Linux install the update in-app"
        );
    }

    #[test]
    fn dispatch_up_to_date_outcome_does_not_emit() {
        // An UP-TO-DATE outcome must NOT emit and must return None.
        let emitted: RefCell<Vec<UpdateInfo>> = RefCell::new(Vec::new());
        let returned = dispatch_check_outcome(CheckOutcome::UpToDate, |i| {
            emitted.borrow_mut().push(i.clone());
        });
        assert!(emitted.borrow().is_empty());
        assert!(returned.is_none());
    }

    // A single self-signed test CA (RSA-2048, CN "Driven Test CA 1").
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
    fn load_updater_ca_certs_covers_none_valid_and_bad() {
        // Issue #34: the updater's reqwest-0.13 CA parse. `None` -> no certs (the
        // plugin keeps its default trust); a valid PEM -> the certs; a garbage /
        // missing file -> fail-closed error (mapped to the update error code).
        assert!(load_updater_ca_certs(&driven_tls::CustomCaConfig::none())
            .expect("none is ok")
            .is_empty());

        let dir = std::env::temp_dir().join(format!("driven-updater-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mk dir");
        let good = dir.join("ca.pem");
        std::fs::write(&good, TEST_CA_PEM).expect("write pem");
        let certs = load_updater_ca_certs(&driven_tls::CustomCaConfig::from_path(Some(good)))
            .expect("valid pem parses");
        assert_eq!(certs.len(), 1);

        let garbage = dir.join("garbage.pem");
        std::fs::write(&garbage, b"not a cert\n").expect("write garbage");
        let err = load_updater_ca_certs(&driven_tls::CustomCaConfig::from_path(Some(garbage)))
            .expect_err("garbage fails closed");
        assert_eq!(err.code, ErrorCode::UpdateEndpointUnreachable);

        let missing = dir.join("nope.pem");
        let err = load_updater_ca_certs(&driven_tls::CustomCaConfig::from_path(Some(missing)))
            .expect_err("missing fails closed");
        assert_eq!(err.code, ErrorCode::UpdateEndpointUnreachable);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
