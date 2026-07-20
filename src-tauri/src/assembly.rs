//! App-shell assembly: wire the real seams, build one orchestrator per
//! account, and spawn each run loop (SPEC s5, DESIGN s5.1).
//!
//! This is where the abstract `driven-core` seams meet their PRODUCTION
//! implementations. For every account in the state DB, [`build_and_spawn`]:
//! 1. constructs the [`RemoteStore`](driven_drive::remote_store::RemoteStore) -
//!    a real `GoogleDriveStore` from the keyring refresh token via
//!    `RefreshingTokenSource` (the `driven-cli` `build_store` pattern), OR an
//!    `InMemoryRemoteStore` when `DRIVEN_USE_FAKE_REMOTE=1` (dev / e2e) or the
//!    account has not authenticated yet;
//! 2. builds the real `Pacer`, `RealPowerSource`, a `ReqwestBackend`-backed
//!    `NetworkProbe` (`Some`, so the Drive breaker is driven by REAL request
//!    outcomes - CODEX_NOTES P2-9 / V-G), the Windows `VssProvider` (M3.5), and
//!    a [`KeystoreCryptoProvider`](crate::crypto_provider_impl::KeystoreCryptoProvider)
//!    (per-source crypto, GA blocker);
//! 3. assembles [`ExecutorDeps`] -> `DefaultExecutor`, builds the
//!    [`SyncOrchestrator`] (`.with_vss(..)` on Windows), spawns
//!    [`Orchestrator::run`], and bridges the watcher + the orchestrator event
//!    stream;
//! 4. collects the per-account handles into an [`AppState`].
//!
//! Robustness: one account failing to build (a broken keychain entry, an
//! un-spawnable watcher) is logged and SKIPPED - it must never abort the other
//! accounts' sync.

use std::collections::HashMap;
use std::sync::Arc;

use tauri::AppHandle;

use driven_core::executor::{DefaultExecutor, Executor, ExecutorDeps};
use driven_core::network::{NetworkProbe, Prober};
use driven_core::orchestrator::{Orchestrator, OrchestratorConfig, SyncOrchestrator};
use driven_core::pacer::{AimdPacer, Pacer};
use driven_core::state::{AccountRow, SourceRow, StateRepo};
use driven_core::time::{Clock, SystemClock};
use driven_core::types::{AccountId, AccountState, OrchestratorEvent};
use driven_core::watcher::{NotifyWatcher, SourceWatcher};

use driven_drive::google::token_store::{KeyringTokenStore, RefreshingTokenSource};
use driven_drive::google::GoogleDriveStore;
use driven_drive::remote_store::RemoteStore;

use driven_net::ReqwestBackend;
use driven_tls::CustomCaConfig;

use driven_power::RealPowerSource;

// The brokered provider + launcher trait are only referenced in the Windows
// `build_vss` arm; off Windows they would be unused imports (clippy -D warnings).
#[cfg(windows)]
use driven_vss_helper::{BrokeredVssProvider, HelperLauncher};

use crate::app_state::{
    fake_remote_store_in, AccountHandle, AccountTasks, AppState, FakeRemoteStores, RemoteMode,
};
use crate::crypto_provider_impl::KeystoreCryptoProvider;
use crate::vss_helper::VssHelperManager;
use crate::{events, tray};

/// Tracing target for the app-shell assembly.
const TARGET: &str = "driven::app::assembly";

/// Environment flag selecting the in-memory fake remote (dev / e2e) instead of
/// a real `GoogleDriveStore`. Mirrors the assembly contract in the task spec.
pub const ENV_USE_FAKE_REMOTE: &str = "DRIVEN_USE_FAKE_REMOTE";

/// R2-P2-1 (BYO-only, SPEC s11.1 / DESIGN s6.1): the OAuth client id env
/// override. Driven is BYO-ONLY - there is NO baked-in default client. This env
/// var exists ONLY as a TEST injection seam (real-Drive e2e); a production
/// account refreshes against its PERSISTED BYO client creds.
const ENV_OAUTH_CLIENT_ID: &str = "DRIVEN_OAUTH_CLIENT_ID";

/// R2-P2-1: the OAuth client secret env override (test injection seam only). An
/// installed-app PKCE client has no real secret, so this defaults to empty.
const ENV_OAUTH_CLIENT_SECRET: &str = "DRIVEN_OAUTH_CLIENT_SECRET";

/// Build every account's orchestrator over the real seams and spawn its run
/// loop, returning the [`AppState`] for `.manage(..)` (SPEC s5).
///
/// `app` is the Tauri handle (for the orchestrator-event -> tray/IPC bridge);
/// `state` is the already-migrated [`StateRepo`] from [`crate::migrations::run`].
pub async fn build_and_spawn(
    app: &AppHandle,
    state: Arc<dyn StateRepo>,
) -> anyhow::Result<AppState> {
    let use_fake = use_fake_remote();
    // The recorded global mode reflects INTENT: `Fake` iff the env forces it.
    // A real-mode account with no token yet still falls back to the in-memory
    // store (logged per-account) but does not flip the global verdict.
    let remote_mode = if use_fake {
        RemoteMode::Fake
    } else {
        RemoteMode::RealGoogleDrive
    };

    let accounts = state.list_accounts().await?;
    let all_sources = state.list_sources().await?;

    // Issue #25 (DESIGN s5.3.1): build the least-privilege VSS helper broker
    // manager ONCE (Windows + un-elevated + `windows.vss_helper` on). The SAME
    // `Arc` is handed to every account's BrokeredVssProvider below, so there is a
    // single launch / UAC prompt / pipe across all accounts. `None` off Windows,
    // when elevated, or when the setting is off.
    let helper_enabled = crate::commands::settings::load_vss_helper_enabled(state.as_ref()).await;
    let vss_helper = build_vss_helper_manager(&all_sources, helper_enabled);

    tracing::info!(
        target: TARGET,
        accounts = accounts.len(),
        sources = all_sources.len(),
        fake_remote = use_fake,
        vss_helper = vss_helper.is_some(),
        "assembling per-account orchestrators"
    );

    // R2-P1-2: the shared per-account fake-remote-store registry. Built HERE
    // (before the account loop) and threaded into `build_account` so the
    // orchestrator's fake store comes from it; the SAME registry is then moved
    // into `AppState`, so the Drive-folder picker reads the SAME instance per
    // account. A picker-minted folder id is therefore visible to the uploader.
    let fake_remote_stores: FakeRemoteStores = Arc::new(std::sync::Mutex::new(HashMap::new()));

    // DESIGN s13 telemetry: the app-global latency reservoir, created ONCE with
    // its capture gate initialized from the persisted `telemetry.enabled` pref
    // (BEFORE any executor / scanner is wired, so an opted-out user gets no
    // startup capture window). The SAME `Arc` is threaded into every account's
    // executor + orchestrator below and installed on the AppState so the ping
    // task reads the samples both write.
    let telemetry_enabled = crate::telemetry::read_enabled(state.as_ref()).await;
    let latency = Arc::new(driven_core::telemetry::LatencyReservoir::new(
        telemetry_enabled,
    ));

    let mut handles: HashMap<AccountId, AccountHandle> = HashMap::new();

    for account in &accounts {
        // C5-P1-2: only spawn an orchestrator for an `AccountState::Ok` account.
        // A `NeedsReauth` account is waiting on re-consent (it must issue ZERO
        // remote calls until the M6 reauth flow rebuilds it); a `Disabled`
        // account was explicitly turned off by the user. Spawning either would
        // tick a dead / unwanted credential.
        if account.state != AccountState::Ok {
            tracing::info!(
                target: TARGET,
                account_id = %account.id,
                email = %account.email,
                state = ?account.state,
                "account is not Ok; not spawning its orchestrator"
            );
            // Surface a needs-reauth account to the webview so the re-consent
            // banner shows on boot (the orchestrator that would normally emit
            // this is never spawned for a non-Ok account).
            if account.state == AccountState::NeedsReauth {
                emit_needs_reauth(app, account);
            }
            continue;
        }

        // Every account's sources (the watcher + crypto provider need the full
        // set, including disabled ones, so a later enable does not require a
        // rebuild of the crypto cache key space).
        let sources: Vec<SourceRow> = all_sources
            .iter()
            .filter(|s| s.account_id == account.id)
            .cloned()
            .collect();

        match build_account(
            app,
            &state,
            account,
            sources,
            use_fake,
            &fake_remote_stores,
            vss_helper.as_ref(),
            &latency,
        )
        .await
        {
            Ok(BuildOutcome::Spawned(handle)) => {
                handles.insert(account.id, *handle);
                tracing::info!(
                    target: TARGET,
                    account_id = %account.id,
                    email = %account.email,
                    "orchestrator assembled + spawned"
                );
            }
            Ok(BuildOutcome::NeedsReauth) => {
                // C5-P1-1: real mode, no/invalid stored token. We did NOT build a
                // fake remote (that would mark files synced against fake Drive
                // ids and silently lose the bytes on exit). The account was moved
                // to NeedsReauth + the banner emitted; do not spawn its loop.
                tracing::warn!(
                    target: TARGET,
                    account_id = %account.id,
                    email = %account.email,
                    "real remote has no valid token; account marked needs_reauth, orchestrator NOT spawned"
                );
            }
            Err(err) => {
                // One account failing must NOT abort the others (task spec).
                tracing::error!(
                    target: TARGET,
                    account_id = %account.id,
                    email = %account.email,
                    %err,
                    "failed to assemble account; skipping (other accounts continue)"
                );
            }
        }
    }

    let mut app_state = AppState::new(state, handles, remote_mode, fake_remote_stores);
    // DESIGN s13: share the SAME reservoir the executors + orchestrators record
    // into with the AppState so the ping task snapshots those samples (replaces
    // the default-ON placeholder).
    app_state.install_telemetry_latency(latency);
    // Issue #25: install the broker manager so the quit sweep can shut it down
    // and `get_vss_helper_status` can report truthful liveness.
    if let Some(manager) = vss_helper {
        app_state.set_vss_helper_manager(manager);
    }
    Ok(app_state)
}

