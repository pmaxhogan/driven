//! Anonymous usage telemetry client (SPEC s16, ROADMAP M9b).
//!
//! Driven sends a small, anonymous usage ping on startup and every 24h to
//! `https://driven.maxhogan.dev/telemetry/v1/ping` (the Cloudflare Worker in
//! `telemetry-worker/`), so the project can see aggregate health (uploads,
//! error rates, OS mix) WITHOUT collecting any PII.
//!
//! PRIVACY (load-bearing, SPEC s16): the payload carries ONLY counts, sizes,
//! error CODES, and latencies - NEVER a file name, path, or content. The only
//! stable identifier is a random `install_id` (a UUID v4 minted on first run);
//! it is anonymous and not linkable to a user. Telemetry is DEFAULT ON with a
//! single Settings toggle to disable it, and the toggle is honored IMMEDIATELY
//! (the periodic task re-reads the pref every tick, and when OFF makes NO
//! network call at all).
//!
//! BEST-EFFORT (SPEC s16): a send failure (offline, timeout, non-2xx) is logged
//! at debug/info and NEVER affects backups, never surfaces an error, never
//! retries in a storm. The HTTP call has a bounded timeout and the loop simply
//! waits for the next 24h tick.
//!
//! Shape mirrors [`crate::updater`]: a settings-backed pref + a tokio `interval`
//! task (NOT a sleep/poll loop) that `select!`s on a shutdown watch, with its
//! handle + shutdown sender tracked on [`AppState`] so the app-quit drain joins
//! it with NO orphan (the M5 no-orphan bookkeeping).
//!
//! TEST SEAM (no live endpoint in tests): the actual POST is hidden behind the
//! [`TelemetrySink`] trait. The payload build + the "should we send?" decision
//! are pure functions the unit tests exercise directly; the production sink
//! ([`HttpTelemetrySink`]) is the only part that touches the network and the
//! tests never use it, so nothing here hits `driven.maxhogan.dev`.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use driven_core::state::StateRepo;
use driven_core::time::Clock;

use crate::app_state::AppState;
use crate::commands::{CommandError, CommandResult};

/// Tracing target for the telemetry layer.
const TARGET: &str = "driven::app::telemetry";

/// The telemetry ping cadence (SPEC s16: "on startup AND every 24h"). A
/// `tokio::time::interval`, NOT a sleep/poll loop.
const PING_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The aggregation window for `events_24h` (SPEC s16): the last 24h of activity.
const WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// Bounded per-send HTTP timeout (SPEC s16: best-effort, no hang). A telemetry
/// POST must never block a tick for long; on timeout the send is dropped.
const SEND_TIMEOUT: Duration = Duration::from_secs(15);

/// The SPEC s22 `telemetry` settings KV key (must match settings.rs
/// `KEY_TELEMETRY`).
const KEY_TELEMETRY: &str = "telemetry";

/// The SPEC s22 `updater` settings KV key, read to report the active channel in
/// the payload (must match updater.rs / settings.rs `KEY_UPDATER`).
const KEY_UPDATER: &str = "updater";

/// The default ingest endpoint (SPEC s16). Overridable via the stored
/// `telemetry.endpoint` field (the migration 0002 seed writes this exact value),
/// so a test / self-host can repoint it without code changes.
const DEFAULT_ENDPOINT: &str = "https://driven.maxhogan.dev/telemetry/v1/ping";

// ---------------------------------------------------------------------------
// Settings group shape (minimal, local)
// ---------------------------------------------------------------------------

/// The minimal `telemetry` settings group this module reads/writes. Mirrors the
/// on-disk `snake_case` form the M6 settings layer persists (settings.rs
/// `storage::Telemetry`); kept local so this module does not depend on the
/// settings command internals. Extra fields are preserved on a round-trip
/// because we read the raw JSON object and only mutate the keys we own.
struct TelemetryPrefs {
    /// Whether anonymous usage stats are sent (DEFAULT ON, SPEC s16).
    enabled: bool,
    /// The stable anonymous install id (UUID v4). Empty until first ensured.
    install_id: String,
    /// The ingest endpoint URL.
    endpoint: String,
}

