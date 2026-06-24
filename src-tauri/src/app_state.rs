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
use tokio::sync::Mutex;
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
/// `state`) and so a clean shutdown can GRACEFULLY drain the run loop (ROADMAP
/// M5 "Quit cleanly shuts down the runtime, no orphaned tokio tasks";
/// DESIGN s5.10.2 in-flight drain).
pub struct AccountHandle {
    /// The per-account orchestrator control surface.
    pub orchestrator: Arc<dyn Orchestrator>,
    /// The spawned run-loop task. Behind a `Mutex<Option<..>>` so a clean
    /// shutdown can TAKE + await it for a graceful drain (awaiting a
    /// `JoinHandle` needs ownership, which the shared `&AppState` cannot give
    /// otherwise). `None` once drained.
    pub run_loop: Mutex<Option<JoinHandle<()>>>,
}

impl AccountHandle {
    /// Build a handle from the orchestrator control surface + its spawned run
    /// loop.
    #[must_use]
    pub fn new(orchestrator: Arc<dyn Orchestrator>, run_loop: JoinHandle<()>) -> Self {
        Self {
            orchestrator,
            run_loop: Mutex::new(Some(run_loop)),
        }
    }

    /// Await the run-loop task to completion (the graceful-drain path). Takes
    /// the handle out so the wait happens by value; a second call (already
    /// drained) returns immediately. Errors awaiting the task (cancelled /
    /// panicked) are swallowed - the goal is "the task is no longer running".
    pub async fn run_loop_drain(&self) {
        let taken = self.run_loop.lock().await.take();
        if let Some(handle) = taken {
            let _ = handle.await;
        }
    }

    /// An [`tokio::task::AbortHandle`] for the run loop (the timeout-fallback
    /// path), or `None` if it was already drained/taken.
    pub async fn run_loop_abort_handle(&self) -> Option<tokio::task::AbortHandle> {
        self.run_loop
            .lock()
            .await
            .as_ref()
            .map(JoinHandle::abort_handle)
    }
}

/// The Tauri-managed application state (SPEC s11).
pub struct AppState {
    /// The SQLite state layer (SPEC s2), shared by every account + IPC path.
    state: Arc<dyn StateRepo>,
    /// Per-account orchestrator handles, keyed by [`AccountId`].
    accounts: HashMap<AccountId, AccountHandle>,
    /// How remotes were constructed this run (real Drive vs in-memory fake).
    remote_mode: RemoteMode,
    /// C5-P2-1: per-account pause "generation" token. Every pause/resume bumps
    /// the account's generation; a TIMED pause captures the new generation and
    /// its detached auto-resume timer only fires if the generation still
    /// matches when it wakes. A newer pause/resume (e.g. a `pause(None)`
    /// indefinite pause issued before the old timer fires) bumps the generation
    /// and thereby CANCELS the stale timer's auto-resume. Behind a sync `Mutex`
    /// (only ever held for a counter bump/read, never across an await).
    pause_generations: std::sync::Mutex<HashMap<AccountId, u64>>,
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
            pause_generations: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Bump and return the new pause generation for `account` (C5-P2-1). Called
    /// on every pause/resume so any in-flight timed-resume timer for that
    /// account is superseded.
    #[must_use]
    pub fn bump_pause_generation(&self, account: AccountId) -> u64 {
        let mut map = self
            .pause_generations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let gen = map.entry(account).or_insert(0);
        *gen = gen.wrapping_add(1);
        *gen
    }

    /// `true` if `account`'s current pause generation still equals `token`
    /// (C5-P2-1): the timed-resume timer auto-resumes ONLY when this holds, so a
    /// newer pause/resume that bumped the generation cancels the stale timer.
    #[must_use]
    pub fn pause_generation_matches(&self, account: AccountId, token: u64) -> bool {
        let map = self
            .pause_generations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.get(&account).copied() == Some(token)
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