/// R8-P1-1 (DATA-SAFETY): the FAIL-CLOSED decision for the boot path. `true` iff
/// the one-time upgrade recovery-ack repair SUCCEEDED, so it is safe to spawn the
/// sync orchestrators. On a repair ERROR this returns `false` so the boot path
/// goes QUIESCED ([`build_quiesced`]) and no orchestrator (encrypted or otherwise)
/// is started until the repair succeeds on a later boot. Pure + unit-tested so the
/// fail-closed branch is asserted without a live Tauri runtime / `AppHandle`.
#[must_use]
pub fn repair_allows_spawn(repair: &anyhow::Result<usize>) -> bool {
    repair.is_ok()
}

/// R8-P1-1 (DATA-SAFETY): build a QUIESCED [`AppState`] - one that manages the
/// state repo but spawns NO per-account orchestrators - for the FAIL-CLOSED
/// startup path. When the one-time upgrade recovery-ack repair
/// ([`StateRepo::repair_unacked_encrypted_sources_on_upgrade`]) FAILS, a pre-0004
/// encrypted source could still be `enabled` with no durable recovery ack; running
/// its orchestrator would keep producing encrypted backups for a phrase the user
/// may never have saved (unrestorable). So instead of [`build_and_spawn`], the
/// boot path builds THIS - no orchestrator runs, so nothing syncs - and surfaces a
/// startup error + tray note. The repair marker stays unset, so a later boot
/// retries and, on success, spawns normally.
///
/// Mirrors the no-orchestrator AppState shape `build_and_spawn` would return with
/// an empty handle set: the same `remote_mode` verdict and a fresh shared
/// fake-remote registry (unused while quiesced). The IPC layer still works (the
/// state repo is managed), so the user can reach Settings to reveal/ack a phrase.
#[must_use]
pub fn build_quiesced(state: Arc<dyn StateRepo>) -> AppState {
    let remote_mode = if use_fake_remote() {
        RemoteMode::Fake
    } else {
        RemoteMode::RealGoogleDrive
    };
    let fake_remote_stores: FakeRemoteStores = Arc::new(std::sync::Mutex::new(HashMap::new()));
    tracing::error!(
        target: TARGET,
        "R8-P1-1: starting QUIESCED (no orchestrators spawned) - the recovery-ack upgrade repair failed, so sync is held off until it succeeds on a later boot"
    );
    AppState::new(state, HashMap::new(), remote_mode, fake_remote_stores)
}

/// `true` when `DRIVEN_USE_FAKE_REMOTE=1` selects the in-memory fake remote.
fn use_fake_remote() -> bool {
    std::env::var(ENV_USE_FAKE_REMOTE)
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// A2: HOT-SPAWN one account's orchestrator into an already-running [`AppState`]
/// (the wizard's `finish_add_account` calls this so the freshly-added account
/// has a live orchestrator + handle without an app restart, and the wizard's
/// initial `sync_now(sourceId)` finds it).
///
/// Reads the account row + its sources from the (strongly-consistent) state DB,
/// builds the SAME per-account stack `build_and_spawn` builds at boot, and
/// INSERTS the resulting [`AccountHandle`] into the running set (shutting down
/// any prior handle for that id first, so no per-account task is orphaned -
/// mirrors the M5 no-orphan bookkeeping). Returns `Ok(true)` when an
/// orchestrator was spawned, `Ok(false)` when the account needs re-consent
/// (no/invalid token) so no orchestrator could be spawned.
///
/// An account in fake-remote mode (dev/e2e) builds the in-memory store, so the
/// wizard walkthrough completes end-to-end against the fake remote.
pub async fn spawn_account(
    app: &AppHandle,
    app_state: &AppState,
    account_id: AccountId,
) -> anyhow::Result<bool> {
    let state = app_state.state().clone();
    let use_fake = use_fake_remote();

    let accounts = state.list_accounts().await?;
    let account = accounts
        .into_iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| anyhow::anyhow!("spawn_account: unknown account id {account_id}"))?;

    // Only an Ok account spawns an orchestrator (mirrors build_and_spawn).
    if account.state != AccountState::Ok {
        tracing::info!(
            target: TARGET,
            account_id = %account.id,
            state = ?account.state,
            "spawn_account: account is not Ok; not spawning"
        );
        if account.state == AccountState::NeedsReauth {
            emit_needs_reauth(app, &account);
        }
        return Ok(false);
    }

    let all_sources = state.list_sources().await?;
    let sources: Vec<SourceRow> = all_sources
        .into_iter()
        .filter(|s| s.account_id == account.id)
        .collect();

    // R2-P1-2: hot-spawn builds the orchestrator store from the SAME shared
    // fake-remote registry the running AppState (and the picker) uses, so a
    // folder the wizard's picker minted in fake mode is the same instance the
    // new orchestrator uploads into.
    let fake_remote_stores = app_state.fake_remote_stores();

    // Issue #25: reuse the SAME broker manager the running AppState owns, so a
    // hot-added account's BrokeredVssProvider shares the one launch / pipe.
    let vss_helper = app_state.vss_helper_manager();

    // DESIGN s13: a hot-added account records into the SAME app-global latency
    // reservoir the running AppState + ping task already share.
    let latency = app_state.telemetry_latency();

    match build_account(
        app,
        &state,
        &account,
        sources,
        use_fake,
        &fake_remote_stores,
        vss_helper.as_ref(),
        &latency,
    )
    .await?
    {
        BuildOutcome::Spawned(handle) => {
            // Replace any prior handle for this id, shutting the old one down so
            // its tasks are not orphaned (defensive: a fresh add has none).
            if let Some(prior) = app_state.insert_account(account.id, *handle) {
                tracing::info!(
                    target: TARGET,
                    account_id = %account.id,
                    "spawn_account: replacing a prior handle; draining the old one"
                );
                prior.shutdown().await;
            }
            tracing::info!(
                target: TARGET,
                account_id = %account.id,
                "spawn_account: orchestrator assembled + spawned (hot)"
            );
            Ok(true)
        }
        BuildOutcome::NeedsReauth => {
            tracing::warn!(
                target: TARGET,
                account_id = %account.id,
                "spawn_account: no valid token; account needs reauth, orchestrator NOT spawned"
            );
            Ok(false)
        }
    }
}