/// Read the `telemetry` settings group, applying SPEC s16 defaults for any
/// absent field: `enabled = true` (default ON), `endpoint = DEFAULT_ENDPOINT`,
/// `install_id = ""` (ensured non-empty by [`ensure_install_id`] on startup).
async fn read_prefs(state: &dyn StateRepo) -> CommandResult<TelemetryPrefs> {
    let value = state
        .get_setting(KEY_TELEMETRY)
        .await
        .map_err(CommandError::from)?;
    let obj = value.as_ref().and_then(|v| v.as_object());
    let enabled = obj
        .and_then(|m| m.get("enabled"))
        .and_then(|v| v.as_bool())
        // DEFAULT ON: a missing / malformed flag means enabled (SPEC s16).
        .unwrap_or(true);
    let install_id = obj
        .and_then(|m| m.get("install_id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let endpoint = obj
        .and_then(|m| m.get("endpoint"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_ENDPOINT)
        .to_string();
    Ok(TelemetryPrefs {
        enabled,
        install_id,
        endpoint,
    })
}

/// Persist the `telemetry.enabled` flag, PRESERVING the sibling `install_id` /
/// `endpoint` fields (read-modify-write the raw object so a toggle never wipes
/// the stable install id).
async fn write_enabled(state: &dyn StateRepo, enabled: bool) -> CommandResult<()> {
    let mut group = match state
        .get_setting(KEY_TELEMETRY)
        .await
        .map_err(CommandError::from)?
    {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    group.insert("enabled".to_string(), serde_json::Value::Bool(enabled));
    // Keep the document complete + well-typed even if it was never seeded.
    group
        .entry("install_id".to_string())
        .or_insert_with(|| serde_json::Value::String(String::new()));
    group
        .entry("endpoint".to_string())
        .or_insert_with(|| serde_json::Value::String(DEFAULT_ENDPOINT.to_string()));
    state
        .set_setting(KEY_TELEMETRY, &serde_json::Value::Object(group))
        .await
        .map_err(CommandError::from)
}

/// Ensure the stable anonymous `install_id` exists (SPEC s16: a UUID v4 minted on
/// FIRST run, stable across restarts). The migration 0002 seed writes a random id,
/// but a DB that predates the seed (or a cleared field) must still get one; this
/// mints a UUID v4 and persists it (preserving siblings) when the stored id is
/// empty. Returns the (now non-empty) install id. Idempotent.
async fn ensure_install_id(state: &dyn StateRepo) -> CommandResult<String> {
    let prefs = read_prefs(state).await?;
    if !prefs.install_id.is_empty() {
        return Ok(prefs.install_id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    let mut group = match state
        .get_setting(KEY_TELEMETRY)
        .await
        .map_err(CommandError::from)?
    {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    group.insert(
        "install_id".to_string(),
        serde_json::Value::String(id.clone()),
    );
    group
        .entry("enabled".to_string())
        .or_insert(serde_json::Value::Bool(true));
    group
        .entry("endpoint".to_string())
        .or_insert_with(|| serde_json::Value::String(DEFAULT_ENDPOINT.to_string()));
    state
        .set_setting(KEY_TELEMETRY, &serde_json::Value::Object(group))
        .await
        .map_err(CommandError::from)?;
    Ok(id)
}

/// Read the active updater channel string (`stable` | `dev`) for the payload.
/// A missing / malformed value reports `stable` (the safe default).
async fn read_channel(state: &dyn StateRepo) -> CommandResult<String> {
    let value = state
        .get_setting(KEY_UPDATER)
        .await
        .map_err(CommandError::from)?;
    let channel = value
        .as_ref()
        .and_then(|v| v.get("channel"))
        .and_then(|c| c.as_str())
        .filter(|s| *s == "stable" || *s == "dev")
        .unwrap_or("stable")
        .to_string();
    Ok(channel)
}

// ---------------------------------------------------------------------------
// The wire payload (SPEC s16) - PRIVACY: counts/sizes/codes/latencies ONLY
// ---------------------------------------------------------------------------

/// The 24h event aggregates carried in the ping (SPEC s16 `events_24h`).
///
/// PRIVACY: every field is a count/size or an error CODE map - no path/name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Events24h {
    /// Files uploaded in the window.
    pub files_uploaded: u64,
    /// Bytes uploaded in the window.
    pub bytes_uploaded: u64,
    /// Error counts keyed by SPEC s24 error code (e.g. `drive.rate_limited`).
    /// A `BTreeMap` so the JSON is deterministic (sorted keys) for snapshotting.
    pub errors_by_class: std::collections::BTreeMap<String, u64>,
    /// Deep-verify passes completed in the window.
    pub deep_verify_runs: u64,
    /// In-app updates applied in the window.
    pub update_applied: u64,
}

/// The scan / upload-per-MB latency percentiles carried in the ping (SPEC s16
/// `latency_p50_p95_ms`).
///
/// Each is a `[p50, p95]` pair in milliseconds. V1 does NOT record per-op
/// durations in the durable state, so there is no honest percentile source yet;
/// these are emitted as EMPTY arrays rather than fabricated values (the wire
/// shape still carries the keys). See CODEX_NOTES M9b: real latency capture is a
/// later, instrumentation-bearing change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyP50P95 {
    /// `[p50, p95]` scan latency in ms (empty until per-scan timing is recorded).
    pub scan: Vec<u64>,
    /// `[p50, p95]` upload-per-MB latency in ms (empty until recorded).
    pub upload_per_mb: Vec<u64>,
}

/// The full anonymous telemetry ping payload (SPEC s16). Serialized as JSON and
/// POSTed to the ingest endpoint.
///
/// PRIVACY: `install_id` is a random anonymous UUID; everything else is a
/// platform descriptor, the channel, or a count/size/code/latency. NO file
/// names, paths, or contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryPayload {
    /// The stable anonymous install id (UUID v4).
    pub install_id: String,
    /// Wall-clock send time (Unix epoch ms).
    pub ts: i64,
    /// The app version (`CARGO_PKG_VERSION` / package info).
    pub version: String,
    /// The OS family (`windows` | `macos` | `linux` | ...), from
    /// `std::env::consts::OS`.
    pub os: String,
    /// A coarse OS version string, or `None` when not determinable without a
    /// host probe (V1 does not depend on an OS-version crate; see CODEX_NOTES).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    /// The CPU arch (`x86_64` | `aarch64` | ...), from `std::env::consts::ARCH`.
    pub arch: String,
    /// The active updater channel (`stable` | `dev`).
    pub channel: String,
    /// The 24h event aggregates.
    pub events_24h: Events24h,
    /// The scan / upload-per-MB latency percentiles.
    pub latency_p50_p95_ms: LatencyP50P95,
}

/// Build the SPEC s16 payload from the resolved parts (PURE - no network, no
/// clock, no settings read). Split out so the unit tests assert the exact shape
/// + privacy invariants without an `AppHandle` or a live endpoint.
#[must_use]
fn build_payload(
    install_id: String,
    ts: i64,
    version: String,
    channel: String,
    aggregate: driven_core::state::TelemetryAggregate,
) -> TelemetryPayload {
    let errors_by_class = aggregate.errors_by_class.into_iter().collect();
    TelemetryPayload {
        install_id,
        ts,
        version,
        os: std::env::consts::OS.to_string(),
        // V1: no OS-version crate dependency, so this is honestly absent rather
        // than guessed (privacy + dependency-minimalism, see CODEX_NOTES M9b).
        os_version: None,
        arch: std::env::consts::ARCH.to_string(),
        channel,
        events_24h: Events24h {
            files_uploaded: aggregate.files_uploaded,
            bytes_uploaded: aggregate.bytes_uploaded,
            errors_by_class,
            deep_verify_runs: aggregate.deep_verify_runs,
            update_applied: aggregate.update_applied,
        },
        // No per-op latency is recorded in durable state in V1; emit empty
        // arrays (the keys are present) rather than fabricating percentiles.
        latency_p50_p95_ms: LatencyP50P95::default(),
    }
}

// ---------------------------------------------------------------------------
// The send seam (the unit-test boundary - no live endpoint in tests)
// ---------------------------------------------------------------------------

/// The telemetry transport seam (SPEC s16 test requirement: "use a seam/trait so
/// it is offline-testable - do NOT hit the live endpoint"). Production uses
/// [`HttpTelemetrySink`]; tests use an in-memory recorder.
#[async_trait]
pub trait TelemetrySink: Send + Sync {
    /// POST `payload` to `endpoint`. BEST-EFFORT: the implementation must apply a
    /// bounded timeout and return an error rather than hang; the caller logs +
    /// swallows the error (a send failure never affects backups).
    async fn send(&self, endpoint: &str, payload: &TelemetryPayload) -> anyhow::Result<()>;
}

/// The production telemetry sink: a single bounded-timeout `reqwest` POST of the
/// JSON payload. Best-effort - a non-2xx / network / timeout error is returned
/// (the caller logs + swallows it). Builds its own client (the workspace
/// `reqwest` is rustls-only; no `json` feature, so the body is serialized
/// manually like the GitHub-releases client in settings.rs).
pub struct HttpTelemetrySink;

#[async_trait]
impl TelemetrySink for HttpTelemetrySink {
    async fn send(&self, endpoint: &str, payload: &TelemetryPayload) -> anyhow::Result<()> {
        let body = serde_json::to_vec(payload)?;
        let client = reqwest::Client::builder()
            .user_agent(concat!("driven-app/", env!("CARGO_PKG_VERSION")))
            .timeout(SEND_TIMEOUT)
            .build()?;
        let resp = client
            .post(endpoint)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "telemetry endpoint returned HTTP {}",
                resp.status().as_u16()
            ));
        }
        Ok(())
    }
}

/// Gather + send ONE telemetry ping IF enabled (SPEC s16). Reads the CURRENT
/// pref each call (so a toggle is honored immediately): when DISABLED it makes NO
/// network call and returns early. When enabled it ensures the install id,
/// aggregates the last 24h from the durable state, builds the payload, and sends
/// it best-effort through `sink`. Returns `true` if a send was attempted, `false`
/// if telemetry was disabled (so tests can assert the no-network path).
async fn maybe_send_once(
    state: &dyn StateRepo,
    version: String,
    now_ms: i64,
    sink: &dyn TelemetrySink,
) -> bool {
    let prefs = match read_prefs(state).await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(target: TARGET, error = %e, "telemetry: could not read prefs; skipping ping");
            return false;
        }
    };
    // DISABLED -> NO network at all (SPEC s16). Honored immediately each tick.
    if !prefs.enabled {
        tracing::debug!(target: TARGET, "telemetry disabled; no ping sent");
        return false;
    }
    let install_id = match ensure_install_id(state).await {
        Ok(id) => id,
        Err(e) => {
            tracing::debug!(target: TARGET, error = %e, "telemetry: could not ensure install_id; skipping ping");
            return false;
        }
    };
    let channel = read_channel(state).await.unwrap_or_else(|_| "stable".to_string());
    let since_ms = now_ms.saturating_sub(WINDOW.as_millis() as i64);
    let aggregate = match state.telemetry_events_24h(since_ms).await {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(target: TARGET, error = %e, "telemetry: could not aggregate events_24h; skipping ping");
            return false;
        }
    };
    let payload = build_payload(install_id, now_ms, version, channel, aggregate);
    // BEST-EFFORT: log + swallow any send failure. NEVER affects backups.
    match sink.send(&prefs.endpoint, &payload).await {
        Ok(()) => {
            tracing::debug!(target: TARGET, files = payload.events_24h.files_uploaded, "telemetry ping sent");
        }
        Err(e) => {
            tracing::info!(target: TARGET, error = %e, "telemetry ping failed (best-effort, ignored)");
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Periodic ping task (SPEC s16 - startup + every 24h, no orphan)
// ---------------------------------------------------------------------------

/// Spawn the periodic telemetry-ping task (SPEC s16): an immediate ping on
/// startup, then one every [`PING_INTERVAL`] (24h) via a tokio `interval`. The
/// task `select!`s on a shutdown watch so an explicit Quit stops it promptly, and
/// its handle + shutdown sender are registered on [`AppState`] so the quit drain
/// joins it with NO orphan (mirrors the M9a updater task).
///
/// The task self-checks the `telemetry.enabled` pref every tick ([`maybe_send_once`]),
/// so it can be spawned unconditionally - when disabled it makes no network call.
pub fn spawn_periodic_ping(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        tracing::warn!(target: TARGET, "AppState not managed; telemetry ping not started");
        return;
    };
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let app_handle = app.clone();

    // `tokio::spawn` (not `tauri::async_runtime::spawn`) so the returned handle is
    // a `tokio::task::JoinHandle`, matching the no-orphan drain in lib.rs. Spawned
    // from inside the setup `block_on`, so a reactor is active.
    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PING_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let sink = HttpTelemetrySink;
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
                    ping_once(&app_handle, &sink).await;
                }
            }
        }
        tracing::debug!(target: TARGET, "telemetry ping task exited");
    });

    state.set_telemetry_task(task, shutdown_tx);
    tracing::info!(target: TARGET, interval_secs = PING_INTERVAL.as_secs(), "telemetry ping task started");
}

