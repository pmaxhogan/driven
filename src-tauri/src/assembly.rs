//! App-shell assembly: wire the real seams, build one orchestrator per
//! account, and spawn each run loop (SPEC s5, DESIGN s5.1).
//!
//! This is where the abstract `driven-core` seams meet their PRODUCTION
//! implementations. For every account in the state DB, [`build_and_spawn`]:
//! 1. constructs the [`RemoteStore`](driven_drive::remote_store::RemoteStore) -
//!    a real `GoogleDriveStore` from the keyring refresh token via
//!    `RefreshingTokenSource` (the `driven-cli` `build_store` pattern), OR an
//!    `InMemoryRemoteStore` when `DRIVEN_USE_FAKE_REMOTE=1` (dev / e2e);
//! 2. builds the real `Pacer`, `RealPowerSource`, a `ReqwestBackend`-backed
//!    `NetworkProbe` (`Some`, so the Drive breaker is driven by REAL request
//!    outcomes - CODEX_NOTES P2-9), the Windows `VssProvider` (M3.5), and a
//!    [`KeystoreCryptoProvider`](crate::crypto_provider_impl::KeystoreCryptoProvider)
//!    (per-source crypto, GA blocker);
//! 3. assembles [`ExecutorDeps`] -> `DefaultExecutor`, builds the
//!    [`SyncOrchestrator`] (`.with_vss(..)` on Windows), spawns
//!    [`Orchestrator::run`], and bridges the watcher + power-subscribe;
//! 4. collects the per-account handles into an [`AppState`].

use std::collections::HashMap;
use std::sync::Arc;

use tauri::AppHandle;

use driven_core::state::StateRepo;

use crate::app_state::AppState;

/// Environment flag selecting the in-memory fake remote (dev / e2e) instead of
/// a real `GoogleDriveStore`. Mirrors the assembly contract in the task spec.
pub const ENV_USE_FAKE_REMOTE: &str = "DRIVEN_USE_FAKE_REMOTE";

/// Build every account's orchestrator over the real seams and spawn its run
/// loop, returning the [`AppState`] for `.manage(..)` (SPEC s5).
///
/// `app` is the Tauri handle (for the orchestrator-event -> tray/IPC bridge);
/// `state` is the already-migrated [`StateRepo`] from
/// [`crate::migrations::run`].
///
/// TODO(M5): implement the per-account assembly described in the module docs.
/// Concretely, for each `state.list_accounts().await?`:
/// - remote: if `std::env::var(ENV_USE_FAKE_REMOTE) == Ok("1")` ->
///   `Arc::new(InMemoryRemoteStore::new())`; else build a `GoogleDriveStore`
///   from `KeyringTokenStore` + `RefreshingTokenSource::from_stored_refresh_token`
///   (the `driven-cli::build_store` pattern) and `Arc` it;
/// - pacer: the real `Pacer` impl seeded from the account's settings;
/// - power: `Arc::new(RealPowerSource::new()?)` (+ `spawn_poller`);
/// - network: `Arc::new(Prober::new(Arc::new(ReqwestBackend::new()?), clock))`
///   - pass `Some(..)` into `ExecutorDeps.network` so the breaker sees real
///   Drive outcomes (CODEX_NOTES P2-9);
/// - vss (Windows): `Arc::new(RealVssProvider::new(config.vss_mode))`, threaded
///   into BOTH `ExecutorDeps.vss` and `SyncOrchestrator::with_vss` (the SAME
///   Arc, DESIGN s5.3);
/// - crypto: `Arc::new(KeystoreCryptoProvider::new(account.id, sources))` into
///   `ExecutorDeps.crypto` (per-source, FAIL CLOSED - GA blocker);
/// - executor: `Arc::new(DefaultExecutor::new(deps))`;
/// - orchestrator: `SyncOrchestrator::new(account.id, state, executor, power,
///   network, clock, config)` (`.with_vss(vss)` on Windows), `Arc` it, spawn
///   `tokio::spawn(orch.clone().run())`, and store the `JoinHandle` +
///   `Arc<dyn Orchestrator>` in an `AccountHandle`;
/// - bridge: subscribe to the orchestrator's event broadcast and call
///   `tray::apply_state` + the SPEC s11.7 emit helpers; bridge the watcher
///   ticks + power subscribe.
/// Then assemble `AppState::new(state, handles, remote_mode)`.
pub async fn build_and_spawn(
    app: &AppHandle,
    state: Arc<dyn StateRepo>,
) -> anyhow::Result<AppState> {
    let _ = (app, state, ENV_USE_FAKE_REMOTE, HashMap::<(), ()>::new());
    todo!("M5: per-account real-seam assembly + orchestrator spawn + event bridge -> AppState")
}