/// The result of attempting to build + spawn one account's orchestrator.
enum BuildOutcome {
    /// The orchestrator was assembled and its run loop spawned. Boxed because
    /// [`AccountHandle`] now tracks four per-account task handles + a shutdown
    /// sender (R-P1-1), so the inline variant would dwarf the unit `NeedsReauth`
    /// arm (clippy `large_enum_variant`).
    Spawned(Box<AccountHandle>),
    /// C5-P1-1: real remote mode with no/invalid stored token. The account was
    /// moved to [`AccountState::NeedsReauth`] and the banner emitted; NO fake
    /// fallback was constructed and NO orchestrator was spawned.
    NeedsReauth,
}

/// What [`build_remote`] resolved for one account.
enum RemoteOutcome {
    /// A live [`RemoteStore`] (real `GoogleDriveStore` or the in-memory fake).
    Store(Arc<dyn RemoteStore>),
    /// C5-P1-1: real mode, no/invalid token. Do NOT fall back to a fake remote
    /// (that silently loses bytes); the account needs re-consent.
    NeedsReauth,
}

/// Build + spawn ONE account's orchestrator over the real seams. Returns a
/// [`BuildOutcome`] (spawned, or needs-reauth) or an error that the caller
/// logs + skips.
#[allow(clippy::too_many_arguments)]
async fn build_account(
    app: &AppHandle,
    state: &Arc<dyn StateRepo>,
    account: &AccountRow,
    sources: Vec<SourceRow>,
    use_fake: bool,
    fake_remote_stores: &FakeRemoteStores,
    vss_helper: Option<&Arc<VssHelperManager>>,
    latency: &Arc<driven_core::telemetry::LatencyReservoir>,
) -> anyhow::Result<BuildOutcome> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    // Issue #34: resolve the user's custom root CA once for this account's
    // client builds (Drive metadata/stream, OAuth refresh, and the network
    // probe backend below). Best-effort read of the settings blob; the PEM is
    // opened - and fails closed - inside each client build. A change is picked up
    // when the account is next assembled (app restart / account re-add).
    let ca = crate::commands::settings::load_custom_ca_config(state.as_ref())
        .await
        .unwrap_or_default();
    // Issue #34: likewise resolve the proxy once (System = honour env proxies).
    // For PAC mode this fetches + compiles the PAC file (fail-closed); a resolve
    // failure means we cannot honour the configured proxy, so we surface it
    // rather than silently connecting direct.
    let proxy = crate::commands::settings::load_proxy_config(state.as_ref()).await?;

    // --- remote: real GoogleDriveStore or the in-memory fake -----------------
    let remote = match build_remote(account, use_fake, fake_remote_stores, &ca, &proxy)? {
        RemoteOutcome::Store(store) => store,
        RemoteOutcome::NeedsReauth => {
            // C5-P1-1: persist needs_reauth + emit the banner; do NOT build a
            // fake remote and do NOT spawn the orchestrator (silent-data-loss
            // guard for a backup tool).
            if let Err(err) = state
                .mark_account_state(account.id, AccountState::NeedsReauth)
                .await
            {
                tracing::error!(
                    target: TARGET,
                    account_id = %account.id,
                    %err,
                    "failed to persist needs_reauth account state"
                );
            }
            emit_needs_reauth(app, account);
            return Ok(BuildOutcome::NeedsReauth);
        }
    };

    // --- pacer: real AIMD pacer seeded from the account's config -------------
    // R1-P2-1: load the PERSISTED SPEC s22 settings (scan cadence, bandwidth
    // cap, metered/battery gates, VSS mode) so a cold start honours the user's
    // saved settings, not the hard defaults. Before this fix the orchestrator
    // always booted with `OrchestratorConfig::default()` and only picked up the
    // persisted values after a live settings edit. A read/parse failure falls
    // back to the conservative default (reconfigure-style best-effort).
    let config = crate::commands::settings::load_orchestrator_config(state.as_ref())
        .await
        .unwrap_or_default();
    let pacer: Arc<dyn Pacer> = Arc::new(AimdPacer::with_ceilings(
        clock.clone(),
        config.bandwidth_cap_mbps.map(f64::from),
        config.pacer_ceilings,
    ));

    // --- power: real OS power source + its background poller ------------------
    let power = Arc::new(RealPowerSource::new()?);
    // Spawn the 30s poller so `current()` / `subscribe()` reflect live OS power
    // transitions (battery<->AC, metered, reachability). The orchestrator's run
    // loop subscribes to this internally (DESIGN s5.7 gate re-evaluation).
    //
    // R-P1-1: the poller loops FOREVER (no natural end), so its handle is KEPT
    // and stored in `AccountHandle` to be aborted on quit. Dropping it (the old
    // bug) detached the task and orphaned it past shutdown.
    let power_poller = power.spawn_poller();
    // Start the OS sleep/wake EDGE monitor (DESIGN s5.10.1, issue #33): the
    // per-OS backend (Win32 suspend/resume callback / macOS IOKit CFRunLoop /
    // Linux logind DBus) broadcasts suspend/resume edges the orchestrator's run
    // loop consumes via `subscribe_sleep_wake` to run the s5.10.2 / s5.10.3
    // sequences at the edge instead of waiting for the 30 s poll. Best-effort:
    // a registration failure is logged and the app degrades to the poll (the
    // account still syncs). The returned monitor is stored in `AccountHandle`
    // and torn down on quit so no OS handle / thread / task is orphaned.
    let sleep_wake_monitor = match power.spawn_sleep_wake_monitor() {
        Ok(monitor) => Some(monitor),
        Err(err) => {
            tracing::warn!(
                target: TARGET,
                account_id = %account.id,
                %err,
                "OS sleep/wake monitor unavailable; relying on the 30s power poll"
            );
            None
        }
    };
    let power: Arc<dyn driven_power::PowerSource> = power;

    // --- network: real ReqwestBackend-backed prober (Some, V-G) --------------
    // Passing `Some(..)` into `ExecutorDeps.network` routes every Drive request
    // through the breaker-reporting decorator so the Drive circuit breaker is
    // driven by REAL request outcomes, not probes alone (CODEX_NOTES P2-9 / V-G).
    let backend = Arc::new(ReqwestBackend::new(ca.clone(), proxy.clone())?);
    let network: Arc<dyn NetworkProbe> = Arc::new(Prober::new(backend, clock.clone()));

    // --- VSS provider (Windows): SAME Arc into executor + orchestrator --------
    // Brokered through the least-privilege helper when `vss_helper` is present
    // (un-elevated + setting on); in-process RealVssProvider otherwise (issue #25).
    let vss = build_vss(&config, vss_helper);

    // --- crypto: per-source keystore resolver (FAIL CLOSED - GA blocker) -----
    // B2: keep the CONCRETE Arc so the AccountHandle can expose it for live
    // refresh on a source change (the executor holds the same Arc as
    // `dyn CryptoProvider`).
    let crypto = Arc::new(KeystoreCryptoProvider::new(account.id, sources.clone()));
    let crypto_dyn: Arc<dyn driven_core::crypto_provider::CryptoProvider> = crypto.clone();

    // --- adaptive upload parallelism (DESIGN s11.4.7) -----------------------
    // Build the resizable upload pool sized at the user's
    // `default_concurrent_uploads` setting (finally wired; `None` auto-picks
    // `min(available_parallelism*2, 16)`), plus the throughput probe. Both are
    // shared by `Arc` into the executor (which acquires + records bytes) AND, when
    // adaptive is enabled, into the controller (which resizes + drains) - the same
    // one-Arc-into-two-consumers pattern as the pacer + latency reservoir.
    let pool_start = config
        .default_concurrent_uploads
        .map(|n| n as usize)
        .unwrap_or_else(driven_core::adaptive::default_pool_size);
    let upload_pool = driven_core::adaptive::UploadPool::new(pool_start);
    let throughput = driven_core::adaptive::ThroughputProbe::new();
    // Capture what the controller needs BEFORE `config` / `clock` / `pacer` are
    // moved into the executor + orchestrator below. The disk-busy reader is bound
    // to the first source's root (only the Linux backend uses it to pick the
    // backing device; the Windows `_Total` / macOS aggregate backends ignore it);
    // an account with no sources yet falls back to `.` and simply reads Unknown.
    let adaptive_enabled = config.adaptive_parallelism_enabled;
    let disk_root = sources
        .first()
        .map(|s| std::path::PathBuf::from(&s.local_path))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let clock_for_adaptive = clock.clone();
    let pacer_for_adaptive = pacer.clone();

    // --- executor -----------------------------------------------------------
    // The upload pool is wired in BOTH modes: it carries the start size
    // (`default_concurrent_uploads`, or the auto default) that the executor's
    // per-file gate uses whether or not adaptation is on. The throughput probe,
    // by contrast, is wired ONLY when adaptive is enabled (F4): with the
    // kill-switch off nothing drains it, so feeding it would be pure hot-path
    // overhead - and `record_bytes` is then skipped entirely (the executor's
    // `throughput` stays `None`), so "disabled" truly means zero adaptive code on
    // the upload path.
    let mut exec = DefaultExecutor::with_clock(
        ExecutorDeps {
            remote,
            state: state.clone(),
            // Clone so the orchestrator can share the SAME pacer for the V2
            // metered throttle (`with_pacer` below) - a runtime cap change must
            // be seen by this executor's upload path.
            pacer: pacer.clone(),
            crypto: Some(crypto_dyn),
            vss: vss.clone(),
            network: Some(network.clone()),
        },
        clock.clone(),
    )
    // DESIGN s13: the SAME app-global reservoir the orchestrator's scans
    // record into, for the upload-per-MB latency metric.
    .with_latency_reservoir(latency.clone())
    // DESIGN s11.4.7: the SAME pool the controller resizes (fixed here when the
    // kill-switch is off, honouring `default_concurrent_uploads` either way).
    .with_upload_pool(upload_pool.clone());
    if adaptive_enabled {
        // DESIGN s11.4.7: the SAME probe the controller drains.
        exec = exec.with_throughput_probe(throughput.clone());
    }
    let executor: Arc<dyn Executor> = Arc::new(exec);

    // --- orchestrator -------------------------------------------------------
    // Held as the CONCRETE `Arc<SyncOrchestrator>` (not `Arc<dyn Orchestrator>`)
    // so the assembly can call its INHERENT bridging methods (`subscribe`,
    // `watcher_sender`) that the object-safe `Orchestrator` trait does not
    // expose. It coerces to `Arc<dyn Orchestrator>` for the stored handle.
    let mut orchestrator = SyncOrchestrator::new(
        account.id,
        state.clone(),
        executor,
        power,
        network,
        clock,
        config,
    );
    if let Some(vss) = vss {
        // The SAME provider Arc the executor's snapshot reads use, so the
        // orchestrator's per-cycle release + orphan cleanup share one provider
        // (DESIGN s5.3).
        orchestrator = orchestrator.with_vss(vss);
    }
    // Real pre/post backup hook runner (V2, DESIGN s17): without this the
    // orchestrator keeps the inert no-op runner and configured hooks never run.
    orchestrator =
        orchestrator.with_command_runner(Arc::new(crate::hook_runner::TokioCommandRunner));
    // Share the executor's pacer so the V2 metered throttle (DESIGN s17) can
    // lower / lift its bandwidth cap as the network goes on / off metered.
    orchestrator = orchestrator.with_pacer(pacer);
    // DESIGN s13: the SAME reservoir the executor holds, so per-file scan latency
    // and upload-per-MB latency feed one app-global sampler.
    orchestrator = orchestrator.with_latency_reservoir(latency.clone());
    // DESIGN s11.4.7: adaptive upload parallelism. Default-ON; when the kill-switch
    // (`adaptive_parallelism_enabled`) is off, no controller is wired and the pool
    // stays fixed at `default_concurrent_uploads`. The controller resizes the SAME
    // `upload_pool` the executor acquires from and drains the SAME `throughput`
    // probe it feeds, gated by the real per-OS disk-busy reader (DESIGN s18.2) and
    // the account's pacer (for the window-scoped "not throttling" check).
    if adaptive_enabled {
        let disk: Arc<dyn driven_diskstat::DiskBusyProbe> =
            Arc::new(driven_diskstat::RealDiskBusyProbe::new(disk_root));
        let controller = Arc::new(driven_core::adaptive::AdaptiveController::new(
            upload_pool.clone(),
            throughput.clone(),
            disk,
            pacer_for_adaptive,
            clock_for_adaptive,
        ));
        orchestrator = orchestrator.with_adaptive_controller(controller);
    }
    let orchestrator = Arc::new(orchestrator);

    // R-P1-1: one shutdown signal both bridges select! on, so quit can stop the
    // watcher bridge (whose `NotifyWatcher` owns the mpsc::Sender, so its
    // `recv().await` never closes on its own) and the event bridge promptly.
    let (bridge_shutdown, _bridge_rx0) = tokio::sync::watch::channel(false);

    // --- watcher bridge (DESIGN s5.9.1) -------------------------------------
    // The real `NotifyWatcher` emits debounced scan-ticks; forward them into the
    // orchestrator's watcher channel. A watcher that cannot be built / watched
    // is NON-FATAL: the scheduled scan is the authoritative fallback (DESIGN
    // s5.9.4), so the account still backs up, just without the latency win.
    // Returns `None` when no enabled source produced a watcher.
    let watcher_bridge = spawn_watcher_bridge(
        account.id,
        orchestrator.watcher_sender(),
        &sources,
        bridge_shutdown.subscribe(),
    );

    // --- event bridge: orchestrator broadcast -> tray + IPC events -----------
    let event_bridge = spawn_event_bridge(
        app,
        account.id,
        account.email.clone(),
        orchestrator.subscribe(),
        bridge_shutdown.subscribe(),
    );

    // --- spawn the run loop --------------------------------------------------
    // `tokio::spawn` (not `tauri::async_runtime::spawn`) so the returned handle
    // is the `tokio::task::JoinHandle<()>` `AccountHandle` stores (the Tauri
    // async runtime is tokio-backed, and `.setup()` runs us inside it). The
    // handle is held so a clean shutdown can abort the loop.
    let run_loop = {
        let orchestrator = orchestrator.clone();
        tokio::spawn(async move {
            if let Err(err) = orchestrator.run().await {
                tracing::error!(target: TARGET, %err, "orchestrator run loop exited with error");
            }
        })
    };

    // Coerce the concrete Arc into the trait object the handle + IPC use.
    let orchestrator: Arc<dyn Orchestrator> = orchestrator;
    Ok(BuildOutcome::Spawned(Box::new(AccountHandle::new(
        orchestrator,
        AccountTasks {
            crypto,
            run_loop,
            watcher_bridge,
            event_bridge,
            power_poller,
            sleep_wake_monitor,
            bridge_shutdown,
        },
    ))))
}