/// One periodic-ping iteration: resolve the version + now, then delegate to
/// [`maybe_send_once`] (which self-checks the enabled pref). All failures are
/// logged inside, never propagated (the loop must survive a transient error).
async fn ping_once(app: &AppHandle, sink: &dyn TelemetrySink) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let version = app.package_info().version.to_string();
    let now_ms = driven_core::time::SystemClock.now_ms();
    let _ = maybe_send_once(state.state().as_ref(), version, now_ms, sink).await;
}

// ---------------------------------------------------------------------------
// IPC commands (SPEC s16)
// ---------------------------------------------------------------------------

/// `get_telemetry_enabled()` - whether anonymous usage stats are sent (SPEC s16).
/// DEFAULT ON: a fresh / malformed pref reports `true`.
#[tauri::command]
pub async fn get_telemetry_enabled(state: State<'_, AppState>) -> CommandResult<bool> {
    Ok(read_prefs(state.state().as_ref()).await?.enabled)
}

/// `set_telemetry_enabled(enabled)` - toggle anonymous usage stats (SPEC s16).
/// Persists the flag (preserving the stable `install_id`), honored immediately by
/// the next tick (and, when turning OFF, the in-flight loop makes no further
/// network call). Returns the stored value.
#[tauri::command]
pub async fn set_telemetry_enabled(
    state: State<'_, AppState>,
    enabled: bool,
) -> CommandResult<bool> {
    write_enabled(state.state().as_ref(), enabled).await?;
    tracing::info!(target: TARGET, enabled, "telemetry enabled toggled");
    Ok(enabled)
}

