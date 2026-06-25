//! In-app updater (SPEC s15.2, ROADMAP M9).
//!
//! Two responsibilities:
//!
//! 1. RUNTIME CHANNEL SELECTION. The Tauri updater natively substitutes only
//!    `{{target}}`, `{{arch}}`, and `{{current_version}}` - **`{{channel}}` is
//!    NOT a valid placeholder** (SPEC s15.1). The channel (`stable` vs `dev`)
//!    therefore lives in the URL PATH and is chosen AT RUNTIME via
//!    `app.updater_builder().endpoints(vec![<channel URL>])`. [`build_updater`]
//!    is that wrapper. The active channel is read from / written to the SPEC s22
//!    `updater.channel` settings group (the same group the M6 Settings layer
//!    persists), so there is no ad-hoc state.
//!
//! 2. PERIODIC CHECK. On startup AND every [`CHECK_INTERVAL`] (6h) a background
//!    task ([`spawn_periodic_check`]) checks the active channel and, on an
//!    available update, records it as the pending update + emits
//!    `updater:available`. The task is a tokio `interval` loop (NOT a busy /
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

/// The stable-channel update-manifest URL (SPEC s15.1/s15.2). `{{target}}` /
/// `{{current_version}}` are substituted by the Tauri updater; the channel
/// (`stable`) is in the PATH, never a placeholder.
const STABLE_ENDPOINT: &str =
    "https://driven.maxhogan.dev/updates/stable/{{target}}/{{current_version}}/update.json";
/// The dev-channel update-manifest URL (SPEC s15.2). Pre-release / opt-in.
const DEV_ENDPOINT: &str =
    "https://driven.maxhogan.dev/updates/dev/{{target}}/{{current_version}}/update.json";

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
    /// the PATH; `{{target}}` / `{{current_version}}` are Tauri placeholders.
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
    app.updater_builder()
        .endpoints(vec![url])
        .map_err(|e| {
            CommandError::with_code(
                ErrorCode::UpdateEndpointUnreachable,
                format!("could not set update endpoint: {e}"),
            )
        })?
        .build()
        .map_err(|e| {
            CommandError::with_code(
                ErrorCode::UpdateEndpointUnreachable,
                format!("could not build updater: {e}"),
            )
        })
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
    let updater = build_updater(app, channel)?;
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
/// available update record the pending update + emit `updater:available`. All
/// failures are logged, never propagated (the loop must survive a transient
/// network error).
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
        Ok((outcome, update)) => {
            if matches!(outcome, CheckOutcome::Available(_)) {
                // Stash the raw Update so install_update can apply it directly.
                state.set_pending_update(update);
            }
            let app_for_emit = app.clone();
            let info = dispatch_check_outcome(outcome, |info| {
                if let Err(e) = crate::events::emit_updater_available(&app_for_emit, info) {
                    tracing::debug!(target: TARGET, error = %e, "emit updater:available failed");
                }
            });
            if let Some(info) = info {
                tracing::info!(target: TARGET, version = %info.version, channel = %info.channel, "update available (periodic check)");
            } else {
                tracing::debug!(target: TARGET, channel = channel.as_str(), "no update available (periodic check)");
            }
        }
        Err(e) => {
            tracing::debug!(target: TARGET, error = %e, channel = channel.as_str(), "updater periodic check failed (will retry next interval)");
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
    if matches!(outcome, CheckOutcome::Available(_)) {
        state.set_pending_update(update);
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
    let update = state.take_pending_update().ok_or_else(|| {
        CommandError::with_code(
            ErrorCode::UpdateEndpointUnreachable,
            "no pending update to install; run a check first",
        )
    })?;

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
    let done_info = build_update_info(&update, Channel::Stable);
    let on_done = move || {
        // Emit the channel-agnostic info; the channel field is only used for
        // display, and the install is for whatever channel the check used.
        if let Err(e) = crate::events::emit_updater_downloaded(&done_app, &done_info) {
            tracing::debug!(target: TARGET, error = %e, "emit updater:downloaded failed");
        }
    };

    update
        .download_and_install(on_chunk, on_done)
        .await
        .map_err(map_install_error)?;

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
            assert!(
                url.contains("{{current_version}}"),
                "has current_version placeholder: {url}"
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
    fn dispatch_up_to_date_outcome_does_not_emit() {
        // An UP-TO-DATE outcome must NOT emit and must return None.
        let emitted: RefCell<Vec<UpdateInfo>> = RefCell::new(Vec::new());
        let returned = dispatch_check_outcome(CheckOutcome::UpToDate, |i| {
            emitted.borrow_mut().push(i.clone());
        });
        assert!(emitted.borrow().is_empty());
        assert!(returned.is_none());
    }
}