/// Construct the [`RemoteStore`] for one account.
///
/// - `DRIVEN_USE_FAKE_REMOTE=1`: the in-memory fake (dev / e2e ONLY).
/// - Real mode with a valid stored refresh token: a real `GoogleDriveStore`
///   (the `driven-cli` `build_store` pattern).
/// - Real mode with NO stored refresh token: [`RemoteOutcome::NeedsReauth`].
///
/// C5-P1-1 (silent-data-loss guard): in REAL mode a missing/invalid token does
/// NOT fall back to the in-memory fake. Doing so would let the orchestrator mark
/// files `synced` against EPHEMERAL fake Drive ids and then lose the actual
/// bytes on process exit - catastrophic for a backup tool. The ONLY way to get a
/// fake remote is the explicit `DRIVEN_USE_FAKE_REMOTE=1` opt-in.
fn build_remote(
    account: &AccountRow,
    use_fake: bool,
    fake_remote_stores: &FakeRemoteStores,
    ca: &CustomCaConfig,
    proxy: &driven_tls::ProxyConfig,
) -> anyhow::Result<RemoteOutcome> {
    if use_fake {
        tracing::info!(
            target: TARGET,
            account_id = %account.id,
            "remote: in-memory fake (DRIVEN_USE_FAKE_REMOTE=1)"
        );
        // R2-P1-2: pull the account's fake store from the SHARED registry (the
        // same one the picker reads via `AppState::fake_remote_store`), so a
        // folder id the picker minted is visible to this uploader.
        let store = fake_remote_store_in(fake_remote_stores, account.id);
        return Ok(RemoteOutcome::Store(Arc::new(store)));
    }

    // The keychain token store is keyed by account id (the same key the auth
    // flow stores under). Wrap it in an Arc so a refresh-token ROTATION is
    // persisted back to the keychain (codex C-P2-4 / V-A3).
    let token_store = Arc::new(KeyringTokenStore::new(account.id.to_string()));
    let refresh_token = match token_store.load_refresh_token() {
        Ok(Some(token)) => token,
        Ok(None) => {
            // C5-P1-1: NO fake fallback in real mode. The account needs
            // re-consent; the caller persists needs_reauth + emits the banner
            // and does NOT spawn the orchestrator (no fake-id silent data loss).
            tracing::warn!(
                target: TARGET,
                account_id = %account.id,
                "remote: no stored refresh token in real mode; needs reauth (NOT falling back to fake)"
            );
            return Ok(RemoteOutcome::NeedsReauth);
        }
        Err(err) => return Err(err),
    };

    // A1: prefer the account's persisted BYO client creds (the client that
    // minted this refresh token); fall back to env / public default only when
    // the account stored none (a default-client account). A refresh token is
    // bound to the client that minted it, so using the wrong client fails.
    let (client_id, client_secret) = resolve_account_oauth_creds(account.id);
    let token_source = RefreshingTokenSource::from_stored_refresh_token(
        refresh_token,
        client_id,
        client_secret,
        ca,
        proxy,
    )?
    .with_store(token_store);
    let store = GoogleDriveStore::with_default_clients(token_source, ca, proxy)?;
    tracing::info!(
        target: TARGET,
        account_id = %account.id,
        "remote: real GoogleDriveStore (keyring refresh token)"
    );
    Ok(RemoteOutcome::Store(Arc::new(store)))
}