/// `get_telemetry_install_id()` - the stable anonymous install id (SPEC s16),
/// ensuring one exists (mints a UUID v4 on first read if absent). Exposed so the
/// About / privacy surface can show the user their anonymous id.
#[tauri::command]
pub async fn get_telemetry_install_id(state: State<'_, AppState>) -> CommandResult<String> {
    ensure_install_id(state.state().as_ref()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_core::state::sqlite::SqliteStateRepo;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A temp-backed state repo (migrations run on open) for the prefs / send
    /// tests. No real Drive / keychain / network touched.
    async fn temp_repo() -> (SqliteStateRepo, std::path::PathBuf) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("driven-telemetry-test-{nonce}-{:p}", &nonce));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let repo = SqliteStateRepo::open(&dir.join("state.db"))
            .await
            .expect("open state repo");
        (repo, dir)
    }

    fn cleanup(dir: std::path::PathBuf) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An in-memory [`TelemetrySink`] that RECORDS every payload it is asked to
    /// send, so tests assert the send path WITHOUT touching the network. Its
    /// `send` always succeeds.
    #[derive(Default)]
    struct RecordingSink {
        sent: Mutex<Vec<TelemetryPayload>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl TelemetrySink for RecordingSink {
        async fn send(&self, _endpoint: &str, payload: &TelemetryPayload) -> anyhow::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.sent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(payload.clone());
            Ok(())
        }
    }

    /// A [`TelemetrySink`] that always FAILS, to prove a send error is swallowed
    /// (non-fatal) and the call path still reports "attempted".
    struct FailingSink;

    #[async_trait]
    impl TelemetrySink for FailingSink {
        async fn send(&self, _endpoint: &str, _payload: &TelemetryPayload) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("simulated network failure"))
        }
    }

    #[test]
    fn payload_has_the_spec_s16_shape_and_carries_no_paths() {
        // SPEC s16 + PRIVACY: the payload carries install_id, ts, version, os,
        // arch, channel, the events_24h aggregates, and the latency pairs - and
        // NOTHING path/name-shaped. Build it from a known aggregate and assert the
        // serialized JSON has exactly the expected keys + no message/path field.
        let mut errors = BTreeMap::new();
        errors.insert("drive.rate_limited".to_string(), 3u64);
        let aggregate = driven_core::state::TelemetryAggregate {
            files_uploaded: 7,
            bytes_uploaded: 1234,
            errors_by_class: vec![("drive.rate_limited".to_string(), 3)],
            deep_verify_runs: 2,
            update_applied: 1,
        };
        let p = build_payload(
            "00000000-0000-4000-8000-000000000000".to_string(),
            1_700_000_000_000,
            "0.1.0".to_string(),
            "dev".to_string(),
            aggregate,
        );
        assert_eq!(p.install_id, "00000000-0000-4000-8000-000000000000");
        assert_eq!(p.version, "0.1.0");
        assert_eq!(p.channel, "dev");
        assert_eq!(p.os, std::env::consts::OS);
        assert_eq!(p.arch, std::env::consts::ARCH);
        assert_eq!(p.events_24h.files_uploaded, 7);
        assert_eq!(p.events_24h.bytes_uploaded, 1234);
        assert_eq!(p.events_24h.errors_by_class, errors);
        assert_eq!(p.events_24h.deep_verify_runs, 2);
        assert_eq!(p.events_24h.update_applied, 1);
        // Latency pairs are present but empty in V1 (no fabricated percentiles).
        assert!(p.latency_p50_p95_ms.scan.is_empty());
        assert!(p.latency_p50_p95_ms.upload_per_mb.is_empty());

        // PRIVACY: the serialized JSON must not carry any path/name/message-shaped
        // key, and the known top-level keys are exactly the SPEC s16 set.
        let json = serde_json::to_value(&p).unwrap();
        let obj = json.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "arch",
                "channel",
                "events_24h",
                "install_id",
                "latency_p50_p95_ms",
                "os",
                "ts",
                "version",
            ],
            "os_version is skipped when None; no path/name/message key present"
        );
        let s = serde_json::to_string(&p).unwrap();
        assert!(!s.contains("path"), "no path field in the payload: {s}");
        assert!(!s.contains("message"), "no message field: {s}");
        assert!(!s.contains("file.txt"), "no file name leaked: {s}");
    }

    #[tokio::test]
    async fn disabled_telemetry_sends_no_ping() {
        // SPEC s16: when telemetry is DISABLED the send path makes NO network call
        // (the recording sink must record zero sends).
        let (repo, dir) = temp_repo().await;
        write_enabled(&repo, false).await.unwrap();

        let sink = RecordingSink::default();
        let attempted =
            maybe_send_once(&repo, "0.1.0".to_string(), 1_700_000_000_000, &sink).await;

        assert!(!attempted, "disabled telemetry must not attempt a send");
        assert_eq!(
            sink.calls.load(Ordering::SeqCst),
            0,
            "no network call when disabled"
        );
        cleanup(dir);
    }

    #[tokio::test]
    async fn enabled_telemetry_sends_a_well_formed_ping() {
        // SPEC s16: with telemetry enabled (the default) a ping is sent through the
        // sink carrying the ensured install_id + the aggregated events.
        let (repo, dir) = temp_repo().await;
        // Default seed has enabled=true + a generated install_id.
        let sink = RecordingSink::default();
        let attempted =
            maybe_send_once(&repo, "0.1.0".to_string(), 1_700_000_000_000, &sink).await;

        assert!(attempted, "enabled telemetry must attempt a send");
        let sent = sink.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(
            !sent[0].install_id.is_empty(),
            "ping carries a non-empty install_id"
        );
        assert_eq!(sent[0].version, "0.1.0");
        cleanup(dir);
    }

    #[tokio::test]
    async fn send_error_is_swallowed_and_non_fatal() {
        // SPEC s16: a POST failure must be swallowed (best-effort) - the call path
        // still reports "attempted" (it did not panic / propagate).
        let (repo, dir) = temp_repo().await;
        let sink = FailingSink;
        let attempted =
            maybe_send_once(&repo, "0.1.0".to_string(), 1_700_000_000_000, &sink).await;
        assert!(
            attempted,
            "a failed send is still an attempt (error swallowed, not fatal)"
        );
        cleanup(dir);
    }

    #[tokio::test]
    async fn install_id_persists_across_reload() {
        // SPEC s16: the install_id is stable across restarts. Ensure it once, then
        // re-open the same DB and confirm the same id is read back (not re-minted).
        let (repo, dir) = temp_repo().await;
        let id1 = ensure_install_id(&repo).await.unwrap();
        assert!(!id1.is_empty());
        // Ensuring again returns the SAME id (idempotent).
        let id1b = ensure_install_id(&repo).await.unwrap();
        assert_eq!(id1, id1b, "ensure is idempotent within a session");
        drop(repo);

        // Re-open the same on-disk DB: the id survives the reload.
        let repo2 = SqliteStateRepo::open(&dir.join("state.db"))
            .await
            .expect("re-open state repo");
        let id2 = ensure_install_id(&repo2).await.unwrap();
        assert_eq!(id1, id2, "install_id persists across a reload");
        cleanup(dir);
    }

    #[tokio::test]
    async fn toggle_preserves_install_id() {
        // Toggling enabled OFF then ON must NOT wipe the stable install id.
        let (repo, dir) = temp_repo().await;
        let id = ensure_install_id(&repo).await.unwrap();
        write_enabled(&repo, false).await.unwrap();
        assert!(!read_prefs(&repo).await.unwrap().enabled);
        write_enabled(&repo, true).await.unwrap();
        let prefs = read_prefs(&repo).await.unwrap();
        assert!(prefs.enabled);
        assert_eq!(prefs.install_id, id, "toggle preserves the install_id");
        cleanup(dir);
    }

    #[tokio::test]
    async fn default_pref_is_enabled() {
        // SPEC s16: default ON. The migration 0002 seed writes enabled=true; even
        // with NO stored group, read_prefs defaults to enabled.
        let (repo, dir) = temp_repo().await;
        assert!(
            read_prefs(&repo).await.unwrap().enabled,
            "telemetry is default ON"
        );
        cleanup(dir);
    }
}
