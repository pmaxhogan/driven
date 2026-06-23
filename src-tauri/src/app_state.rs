//! [`AppState`]: the Tauri-managed shared application state (SPEC s11).
//!
//! Held behind Tauri's `State<AppState>` and reached by every IPC command
//! (SPEC s11.3). It owns the [`StateRepo`] handle plus one orchestrator
//! handle per account - the `Arc<dyn Orchestrator>` control surface and the
//! `JoinHandle` of its spawned run loop (SPEC s5: one orchestrator per
//! account). The remote-construction mode records whether assembly built
//! real `GoogleDriveStore`s or the in-memory fake (`DRIVEN_USE_FAKE_REMOTE`).

use std::collections::HashMap;
use std::sync::Arc;

use driven_core::orchestrator::Orchestrator;
use driven_core::state::StateRepo;
use driven_core::types::AccountId;
use tokio::task::JoinHandle;

/// How the per-account remote store was constructed at assembly time.
///
/// Recorded so IPC / diagnostics can tell a real run from a fake-backed one
/// (`DRIVEN_USE_FAKE_REMOTE=1` selects [`RemoteMode::Fake`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteMode {
    /// Real `GoogleDriveStore` built from the keyring refresh token.
    RealGoogleDrive,
    /// `InMemoryRemoteStore` (test / dev; `DRIVEN_USE_FAKE_REMOTE=1`).
    Fake,
}

/// One account's live orchestrator: the control-surface handle plus the
/// `JoinHandle` of its spawned [`Orchestrator::run`] loop (SPEC s5).
///
/// Held so IPC can drive the orchestrator (`trigger` / `set_paused` /
/// `state`) and so a clean shutdown can abort the run loop (ROADMAP M5
/// "Quit cleanly shuts down the runtime, no orphaned tokio tasks").
pub struct AccountHandle {
    /// The per-account orchestrator control surface.
    pub orchestrator: Arc<dyn Orchestrator>,
    /// The spawned run-loop task; aborted on shutdown.
    pub run_loop: JoinHandle<()>,
}

/// The Tauri-managed application state (SPEC s11).
pub struct AppState {
    /// The SQLite state layer (SPEC s2), shared by every account + IPC path.
    state: Arc<dyn StateRepo>,
    /// Per-account orchestrator handles, keyed by [`AccountId`].
    accounts: HashMap<AccountId, AccountHandle>,
    /// How remotes were constructed this run (real Drive vs in-memory fake).
    remote_mode: RemoteMode,
}

impl AppState {
    /// Build the managed state from the state repo, the per-account handles,
    /// and the remote-construction mode (called by `assembly::build_and_spawn`).
    #[must_use]
    pub fn new(
        state: Arc<dyn StateRepo>,
        accounts: HashMap<AccountId, AccountHandle>,
        remote_mode: RemoteMode,
    ) -> Self {
        Self {
            state,
            accounts,
            remote_mode,
        }
    }

    /// Borrow the shared [`StateRepo`] (SPEC s2).
    #[must_use]
    pub fn state(&self) -> &Arc<dyn StateRepo> {
        &self.state
    }

    /// The orchestrator handle for `account`, if present.
    #[must_use]
    pub fn account(&self, account: AccountId) -> Option<&AccountHandle> {
        self.accounts.get(&account)
    }

    /// Iterate every account's orchestrator handle (for "all accounts"
    /// commands like `sync_now(None)` / `pause_sync`).
    pub fn accounts(&self) -> impl Iterator<Item = (&AccountId, &AccountHandle)> {
        self.accounts.iter()
    }

    /// How remotes were constructed this run.
    #[must_use]
    pub fn remote_mode(&self) -> RemoteMode {
        self.remote_mode
    }
}