/// Emit the `account:needs_reauth` webview banner + raise the OS notification
/// for an account that requires re-consent at assembly time (C5-P1-1 /
/// C5-P1-2). Mirrors the orchestrator-event bridge's reauth handling for the
/// case where the orchestrator is never spawned.
fn emit_needs_reauth(app: &AppHandle, account: &AccountRow) {
    if let Err(err) =
        events::emit_account_needs_reauth(app, &account.id.to_string(), &account.email)
    {
        tracing::debug!(
            target: TARGET,
            account_id = %account.id,
            %err,
            "emit account:needs_reauth (assembly) failed"
        );
    }
    tray::notify_needs_reauth(app, &account.email);
}

/// R2-P2-1 (BYO-only): resolve the OAuth client creds from the ENV override only
/// (a TEST injection seam). There is NO baked-in production default client, so
/// this returns whatever the env carries (an empty client id when unset). A
/// production account always reaches [`resolve_account_oauth_creds`] with its
/// PERSISTED BYO creds; this env-only path is the fallback for the e2e seam, and
/// an empty client id surfaces a clear `invalid_client` rather than silently
/// using a Driven-owned client.
fn resolve_oauth_creds() -> (String, String) {
    let client_id = std::env::var(ENV_OAUTH_CLIENT_ID).unwrap_or_default();
    let client_secret = std::env::var(ENV_OAUTH_CLIENT_SECRET).unwrap_or_default();
    (client_id, client_secret)
}

/// A1: resolve the OAuth client creds for `account_id`, preferring its PERSISTED
/// BYO client creds (loaded from the keychain) over the env / public default.
///
/// The refresh token in the keychain was minted by a specific OAuth client; a
/// refresh against a different client fails (`invalid_client`). So an account
/// that brought its own client MUST refresh against that same client across
/// restarts. Shared with `commands::sources` (the Drive-folder picker builds the
/// same one-off store). NEVER logs the secret.
pub fn resolve_account_oauth_creds(account_id: AccountId) -> (String, String) {
    use driven_drive::google::token_store::ClientCredsStore;
    match ClientCredsStore::new(account_id.to_string()).load() {
        Ok(Some(creds)) if !creds.client_id.trim().is_empty() => {
            (creds.client_id, creds.client_secret)
        }
        Ok(_) => resolve_oauth_creds(),
        Err(err) => {
            tracing::warn!(
                target: TARGET,
                account_id = %account_id,
                %err,
                "failed to load account BYO client creds from keychain; using env/default client"
            );
            resolve_oauth_creds()
        }
    }
}

/// Build the Windows VSS snapshot provider (ROADMAP M3.5; DESIGN s5.3.1), or
/// `None` off Windows. The returned `Arc` is threaded into BOTH the executor
/// (snapshot reads) and the orchestrator (per-cycle release + orphan cleanup).
///
/// Two Windows shapes (issue #25):
/// - `vss_helper` present (the app is UN-elevated): a [`BrokeredVssProvider`]
///   that reads locked files THROUGH the elevated helper broker, launched via the
///   shared [`VssHelperManager`] launcher, so the main app stays un-elevated. The
///   `windows.vss_helper` SETTING gates the launcher (a disabled launcher reports
///   unavailable, so the provider behaves like the historical skip), and the user
///   can flip it on at runtime because the manager already exists and is shared.
/// - `vss_helper` absent (the app is ALREADY elevated): the in-process
///   [`RealVssProvider`] - which snapshots directly, exactly as before.
#[cfg(windows)]
fn build_vss(
    config: &OrchestratorConfig,
    vss_helper: Option<&Arc<VssHelperManager>>,
) -> Option<Arc<dyn driven_vss::VssProvider>> {
    match vss_helper {
        Some(manager) => {
            let launcher: Arc<dyn HelperLauncher> = manager.clone();
            let provider = BrokeredVssProvider::new(
                config.vss_mode,
                manager.pipe_name(),
                manager.helper_dir(),
                manager.temp_dir(),
            )
            .with_launcher(launcher);
            Some(Arc::new(provider))
        }
        None => Some(Arc::new(driven_vss::RealVssProvider::new(config.vss_mode))),
    }
}

/// Off Windows there is no VSS; the executor's locked-file path skips as before.
#[cfg(not(windows))]
fn build_vss(
    _config: &OrchestratorConfig,
    _vss_helper: Option<&Arc<VssHelperManager>>,
) -> Option<Arc<dyn driven_vss::VssProvider>> {
    None
}

/// Build the app-side least-privilege VSS helper broker manager (DESIGN s5.3.1),
/// or `None` when the helper cannot be in play. Built ONCE at boot and shared into
/// every account's [`BrokeredVssProvider`], so there is a SINGLE launch / UAC
/// prompt / pipe across all accounts.
///
/// Built whenever the app is UN-elevated on Windows - REGARDLESS of the
/// `windows.vss_helper` setting - so the user can flip the setting on at runtime
/// and have the already-wired providers use the broker without an app restart.
/// The `enabled` flag (the current setting) gates the manager's behaviour: a
/// disabled manager never launches and reports itself unavailable, so the
/// providers behave exactly like the historical un-elevated skip until the user
/// enables it. Boot is LAZY (no UAC prompt at silent startup); the eager prompt
/// only fires when the user toggles the setting on.
///
/// `None` when: off Windows; the app is ALREADY elevated (it uses the in-process
/// [`driven_vss::RealVssProvider`] and needs no broker); or the current-exe path
/// cannot be resolved (so the bundled sidecar cannot be located). The broker's
/// allow-list of snapshot-able roots is the union of the configured source roots
/// at boot - a source ADDED mid-session is covered on the next app restart (the
/// roots are fixed at broker launch per the DESIGN trust model).
#[cfg(windows)]
fn build_vss_helper_manager(
    all_sources: &[SourceRow],
    helper_enabled: bool,
) -> Option<Arc<VssHelperManager>> {
    if driven_vss::is_elevated() {
        return None;
    }
    let helper_exe = VssHelperManager::bundled_helper_exe()?;
    // Union of the configured source roots (dedup), fixed as the broker's
    // snapshot-able allow-list at launch.
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    for s in all_sources {
        let p = std::path::PathBuf::from(&s.local_path);
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    // App-owned scratch dir where the provider streams locked-file temp copies.
    let temp_dir = std::env::temp_dir().join("driven-vss-helper");
    tracing::info!(
        target: TARGET,
        roots = roots.len(),
        enabled = helper_enabled,
        "issue #25: least-privilege VSS helper manager built (un-elevated); boot is lazy - the broker launches on the first locked file when enabled, or immediately when the user toggles it on"
    );
    Some(Arc::new(VssHelperManager::new(
        helper_exe,
        temp_dir,
        roots,
        helper_enabled,
    )))
}

/// Off Windows the helper does not exist.
#[cfg(not(windows))]
fn build_vss_helper_manager(
    _all_sources: &[SourceRow],
    _helper_enabled: bool,
) -> Option<Arc<VssHelperManager>> {
    None
}

/// Bridge the real [`NotifyWatcher`] for `account`'s sources into the
/// orchestrator's watcher channel (DESIGN s5.9.1). Establishes a watch per
/// enabled source, then forwards each debounced `ScanTickRequest` into the
/// orchestrator's `sender` (its `watcher_sender()`). Best-effort: a watcher
/// that cannot be built / watched is logged and the source relies on the
/// scheduled scan fallback.
fn spawn_watcher_bridge(
    account_id: AccountId,
    sender: tokio::sync::mpsc::Sender<driven_core::watcher::ScanTickRequest>,
    sources: &[SourceRow],
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let enabled: Vec<SourceRow> = sources.iter().filter(|s| s.enabled).cloned().collect();
    if enabled.is_empty() {
        return None;
    }

    let watcher = NotifyWatcher::new(enabled.clone());
    let Some(mut rx) = watcher.subscribe() else {
        tracing::warn!(
            target: TARGET,
            account_id = %account_id,
            "watcher subscribe returned no receiver; relying on scheduled scan"
        );
        return None;
    };

    for source in &enabled {
        if let Err(err) = watcher.watch(source.id) {
            tracing::warn!(
                target: TARGET,
                account_id = %account_id,
                source_id = %source.id,
                %err,
                "failed to establish filesystem watch; scheduled scan covers this source"
            );
        }
    }

    // Move the watcher into the task so its OS handles + debounce tasks stay
    // alive for the run's lifetime (dropping `NotifyWatcher` tears down every
    // watch). R-P1-1: the `NotifyWatcher` owns the mpsc::Sender feeding `rx`, so
    // `rx.recv()` NEVER returns `None` on its own - the bridge MUST also
    // select! on the shutdown signal, or quit would orphan this task. On
    // shutdown the task returns and `_watcher` drops, releasing the OS watches.
    Some(tokio::spawn(async move {
        let _watcher = watcher;
        loop {
            tokio::select! {
                maybe_tick = rx.recv() => {
                    let Some(tick) = maybe_tick else {
                        // The watcher was torn down elsewhere; nothing left to
                        // forward.
                        break;
                    };
                    // `send` (not `try_send`): apply backpressure rather than
                    // drop a scan-tick. A closed receiver means the orchestrator
                    // stopped, so end the bridge.
                    if sender.send(tick).await.is_err() {
                        tracing::debug!(
                            target: TARGET,
                            account_id = %account_id,
                            "orchestrator watcher channel closed; ending watcher bridge"
                        );
                        break;
                    }
                }
                res = shutdown.changed() => {
                    // `changed()` resolves on a flip OR on sender drop; either
                    // way, if the flag is set (or the sender is gone) we exit so
                    // quit leaves no orphaned watcher bridge.
                    match res {
                        Ok(()) if *shutdown.borrow() => {
                            tracing::debug!(
                                target: TARGET,
                                account_id = %account_id,
                                "shutdown signalled; ending watcher bridge"
                            );
                            break;
                        }
                        Ok(()) => {}
                        Err(_) => break,
                    }
                }
            }
        }
    }))
}

/// Forward one orchestrator's [`OrchestratorEvent`] broadcast (`rx`, from its
/// `subscribe()`) to the tray + the SPEC s11.7 webview events. A `StateChanged`
/// drives `tray::apply_state` and emits `sync:status_changed`; an
/// `ActivityWritten` re-emits `activity:new` for the live tail; a `Lagged`
/// receiver emits `activity:lagged` so the webview reconciles the dropped rows
/// from the durable `activity_log` (M7-P1-1), and the next `StateChanged`
/// re-syncs the tray. The bridge ends when the broadcast closes (orchestrator
/// dropped).
fn spawn_event_bridge(
    app: &AppHandle,
    account_id: AccountId,
    email: String,
    mut rx: tokio::sync::broadcast::Receiver<OrchestratorEvent>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let app = app.clone();
    tokio::spawn(async move {
        loop {
            // R-P1-1: the broadcast closes naturally when the orchestrator is
            // dropped, but quit drops the orchestrator AFTER draining tasks, so
            // also select! on the shutdown signal to end this bridge promptly
            // (and never orphan it).
            let event = tokio::select! {
                res = shutdown.changed() => {
                    match res {
                        Ok(()) if *shutdown.borrow() => {
                            tracing::debug!(
                                target: TARGET,
                                account_id = %account_id,
                                "shutdown signalled; ending event bridge"
                            );
                            break;
                        }
                        Ok(()) => continue,
                        Err(_) => break,
                    }
                }
                event = rx.recv() => event,
            };
            // M7-P1-1: route every received event (or recv error) through the
            // pure `classify_bridge_event` decision so the side-effecting arms
            // below stay a thin dispatch and the Lagged-reconcile decision is
            // unit-testable without an `AppHandle`.
            match classify_bridge_event(event) {
                BridgeAction::SyncStatus { state } => {
                    // Reflect the new state on the tray icon (SPEC s12).
                    tray::apply_state(&app, account_id, state.clone());
                    // Push a global-status refresh to the webview (SPEC s11.7).
                    // M5 carries a single-account snapshot in the payload; M6
                    // aggregates across accounts into the full DTO.
                    let payload = AccountSyncStatusEvent {
                        account_id: account_id.to_string(),
                        state,
                    };
                    if let Err(err) = events::emit_sync_status_changed(&app, &payload) {
                        tracing::debug!(
                            target: TARGET,
                            account_id = %account_id,
                            %err,
                            "emit sync:status_changed failed"
                        );
                    }
                }
                BridgeAction::NeedsReauth { account_id: acct } => {
                    // V-F: a refresh token was revoked. Surface the re-consent
                    // banner to the webview (SPEC s11.7 `account:needs_reauth`)
                    // and raise the OS notification (DESIGN s8.1) - the
                    // `OrchestratorState` cannot carry the email, so this is the
                    // one place that has both the account id and its email.
                    if let Err(err) =
                        events::emit_account_needs_reauth(&app, &acct.to_string(), &email)
                    {
                        tracing::debug!(
                            target: TARGET,
                            account_id = %acct,
                            %err,
                            "emit account:needs_reauth failed"
                        );
                    }
                    tray::notify_needs_reauth(&app, &email);
                }
                BridgeAction::ActivityNew { entry } => {
                    // M7: forward every durable activity row to the webview as
                    // `activity:new` (SPEC s11.7) so the Activity dashboard's
                    // live tail updates within 500ms - event-driven, no polling.
                    // The carried `ActivityEntry` is already the camelCase wire
                    // shape, so it re-emits with no re-mapping.
                    if let Err(err) = events::emit_activity_new(&app, &entry) {
                        tracing::debug!(
                            target: TARGET,
                            account_id = %account_id,
                            %err,
                            "emit activity:new failed"
                        );
                    }
                }
                BridgeAction::ActivityReconcile { skipped } => {
                    // M7-P1-1: the bounded broadcast dropped `skipped` events, so
                    // the dropped `activity:new` rows are gone from this stream.
                    // Rather than silently lose them from the live tail (DESIGN
                    // s8.3 last-1000 tail; ROADMAP M7 <500ms), emit a typed gap
                    // signal so the webview store reconciles from the durable
                    // `activity_log` (the source of truth) via a `query_activity`
                    // page-0 re-fetch + dedup merge - no durable row is missed.
                    // The StateChanged tray sync still re-syncs on the next event.
                    tracing::debug!(
                        target: TARGET,
                        account_id = %account_id,
                        skipped,
                        "event bridge lagged; emitting activity:lagged reconcile signal"
                    );
                    if let Err(err) = events::emit_activity_lagged(&app, skipped) {
                        tracing::debug!(
                            target: TARGET,
                            account_id = %account_id,
                            %err,
                            "emit activity:lagged failed"
                        );
                    }
                }
                BridgeAction::Ignore => {
                    // Progress / Power / Network events: not bridged to the
                    // webview in M5 (the progress DTO lands with a later
                    // milestone). The tray's coarse state is driven by
                    // StateChanged above.
                }
                BridgeAction::Stop => {
                    tracing::debug!(
                        target: TARGET,
                        account_id = %account_id,
                        email = %email,
                        "orchestrator event stream closed; ending event bridge"
                    );
                    break;
                }
            }
        }
    })
}

/// The decision the event bridge takes for one received [`OrchestratorEvent`]
/// (or a broadcast recv error). Factored out of [`spawn_event_bridge`] as a pure
/// value so the M7-P1-1 lag-reconcile decision (and every other arm) is
/// unit-testable WITHOUT a Tauri `AppHandle` / spawned task.
enum BridgeAction {
    /// Apply the new state to the tray and emit `sync:status_changed`.
    SyncStatus {
        state: driven_core::types::OrchestratorState,
    },
    /// Emit `account:needs_reauth` + raise the OS notification.
    NeedsReauth { account_id: AccountId },
    /// Re-emit the durable activity row as `activity:new` (live tail).
    ActivityNew {
        entry: driven_core::types::ActivityEntry,
    },
    /// M7-P1-1: the broadcast lagged and dropped `skipped` events; emit
    /// `activity:lagged` so the webview reconciles from the durable
    /// `activity_log`. No durable row is lost.
    ActivityReconcile { skipped: u64 },
    /// A non-bridged event (progress / power / network); do nothing.
    Ignore,
    /// The broadcast closed (orchestrator dropped); end the bridge.
    Stop,
}

/// Pure classification of a broadcast `recv()` result into the [`BridgeAction`]
/// the bridge should take (M7-P1-1). Keeping this side-effect-free lets the
/// Lagged -> reconcile mapping be asserted in a unit test (see this module's
/// `tests`) rather than only through a live Tauri runtime.
fn classify_bridge_event(
    event: Result<OrchestratorEvent, tokio::sync::broadcast::error::RecvError>,
) -> BridgeAction {
    match event {
        Ok(OrchestratorEvent::StateChanged { state }) => BridgeAction::SyncStatus { state },
        Ok(OrchestratorEvent::AccountNeedsReauth { account_id }) => {
            BridgeAction::NeedsReauth { account_id }
        }
        Ok(OrchestratorEvent::ActivityWritten { entry }) => BridgeAction::ActivityNew { entry },
        Ok(_) => BridgeAction::Ignore,
        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
            BridgeAction::ActivityReconcile { skipped }
        }
        Err(tokio::sync::broadcast::error::RecvError::Closed) => BridgeAction::Stop,
    }
}

/// The per-event `sync:status_changed` payload bridged from one orchestrator's
/// state change (SPEC s11.7). A minimal-but-real shape mirroring
/// `commands::sync::AccountSyncStatus` so the webview sees a concrete account +
/// state; M6 swaps this for the aggregated `GlobalSyncStatus`.
#[derive(serde::Serialize, Clone)]
struct AccountSyncStatusEvent {
    account_id: String,
    state: driven_core::types::OrchestratorState,
}

#[cfg(test)]
mod tests {
    use super::{classify_bridge_event, BridgeAction};
    use driven_core::orchestrator::OrchestratorConfig;
    use driven_core::state::sqlite::SqliteStateRepo;
    use driven_core::state::StateRepo;
    use driven_core::types::{ActivityEntry, OrchestratorEvent};
    use tokio::sync::broadcast::error::RecvError;

    /// M7-P1-1: a broadcast `Lagged` MUST classify as an `ActivityReconcile`
    /// (carrying the dropped count) so the bridge emits `activity:lagged` and the
    /// webview reconciles the dropped rows from the durable `activity_log` -
    /// never silently drops them from the live tail.
    #[test]
    fn lagged_classifies_as_activity_reconcile() {
        match classify_bridge_event(Err(RecvError::Lagged(42))) {
            BridgeAction::ActivityReconcile { skipped } => assert_eq!(skipped, 42),
            other => panic!(
                "Lagged must reconcile, got {:?}",
                BridgeActionKind::of(&other)
            ),
        }
    }

    /// A normal `ActivityWritten` classifies as `ActivityNew` (the 500ms live
    /// path), carrying the entry unchanged - so the typical path stays
    /// event-driven and only the rare lag triggers a reconcile.
    #[test]
    fn activity_written_classifies_as_activity_new() {
        let entry = ActivityEntry {
            id: 7,
            ts: 1000,
            source_id: None,
            level: driven_core::state::ActivityLevel::Info,
            event_type: "upload_done".to_string(),
            file_count: None,
            bytes: None,
            message: None,
        };
        match classify_bridge_event(Ok(OrchestratorEvent::ActivityWritten {
            entry: entry.clone(),
        })) {
            BridgeAction::ActivityNew { entry: got } => assert_eq!(got, entry),
            other => panic!(
                "ActivityWritten must emit new, got {:?}",
                BridgeActionKind::of(&other)
            ),
        }
    }

    /// A closed broadcast classifies as `Stop` so the bridge ends (no orphaned
    /// task); a non-bridged event (`Power`) classifies as `Ignore`.
    #[test]
    fn closed_stops_and_unbridged_is_ignored() {
        assert!(matches!(
            classify_bridge_event(Err(RecvError::Closed)),
            BridgeAction::Stop
        ));
        assert!(matches!(
            classify_bridge_event(Ok(OrchestratorEvent::Power {
                event: driven_core::types::PowerEvent::Resumed,
            })),
            BridgeAction::Ignore
        ));
    }

    /// Test-only stringifier so a failing classification assertion prints which
    /// variant it actually got (the `BridgeAction` payloads are not all `Debug`).
    #[derive(Debug)]
    enum BridgeActionKind {
        SyncStatus,
        NeedsReauth,
        ActivityNew,
        ActivityReconcile,
        Ignore,
        Stop,
    }
    impl BridgeActionKind {
        fn of(a: &BridgeAction) -> Self {
            match a {
                BridgeAction::SyncStatus { .. } => Self::SyncStatus,
                BridgeAction::NeedsReauth { .. } => Self::NeedsReauth,
                BridgeAction::ActivityNew { .. } => Self::ActivityNew,
                BridgeAction::ActivityReconcile { .. } => Self::ActivityReconcile,
                BridgeAction::Ignore => Self::Ignore,
                BridgeAction::Stop => Self::Stop,
            }
        }
    }

    /// R1-P2-1: cold-start orchestrators must build their [`OrchestratorConfig`]
    /// from the PERSISTED SPEC s22 settings, not the hard defaults. `build_account`
    /// now reads `commands::settings::load_orchestrator_config` at assembly time
    /// (replacing the old `OrchestratorConfig::default()`); this asserts that a
    /// persisted NON-DEFAULT setting is reflected in the config that path yields,
    /// so a fresh boot honours the user's saved settings without a live edit.
    #[tokio::test]
    async fn cold_start_config_reflects_persisted_non_default_setting() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("driven-assembly-cfg-{nonce}"));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = SqliteStateRepo::open(&dir.join("state.db"))
            .await
            .expect("open repo");

        // The hard default scan cadence is 600s; persist a DISTINCT non-default
        // value so a cold start that ignored persisted settings would fail this.
        let default_cfg = OrchestratorConfig::default();
        let persisted_scan_secs: u64 = 123;
        assert_ne!(
            default_cfg.scan_interval_secs, persisted_scan_secs,
            "fixture must differ from the default to prove the persisted value wins"
        );
        let global = serde_json::json!({
            "auto_start_on_login": false,
            "default_concurrent_uploads": serde_json::Value::Null,
            "bandwidth_cap_mbps": 7,
            "skip_on_battery": false,
            "skip_on_metered": false,
            "scan_interval_secs": persisted_scan_secs,
            "deep_verify_interval_secs": 604_800,
            "io_priority": "low",
            "log_level": "info",
        });
        repo.set_setting("global", &global)
            .await
            .expect("seed global");

        // The EXACT function `build_account` reads at cold start.
        let cfg = crate::commands::settings::load_orchestrator_config(&repo)
            .await
            .expect("load config");
        assert_eq!(
            cfg.scan_interval_secs, persisted_scan_secs,
            "cold-start config must reflect the persisted scan cadence (R1-P2-1)"
        );
        assert_eq!(cfg.bandwidth_cap_mbps, Some(7));
        assert!(!cfg.skip_on_battery);
        assert!(!cfg.skip_on_metered);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// R8-P1-1 (DATA-SAFETY): the boot path must FAIL CLOSED on a repair error -
    /// `repair_allows_spawn` is the gate. A successful repair (`Ok`) permits
    /// spawning the orchestrators; a repair error (`Err`) does NOT, so the boot
    /// path goes quiesced and no orchestrator (encrypted or otherwise) starts.
    #[test]
    fn repair_error_does_not_allow_spawn() {
        assert!(
            super::repair_allows_spawn(&Ok(0)),
            "a clean repair (no accounts touched) must allow spawning"
        );
        assert!(
            super::repair_allows_spawn(&Ok(3)),
            "a clean repair (some accounts repaired) must allow spawning"
        );
        assert!(
            !super::repair_allows_spawn(&Err(anyhow::anyhow!("injected repair failure"))),
            "a FAILED repair must NOT allow spawning (fail closed)"
        );
    }

    /// R8-P1-1 (DATA-SAFETY): when the repair fails, the boot path builds a
    /// QUIESCED AppState via `build_quiesced` - even with an ENABLED ENCRYPTED
    /// account+source present in the DB, it spawns ZERO orchestrators, so the
    /// unsafe pre-0004 encrypted source does NOT keep syncing. The companion clean
    /// path (a healthy DB) is covered by the live boot in lib.rs (which needs an
    /// AppHandle); here we prove the fail-closed branch starts nothing.
    #[tokio::test]
    async fn quiesced_build_spawns_no_orchestrators_even_with_enabled_encrypted_source() {
        use super::{AccountRow, SourceRow};
        use driven_core::types::{AccountId, AccountState, SourceId};

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("driven-assembly-quiesce-{nonce}"));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = SqliteStateRepo::open(&dir.join("state.db"))
            .await
            .expect("open repo");

        // Seed an Ok account WITH an encryption master key + an ENABLED encrypted
        // source - the exact "unsafe pre-0004" shape the repair guards. If the boot
        // path spawned orchestrators, THIS source would resume encrypted backups.
        let account_id = AccountId::new_v4();
        repo.upsert_account(&AccountRow {
            id: account_id,
            email: "quiesce@example.com".to_string(),
            display_name: None,
            state: AccountState::Ok,
            encryption_master_key_id: Some("mk-quiesce".to_string()),
            created_at: 1,
            last_synced_at: None,
        })
        .await
        .expect("seed account");
        repo.upsert_source(&SourceRow {
            id: SourceId::new_v4(),
            account_id,
            display_name: "enc-src".to_string(),
            enabled: true,
            local_path: dir.join("src").to_string_lossy().into_owned(),
            drive_folder_id: "folder-1".to_string(),
            drive_id: None,
            drive_folder_path: "/Backups".to_string(),
            encryption_enabled: true,
            wrapped_source_key: Some(vec![1, 2, 3, 4]),
            respect_gitignore: true,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            placeholder_policy: Default::default(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            mtime_granularity_ns: None,
            created_at: 1,
        })
        .await
        .expect("seed source");

        let state: std::sync::Arc<dyn StateRepo> = std::sync::Arc::new(repo);
        // The fail-closed builder: NO AppHandle, NO orchestrators.
        let app_state = super::build_quiesced(state);
        assert!(
            app_state.accounts().is_empty(),
            "quiesced boot must spawn ZERO orchestrators (fail closed), got {}",
            app_state.accounts().len()
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
