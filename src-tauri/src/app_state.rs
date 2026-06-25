//! [`AppState`]: the Tauri-managed shared application state (SPEC s11).
//!
//! Held behind Tauri's `State<AppState>` and reached by every IPC command
//! (SPEC s11.3). It owns the [`StateRepo`] handle plus one orchestrator
//! handle per account - the `Arc<dyn Orchestrator>` control surface and the
//! `JoinHandle` of its spawned run loop (SPEC s5: one orchestrator per
//! account). The remote-construction mode records whether assembly built
//! real `GoogleDriveStore`s or the in-memory fake (`DRIVEN_USE_FAKE_REMOTE`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use driven_core::orchestrator::Orchestrator;
use driven_core::state::StateRepo;
use driven_core::types::AccountId;
use driven_drive::fake::InMemoryRemoteStore;
use tokio::sync::{watch, Mutex};
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

/// One account's live orchestrator: the control-surface handle plus EVERY
/// per-account tokio task spawned by `assembly::build_account` (SPEC s5,
/// ROADMAP M5 "no orphaned tokio tasks"; DESIGN s5.10.2 in-flight drain).
///
/// R-P1-1: a clean Quit must leave NO orphaned tasks. Four tasks are spawned
/// per account and ALL are tracked here so [`Self::shutdown`] can drain them:
/// - `run_loop`: [`Orchestrator::run`]. Stopped via `Orchestrator::shutdown()`.
/// - `watcher_bridge`: forwards `NotifyWatcher` scan-ticks into the
///   orchestrator. The watcher owns the `mpsc::Sender`, so its `recv().await`
///   never closes on its own; it must be signalled via [`Self::bridge_shutdown`]
///   (it `select!`s on that watch) or aborted.
/// - `event_bridge`: forwards the orchestrator's `OrchestratorEvent` broadcast
///   to the tray + webview. It ends naturally when the broadcast closes (the
///   orchestrator dropped) but is ALSO signalled so quit does not have to wait
///   on a `Lagged`/slow consumer; aborted on timeout.
/// - `power_poller`: the `RealPowerSource` 30s poll loop. It loops forever (no
///   natural end), so its handle is KEPT and ABORTED on shutdown - dropping it
///   (the old bug) orphaned the task.
///
/// Held so IPC can drive the orchestrator (`trigger` / `set_paused` / `state`)
/// and so [`Self::shutdown`] can stop + join every task on quit.
pub struct AccountHandle {
    /// The per-account orchestrator control surface.
    pub orchestrator: Arc<dyn Orchestrator>,
    /// B2: the per-account LIVE crypto provider. Held so the source-command
    /// layer can REFRESH its source metadata (`crypto.refresh(..)`) after a
    /// source add / toggle / remove, so a mid-session encrypted source's key is
    /// resolved on the next tick (not stranded `Unavailable` until restart).
    pub crypto: Arc<crate::crypto_provider_impl::KeystoreCryptoProvider>,
    /// The spawned run-loop task. Behind a `Mutex<Option<..>>` so the shutdown
    /// path can TAKE + await it by value; `None` once drained.
    run_loop: Mutex<Option<JoinHandle<()>>>,
    /// The watcher-bridge task (forwards scan-ticks), or `None` when no enabled
    /// source produced a watcher. Drained on shutdown.
    watcher_bridge: Mutex<Option<JoinHandle<()>>>,
    /// The orchestrator-event -> tray/IPC bridge task. Drained on shutdown.
    event_bridge: Mutex<Option<JoinHandle<()>>>,
    /// The power-source poller task. Looped forever; ABORTED on shutdown.
    power_poller: Mutex<Option<JoinHandle<()>>>,
    /// Shutdown signal the watcher + event bridges `select!` on (R-P1-1). Set to
    /// `true` by [`Self::shutdown`] so a bridge whose source never closes
    /// (the watcher owns its `Sender`) still exits promptly.
    bridge_shutdown: watch::Sender<bool>,
}

/// The SHORT per-task graceful-drain budget on quit (DESIGN s5.10.2) for the
/// AUXILIARY tasks (watcher bridge, event bridge, power poller): await each this
/// long before aborting it. These tasks carry no in-flight upload work - they
/// only forward signals or poll - so they should stop near-instantly once the
/// run loop has exited; the short budget keeps a single wedged bridge/poller
/// from holding the join indefinitely.
const TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// R2-P2-1: the run loop's OWN graceful-drain budget on quit. The run loop is
/// the ONLY per-account task that may be mid-`execute` (a large in-flight
/// upload), so DESIGN s5.10.2's "let the current cycle finish" guarantee must
/// give it the FULL drain window - not the short [`TASK_DRAIN_TIMEOUT`] the
/// signal-only bridges use. Before this fix the run loop shared the 5s bridge
/// budget, so a >5s in-flight upload was aborted on explicit Quit even though
/// the intended drain window was ~20s.
///
/// R3-P1-1: `lib.rs`'s quit sweep no longer wraps the per-account drains in an
/// outer timeout (that could drop a cancellation-unsafe drain mid-abort and
/// orphan a task); instead it runs every account's `shutdown()` concurrently and
/// lets each self-bound by this budget (plus the short auxiliary one).
pub const RUN_LOOP_DRAIN_TIMEOUT: Duration = Duration::from_secs(20);

/// The collected per-account task handles + the bridge shutdown sender, returned
/// by `assembly::build_account` and stored on [`AccountHandle`]. Groups the four
/// tracked tasks so the constructor signature stays readable (R-P1-1).
pub struct AccountTasks {
    /// B2: the per-account live crypto provider (so the handle can expose it for
    /// refresh on a source change).
    pub crypto: Arc<crate::crypto_provider_impl::KeystoreCryptoProvider>,
    /// [`Orchestrator::run`] loop.
    pub run_loop: JoinHandle<()>,
    /// Watcher -> orchestrator scan-tick bridge, or `None` if none was spawned.
    pub watcher_bridge: Option<JoinHandle<()>>,
    /// Orchestrator-event -> tray/IPC bridge.
    pub event_bridge: JoinHandle<()>,
    /// Power-source poll loop.
    pub power_poller: JoinHandle<()>,
    /// The sender the watcher + event bridges `select!` on for shutdown.
    pub bridge_shutdown: watch::Sender<bool>,
}

impl AccountHandle {
    /// Build a handle from the orchestrator control surface + the collected
    /// per-account task set (R-P1-1).
    #[must_use]
    pub fn new(orchestrator: Arc<dyn Orchestrator>, tasks: AccountTasks) -> Self {
        Self {
            orchestrator,
            crypto: tasks.crypto,
            run_loop: Mutex::new(Some(tasks.run_loop)),
            watcher_bridge: Mutex::new(tasks.watcher_bridge),
            event_bridge: Mutex::new(Some(tasks.event_bridge)),
            power_poller: Mutex::new(Some(tasks.power_poller)),
            bridge_shutdown: tasks.bridge_shutdown,
        }
    }

    /// Stop + JOIN every per-account task so quit leaves NO orphaned tokio task
    /// (R-P1-1, ROADMAP M5 acceptance; DESIGN s5.10.2 graceful drain).
    ///
    /// Order (R2-P2-1):
    /// 1. signal the orchestrator to stop after its in-flight cycle
    ///    (`Orchestrator::shutdown()`), and signal the bridges via
    ///    [`Self::bridge_shutdown`] (the watcher bridge's source never closes on
    ///    its own);
    /// 2. drain the RUN LOOP FIRST with its OWN full [`RUN_LOOP_DRAIN_TIMEOUT`]
    ///    budget - it is the only task that may be mid-upload, so DESIGN s5.10.2's
    ///    "let the current cycle finish" applies to it and it must NOT be cut off
    ///    by the short bridge budget;
    /// 3. ONLY AFTER the run loop has exited, drain the auxiliary tasks (watcher
    ///    bridge, event bridge, power poller) with the short [`TASK_DRAIN_TIMEOUT`]
    ///    - they carry no upload work and stop promptly once the loop is gone.
    ///
    /// For EVERY tracked handle: await-with-timeout, and on timeout abort the
    /// task AND AWAIT the aborted handle - so the task is truly GONE, not merely
    /// abort-requested. The power poller loops forever, so it always takes the
    /// abort path; the others normally drain cleanly.
    ///
    /// Idempotent: a second call finds every handle already taken and returns
    /// immediately. Errors awaiting a task (cancelled / panicked) are swallowed -
    /// the post-condition is "the task is no longer running".
    pub async fn shutdown(&self) {
        // 1) Signal stop. The orchestrator finishes its current cycle then
        //    returns; the bridges observe the watch flip and select! out.
        self.orchestrator.shutdown();
        // A send error only means there are no live bridge receivers (already
        // gone) - benign.
        let _ = self.bridge_shutdown.send(true);

        // 2) Drain the run loop FIRST with the FULL in-flight budget (R2-P2-1):
        //    a large upload mid-`execute` gets the intended ~20s to finish
        //    rather than being aborted at the 5s bridge budget.
        drain_or_abort(&self.run_loop, RUN_LOOP_DRAIN_TIMEOUT).await;

        // 3) THEN drain the signal-only auxiliary tasks with the SHORT budget.
        //    With the run loop already gone they should stop near-instantly.
        drain_or_abort(&self.watcher_bridge, TASK_DRAIN_TIMEOUT).await;
        drain_or_abort(&self.event_bridge, TASK_DRAIN_TIMEOUT).await;
        drain_or_abort(&self.power_poller, TASK_DRAIN_TIMEOUT).await;
    }
}

/// Take the handle out of `slot` and drive it to a true stop: await it up to
/// `budget`; on timeout `abort()` it and AWAIT the aborted handle so the task is
/// genuinely finished before this returns (R-P1-1). A `None` slot (already
/// drained / never spawned) is a no-op. R2-P2-1: the budget is a parameter so
/// the run loop gets the full [`RUN_LOOP_DRAIN_TIMEOUT`] while the auxiliary
/// tasks use the short [`TASK_DRAIN_TIMEOUT`].
///
/// `tokio::time::timeout` MOVES the `JoinHandle` into itself and, on elapse,
/// DROPS it - and a dropped `JoinHandle` does NOT cancel its task (it merely
/// detaches it). So we capture an [`tokio::task::AbortHandle`] BEFORE the
/// timeout, and on elapse abort via it, then RE-AWAIT the same task via a second
/// `JoinHandle` we also kept... which `timeout` consumed. To avoid that, we do
/// not hand the original handle to `timeout`; we `select!` between the handle and
/// a sleep so the handle stays in scope and can be re-awaited after an abort.
async fn drain_or_abort(slot: &Mutex<Option<JoinHandle<()>>>, budget: Duration) {
    let Some(mut handle) = slot.lock().await.take() else {
        return;
    };
    let abort = handle.abort_handle();
    tokio::select! {
        // Bias toward the task finishing: if it completes within the budget we
        // take this arm and never abort.
        biased;
        _join_result = &mut handle => {
            // Joined cleanly (or the task panicked - either way it is gone).
        }
        () = tokio::time::sleep(budget) => {
            // Budget elapsed: request cancellation, then AWAIT the same handle
            // so the task is genuinely finished (a JoinError::cancelled is the
            // expected, swallowed result) before we return.
            abort.abort();
            let _ = handle.await;
        }
    }
}

/// The Tauri-managed application state (SPEC s11).
pub struct AppState {
    /// The SQLite state layer (SPEC s2), shared by every account + IPC path.
    state: Arc<dyn StateRepo>,
    /// Per-account orchestrator handles, keyed by [`AccountId`].
    ///
    /// A2: behind a sync [`std::sync::Mutex`] (never held across an await -
    /// only ever for a quick insert / clone-out) so the wizard can HOT-SPAWN a
    /// brand-new account's orchestrator mid-session (`finish_add_account` ->
    /// `spawn_account`) and `sync_now` finds it without a restart. Each handle
    /// is an [`Arc`] so a caller can clone it out and drive / await it after
    /// releasing the map lock.
    accounts: std::sync::Mutex<HashMap<AccountId, Arc<AccountHandle>>>,
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
    /// C1 (SPEC s11.6.1): one-shot dialog-token -> path bindings. The backend
    /// OWNS the native folder / save-file dialogs; each returns an opaque token
    /// bound to the path the USER actually chose. A path-bearing write command
    /// (`add_source`, `export_diagnostic_bundle`) must present a token that maps
    /// to exactly that path - so the (untrusted) webview can never inject an
    /// arbitrary path. Single-use (taken on validation) with a short TTL so a
    /// leaked token cannot be replayed later. Behind a sync `Mutex` (only ever
    /// held for a quick insert / take, never across an await).
    dialog_tokens: std::sync::Mutex<HashMap<String, DialogTokenBinding>>,
    /// R2-P1-1: per-account ASYNC lock serialising the FIRST-encrypted-source
    /// critical section (ensure-master-key -> stamp -> insert source). Without
    /// it two concurrent `add_source` calls on an account whose
    /// `encryption_master_key_id` is still NULL could BOTH generate DIFFERENT
    /// master keys into the same keychain slot and wrap different source keys -
    /// leaving one source permanently unrestorable. The lock (a `tokio::Mutex`
    /// so it can be held across the awaited DB write) makes the second add see
    /// the master key the first installed and wrap under the SAME key. Keyed by
    /// account; the inner map is behind a sync `Mutex` only to hand out the
    /// per-account `Arc<tokio::Mutex<()>>` (never held across an await).
    ensure_master_key_locks: std::sync::Mutex<HashMap<AccountId, Arc<Mutex<()>>>>,
    /// R2-P1-2: per-account in-memory fake remote store, shared between the
    /// Drive-folder picker (`pick_drive_folder`) and the orchestrator's
    /// uploader (assembly `build_remote`) so a folder id the picker mints in
    /// fake mode is visible to the uploader. Created on demand, one instance per
    /// account ([`InMemoryRemoteStore`] is `Clone` over a shared `Arc<Mutex>`, so
    /// every clone sees the same backing objects). Only ever populated in
    /// [`RemoteMode::Fake`]; in real mode it stays empty.
    fake_remote_stores: FakeRemoteStores,
    /// M8: live restore-job records, keyed by job id. Each entry carries the
    /// latest [`RestoreJobStatus`](crate::commands::dtos::RestoreJobStatus)
    /// snapshot (the background task writes it on every progress tick, so
    /// `get_restore_job` can serve a webview that subscribed late / missed an
    /// event), plus the per-job CANCEL control + spawned [`JoinHandle`] so
    /// `cancel_restore_job` and the app-shutdown drain can stop an in-flight job
    /// (M8-P1-1). Behind a sync `Mutex` (only ever held for a quick insert /
    /// clone-out / take, never across an await). Terminal entries are retained so
    /// a late poll still sees the result, but they are TTL-pruned + count-capped
    /// (M8-P2-3) so a long-running tray app does not leak snapshots forever.
    restore_jobs: std::sync::Mutex<HashMap<String, RestoreJobEntry>>,
    /// M9a (SPEC s15.2): the in-app updater runtime - the pending checked update
    /// (held so `install_update` stages + applies the SAME object the check
    /// found) plus the periodic-check task handle + shutdown signal, so the
    /// app-quit drain joins it with NO orphan (mirrors the M5 no-orphan
    /// bookkeeping).
    updater: UpdaterRuntime,
}

/// M9a (SPEC s15.2): the in-app updater runtime state held on [`AppState`].
///
/// `pending` holds the [`tauri_plugin_updater::Update`] the most recent check
/// found (manual or periodic), so `install_update` can `download_and_install`
/// the SAME object without re-resolving the manifest; it is TAKEN on install (a
/// fresh check re-populates it). `task` + `shutdown` track the single app-wide
/// periodic-check task so the quit drain stops + joins it with no orphan.
#[derive(Default)]
pub struct UpdaterRuntime {
    /// The update the latest check found, awaiting install; `None` when up to
    /// date / not yet checked / already consumed by an install.
    pending: std::sync::Mutex<Option<tauri_plugin_updater::Update>>,
    /// The spawned periodic-check task, behind `Option` so the shutdown drain
    /// can TAKE + await it by value; `None` once drained / never spawned.
    task: std::sync::Mutex<Option<JoinHandle<()>>>,
    /// The shutdown signal the periodic-check task `select!`s on, so it exits
    /// promptly on quit rather than waiting out its 6h interval.
    shutdown: std::sync::Mutex<Option<watch::Sender<bool>>>,
}

/// M8 (P2-3): max number of TERMINAL restore-job records retained for late
/// polling. Active (non-terminal) jobs are never evicted by the cap; only
/// finished ones are pruned once this many accumulate (oldest-terminal first).
const MAX_RETAINED_TERMINAL_JOBS: usize = 32;

/// M8 (P2-3): how long a TERMINAL restore-job record is retained for a late
/// `get_restore_job` poll before it is eligible for pruning. Generous (the
/// webview reconciles right after a job ends) but bounded so the map cannot grow
/// without limit across many restores in one long-lived session.
const TERMINAL_JOB_TTL: Duration = Duration::from_secs(3600);

/// M8 (P1-1): the per-job cancellation control shared between the spawned restore
/// task and the IPC / shutdown paths. A plain [`AtomicBool`] checked between
/// frames in the stream loop (no extra dependency): set once, observed
/// monotonically. Cloned (`Arc`) into the spawned task.
pub type RestoreCancel = Arc<AtomicBool>;

/// M8: one tracked restore job - its latest status snapshot, the instant it
/// reached a terminal state (for TTL pruning, `None` while running), the shared
/// cancel flag, and the spawned task handle (taken on cancel / shutdown so the
/// drain can await it).
struct RestoreJobEntry {
    /// The latest status snapshot served by `get_restore_job`.
    status: crate::commands::dtos::RestoreJobStatus,
    /// When the job reached a terminal state, for TTL pruning; `None` while it
    /// is still running.
    terminal_at: Option<Instant>,
    /// The shared cancel flag the spawned task observes between frames.
    cancel: RestoreCancel,
    /// The spawned job task, behind `Option` so the shutdown drain can TAKE +
    /// await it by value; `None` once the job finished or was drained.
    handle: std::sync::Mutex<Option<JoinHandle<()>>>,
}

/// R2-P1-2: the shared per-account fake-remote-store registry. An `Arc` so
/// assembly (which builds the orchestrator's store BEFORE [`AppState`] exists)
/// and [`AppState`] hold the SAME map - the orchestrator's fake store and the
/// picker's fake store are then guaranteed to be the same instance per account.
pub type FakeRemoteStores = Arc<std::sync::Mutex<HashMap<AccountId, InMemoryRemoteStore>>>;

/// R2-P1-2: get-or-create the fake remote store for `account` in `registry`.
/// A free function (not a method) so assembly's pre-[`AppState`] boot phase -
/// which builds the orchestrator's store before `AppState` exists - shares the
/// SAME registry the picker later reads via [`AppState::fake_remote_store`].
/// Returns a clone (the store wraps a shared `Arc<Mutex>`).
#[must_use]
pub fn fake_remote_store_in(
    registry: &FakeRemoteStores,
    account: AccountId,
) -> InMemoryRemoteStore {
    registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(account)
        .or_default()
        .clone()
}

/// M8 (P2-3): prune retained TERMINAL restore-job records so the map cannot grow
/// unbounded across many restores in one long-lived tray session. Two bounds:
/// 1. TTL: a terminal job older than [`TERMINAL_JOB_TTL`] is dropped.
/// 2. count cap: if more than [`MAX_RETAINED_TERMINAL_JOBS`] terminal jobs
///    remain, the OLDEST-terminal ones are dropped down to the cap.
///
/// Active (non-terminal) jobs are NEVER pruned - only finished ones - so an
/// in-flight job's status + cancel handle always survive.
fn prune_terminal_jobs(map: &mut HashMap<String, RestoreJobEntry>) {
    let now = Instant::now();
    // 1) TTL: drop terminal jobs older than the retention window.
    map.retain(|_, e| match e.terminal_at {
        Some(t) => now.duration_since(t) < TERMINAL_JOB_TTL,
        None => true,
    });
    // 2) count cap: if too many terminal jobs remain, drop the oldest.
    let mut terminal: Vec<(String, Instant)> = map
        .iter()
        .filter_map(|(id, e)| e.terminal_at.map(|t| (id.clone(), t)))
        .collect();
    if terminal.len() > MAX_RETAINED_TERMINAL_JOBS {
        terminal.sort_by_key(|(_, t)| *t);
        let drop_n = terminal.len() - MAX_RETAINED_TERMINAL_JOBS;
        for (id, _) in terminal.into_iter().take(drop_n) {
            map.remove(&id);
        }
    }
}

/// C1: one backend-minted dialog-token binding - the path the user chose via a
/// native dialog plus the instant it was minted (for the short single-use TTL).
struct DialogTokenBinding {
    /// The path the native dialog returned (a folder for the folder dialog, a
    /// concrete file path for the save dialog).
    path: std::path::PathBuf,
    /// When the token was minted (for TTL expiry).
    minted_at: std::time::Instant,
}

/// C1: how long a backend-minted dialog token stays valid. The webview calls the
/// dialog command then immediately calls the write command with the token, so a
/// few minutes is generous; a token older than this is rejected so a leaked one
/// cannot be replayed much later.
const DIALOG_TOKEN_TTL: Duration = Duration::from_secs(300);

impl AppState {
    /// Build the managed state from the state repo, the per-account handles,
    /// the remote-construction mode, and the shared fake-remote-store registry
    /// (called by `assembly::build_and_spawn`). The `fake_remote_stores` map is
    /// the SAME one assembly threaded into `build_remote`, so the orchestrator's
    /// fake store and the picker's fake store are one instance per account
    /// (R2-P1-2).
    #[must_use]
    pub fn new(
        state: Arc<dyn StateRepo>,
        accounts: HashMap<AccountId, AccountHandle>,
        remote_mode: RemoteMode,
        fake_remote_stores: FakeRemoteStores,
    ) -> Self {
        let accounts = accounts
            .into_iter()
            .map(|(id, handle)| (id, Arc::new(handle)))
            .collect();
        Self {
            state,
            accounts: std::sync::Mutex::new(accounts),
            remote_mode,
            pause_generations: std::sync::Mutex::new(HashMap::new()),
            dialog_tokens: std::sync::Mutex::new(HashMap::new()),
            ensure_master_key_locks: std::sync::Mutex::new(HashMap::new()),
            fake_remote_stores,
            restore_jobs: std::sync::Mutex::new(HashMap::new()),
            updater: UpdaterRuntime::default(),
        }
    }

    // --- M9a updater runtime (SPEC s15.2) ----------------------------------

    /// M9a: record the [`tauri_plugin_updater::Update`] a check found, so a
    /// subsequent `install_update` stages + applies the SAME object without
    /// re-resolving the manifest. Overwrites any prior pending update (a newer
    /// check supersedes an older one). `None` clears it.
    pub fn set_pending_update(&self, update: Option<tauri_plugin_updater::Update>) {
        *self
            .updater
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = update;
    }

    /// M9a: TAKE (single-use) the pending update for installation. `None` when no
    /// check has found an update (so `install_update` returns a clear "nothing to
    /// install" error rather than guessing).
    #[must_use]
    pub fn take_pending_update(&self) -> Option<tauri_plugin_updater::Update> {
        self.updater
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// M9a: register the spawned periodic-check task + its shutdown sender so the
    /// app-quit drain can stop + join it with no orphan. Called once after the
    /// updater task is spawned in `lib.rs` setup.
    pub fn set_updater_task(&self, task: JoinHandle<()>, shutdown: watch::Sender<bool>) {
        *self.updater.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        *self
            .updater
            .shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(shutdown);
    }

    /// M9a: signal the periodic-check task to stop and TAKE its handle so the
    /// caller can await it (the app-quit drain). Returns `None` if no task is
    /// tracked (never spawned / already drained). Mirrors the M8 restore-job
    /// no-orphan take pattern.
    #[must_use]
    pub fn shutdown_updater_task(&self) -> Option<JoinHandle<()>> {
        if let Some(tx) = self
            .updater
            .shutdown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            // A send error only means the receiver is already gone - benign.
            let _ = tx.send(true);
        }
        self.updater
            .task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Lock the restore-jobs map, recovering a poisoned lock (house rule: never
    /// panic on a poisoned lock).
    fn lock_restore_jobs(&self) -> std::sync::MutexGuard<'_, HashMap<String, RestoreJobEntry>> {
        self.restore_jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// M8 (P1-1; R3-P1-2): SEED a restore job - its initial status snapshot, shared
    /// cancel flag, AND its already-spawned task [`JoinHandle`] - in ONE locked
    /// insert. Registering the handle ATOMICALLY with the seed (rather than seeding
    /// first and attaching the handle in a separate call) closes the R3-P1-2 race:
    /// `cancel_all_restore_jobs` (app quit) can NEVER observe a seeded restore job
    /// that lacks an awaitable handle. The command spawns the task behind a START
    /// BARRIER (the task does no filesystem work until released), seeds it here with
    /// its handle, THEN releases the barrier - so a quit anywhere in the spawn
    /// window finds a drainable handle and the task creates no partial temp.
    /// `get_restore_job` can serve the job immediately and `cancel_restore_job`
    /// works the moment the job is seeded. Also opportunistically prunes terminal
    /// jobs (P2-3).
    pub fn seed_restore_job(
        &self,
        status: crate::commands::dtos::RestoreJobStatus,
        cancel: RestoreCancel,
        handle: JoinHandle<()>,
    ) {
        let mut map = self.lock_restore_jobs();
        map.insert(
            status.job_id.clone(),
            RestoreJobEntry {
                status,
                terminal_at: None,
                cancel,
                handle: std::sync::Mutex::new(Some(handle)),
            },
        );
        prune_terminal_jobs(&mut map);
    }

    /// M8: record the latest status snapshot for a restore job (the background
    /// task calls this on every progress tick). `get_restore_job` reads it back.
    /// A terminal (`done`) snapshot stamps the terminal time so the entry becomes
    /// eligible for TTL pruning (P2-3). Updating an unknown id (e.g. a job whose
    /// terminal record was already pruned) is a benign no-op.
    pub fn put_restore_job(&self, status: crate::commands::dtos::RestoreJobStatus) {
        let mut map = self.lock_restore_jobs();
        let done = status.done;
        if let Some(entry) = map.get_mut(&status.job_id) {
            entry.status = status;
            if done && entry.terminal_at.is_none() {
                entry.terminal_at = Some(Instant::now());
            }
        }
        prune_terminal_jobs(&mut map);
    }

    /// M8: the current status snapshot for `job_id`, if the job exists (live or
    /// terminal). `None` for an unknown / forged id so `get_restore_job` surfaces
    /// an error rather than fabricating a status.
    #[must_use]
    pub fn restore_job(&self, job_id: &str) -> Option<crate::commands::dtos::RestoreJobStatus> {
        self.lock_restore_jobs()
            .get(job_id)
            .map(|e| e.status.clone())
    }

    /// M8 (R1-P1-2): request cancellation of a running restore job FROM THE UI
    /// WITHOUT detaching it. Sets the shared cancel flag (the spawned task observes
    /// it between frames, deletes any in-flight temp, and emits a terminal
    /// CANCELLED status) but LEAVES the task handle tracked on the job entry, so
    /// the M5-style app-shutdown drain ([`Self::cancel_all_restore_jobs`]) still
    /// awaits/aborts it - a UI cancel never orphans the task. The task clears its
    /// own handle on exit via [`Self::finish_restore_job_handle`]. Returns `true`
    /// if a tracked job's flag was set, `false` for an unknown / already-finished
    /// id (cancellation is idempotent).
    pub fn signal_cancel_restore_job(&self, job_id: &str) -> bool {
        let map = self.lock_restore_jobs();
        match map.get(job_id) {
            Some(entry) => {
                entry.cancel.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// M8 (P1-1): request cancellation of a running restore job AND TAKE its task
    /// handle. Sets the shared cancel flag (the spawned task observes it between
    /// frames, deletes any in-flight temp, and emits a terminal CANCELLED status)
    /// and returns the spawned [`JoinHandle`] if the job is still tracked +
    /// running, so the CALLER can await it. `None` for an unknown / already-
    /// finished id - cancellation is idempotent (a second call, or a call after
    /// the job ended, is a no-op).
    ///
    /// R1-P1-2: this is reserved for callers that take ownership of draining the
    /// handle (e.g. a dedicated shutdown sweep). The UI cancel path must use
    /// [`Self::signal_cancel_restore_job`] instead so the handle stays tracked for
    /// the shutdown drain - taking + dropping the handle would DETACH the task.
    #[must_use]
    pub fn cancel_restore_job(&self, job_id: &str) -> Option<JoinHandle<()>> {
        let map = self.lock_restore_jobs();
        let entry = map.get(job_id)?;
        entry.cancel.store(true, Ordering::SeqCst);
        let handle = entry
            .handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        handle
    }

    /// M8 (P1-1): mark a job's handle as drained (the spawned task finished and
    /// took its own handle). Called by the job task on its way out so the shutdown
    /// drain does not try to await an already-finished handle.
    pub fn finish_restore_job_handle(&self, job_id: &str) {
        if let Some(entry) = self.lock_restore_jobs().get(job_id) {
            let _ = entry
                .handle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        }
    }

    /// Test-only: number of restore-job records currently tracked (live +
    /// terminal). R2-P2-1 uses this to assert that a `restore_files` whose fallible
    /// setup fails leaves NO lingering job entry.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn restore_jobs_len(&self) -> usize {
        self.lock_restore_jobs().len()
    }

    /// M8 (P1-1): signal-cancel EVERY in-flight restore job and TAKE their task
    /// handles, so the app-shutdown drain can await them (mirrors the M5 no-orphan
    /// AccountHandle drain). Sets each job's cancel flag, so each task deletes its
    /// in-flight temp + emits a terminal CANCELLED status before returning, then
    /// returns the handles for the caller to `join` so quit leaves no orphaned
    /// restore task and no partial files.
    #[must_use]
    pub fn cancel_all_restore_jobs(&self) -> Vec<JoinHandle<()>> {
        let map = self.lock_restore_jobs();
        let mut handles = Vec::new();
        for entry in map.values() {
            entry.cancel.store(true, Ordering::SeqCst);
            if let Some(h) = entry
                .handle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                handles.push(h);
            }
        }
        handles
    }

    /// C1: mint a one-shot dialog token bound to `path` (the path a native
    /// backend dialog returned), returning the opaque token string the webview
    /// threads back into the write command. Also opportunistically evicts
    /// expired bindings so the map cannot grow unbounded.
    pub fn mint_dialog_token(&self, path: std::path::PathBuf) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let mut map = self.lock_dialog_tokens();
        let now = std::time::Instant::now();
        map.retain(|_, b| now.duration_since(b.minted_at) < DIALOG_TOKEN_TTL);
        map.insert(
            token.clone(),
            DialogTokenBinding {
                path,
                minted_at: now,
            },
        );
        token
    }

    /// C1: TAKE (single-use) the path bound to `token`, if it exists and has not
    /// expired. Returns `None` for an unknown / already-consumed / expired token
    /// so a path-bearing command can REJECT a path the user did not pick via a
    /// backend dialog. Consuming the token here makes it single-use.
    pub fn take_dialog_token(&self, token: &str) -> Option<std::path::PathBuf> {
        let mut map = self.lock_dialog_tokens();
        let binding = map.remove(token)?;
        if std::time::Instant::now().duration_since(binding.minted_at) >= DIALOG_TOKEN_TTL {
            return None;
        }
        Some(binding.path)
    }

    /// C1 / R1-P1-2: PEEK (non-consuming) the path bound to `token`, if it
    /// exists and has not expired. Unlike [`Self::take_dialog_token`] this does
    /// NOT consume the token, so a read-only, idempotent, repeatable command
    /// (`preview_exclusions`, which the user re-runs as they tweak globs) can
    /// resolve the dialog-derived path without spending the single use the
    /// subsequent `add_source` write needs. The TTL still bounds replay; only a
    /// path-bearing WRITE consumes the token.
    pub fn peek_dialog_token(&self, token: &str) -> Option<std::path::PathBuf> {
        let map = self.lock_dialog_tokens();
        let binding = map.get(token)?;
        if std::time::Instant::now().duration_since(binding.minted_at) >= DIALOG_TOKEN_TTL {
            return None;
        }
        Some(binding.path.clone())
    }

    /// Lock the dialog-token map, recovering a poisoned lock.
    fn lock_dialog_tokens(&self) -> std::sync::MutexGuard<'_, HashMap<String, DialogTokenBinding>> {
        self.dialog_tokens.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Lock the accounts map, recovering a poisoned lock (house rule: never
    /// panic on a poisoned lock).
    fn lock_accounts(&self) -> std::sync::MutexGuard<'_, HashMap<AccountId, Arc<AccountHandle>>> {
        self.accounts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A2: insert (or replace) the live handle for `account`, returning the
    /// PRIOR handle if one was present (so the caller can shut it down to avoid
    /// orphaning its tasks - mirrors the M5 no-orphan bookkeeping). Called by
    /// the wizard hot-spawn path after `finish_add_account`.
    pub fn insert_account(
        &self,
        account: AccountId,
        handle: AccountHandle,
    ) -> Option<Arc<AccountHandle>> {
        self.lock_accounts().insert(account, Arc::new(handle))
    }

    /// A2: remove + return the live handle for `account` (so the caller can
    /// shut it down before deleting the account's rows). `None` if the account
    /// has no running orchestrator (never spawned / needs_reauth).
    pub fn remove_account_handle(&self, account: AccountId) -> Option<Arc<AccountHandle>> {
        self.lock_accounts().remove(&account)
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

    /// The orchestrator handle for `account`, if present. Returns a cloned
    /// [`Arc`] so the caller can drive / await it after the map lock is
    /// released (A2: the map is now behind a sync mutex).
    #[must_use]
    pub fn account(&self, account: AccountId) -> Option<Arc<AccountHandle>> {
        self.lock_accounts().get(&account).cloned()
    }

    /// Snapshot every account's orchestrator handle (for "all accounts"
    /// commands like `sync_now(None)` / `pause_sync`). Returns owned
    /// `(AccountId, Arc<AccountHandle>)` pairs so callers can iterate + await
    /// without holding the map lock across an await (A2).
    #[must_use]
    pub fn accounts(&self) -> Vec<(AccountId, Arc<AccountHandle>)> {
        self.lock_accounts()
            .iter()
            .map(|(id, handle)| (*id, handle.clone()))
            .collect()
    }

    /// How remotes were constructed this run.
    #[must_use]
    pub fn remote_mode(&self) -> RemoteMode {
        self.remote_mode
    }

    /// R2-P1-1: the per-account ensure-master-key async lock (get-or-create).
    /// `add_source` holds this across the ENTIRE first-encrypted-source critical
    /// section (prepare master key -> stamp -> insert) so two concurrent adds
    /// serialise and the second wraps under the master key the first installed.
    /// Returns a cloned `Arc` so the caller can `.lock().await` after releasing
    /// the (sync) map lock.
    #[must_use]
    pub fn ensure_master_key_lock(&self, account: AccountId) -> Arc<Mutex<()>> {
        self.ensure_master_key_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(account)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// R2-P1-2: the per-account fake remote store (get-or-create). Shared
    /// between the Drive-folder picker and the orchestrator's uploader so a
    /// folder id minted by the picker in fake mode is visible to the uploader.
    /// Returns a clone (the store wraps a shared `Arc<Mutex>`, so the clone sees
    /// the same backing objects). Only meaningful in [`RemoteMode::Fake`].
    #[must_use]
    pub fn fake_remote_store(&self, account: AccountId) -> InMemoryRemoteStore {
        fake_remote_store_in(&self.fake_remote_stores, account)
    }

    /// R2-P1-2: a clone of the shared fake-remote-store registry handle, so the
    /// wizard hot-spawn path (`assembly::spawn_account`) builds the new account's
    /// orchestrator store FROM the same registry the picker reads.
    #[must_use]
    pub fn fake_remote_stores(&self) -> FakeRemoteStores {
        self.fake_remote_stores.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_core::orchestrator::{Orchestrator, OrchestratorConfig, TickSource};
    use driven_core::types::OrchestratorState;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A no-source crypto provider for the shutdown tests (B2: `AccountTasks`
    /// now carries the live crypto provider; these tests only exercise the
    /// task-drain bookkeeping, so an empty provider suffices).
    fn test_crypto() -> Arc<crate::crypto_provider_impl::KeystoreCryptoProvider> {
        Arc::new(crate::crypto_provider_impl::KeystoreCryptoProvider::new(
            AccountId::new_v4(),
            Vec::new(),
        ))
    }

    /// A no-op [`Orchestrator`] whose `run()` returns as soon as `shutdown()` is
    /// signalled - mirroring the real run loop's graceful-drain contract, so the
    /// R-P1-1 shutdown test exercises the clean-join path for the run loop while
    /// the poller (which loops forever) exercises the abort-and-await path.
    struct FakeOrchestrator {
        shutdown: watch::Sender<bool>,
        shutdown_rx: watch::Receiver<bool>,
    }

    impl FakeOrchestrator {
        fn new() -> Self {
            let (shutdown, shutdown_rx) = watch::channel(false);
            Self {
                shutdown,
                shutdown_rx,
            }
        }
    }

    #[async_trait::async_trait]
    impl Orchestrator for FakeOrchestrator {
        async fn run(&self) -> anyhow::Result<()> {
            let mut rx = self.shutdown_rx.clone();
            loop {
                if *rx.borrow() {
                    return Ok(());
                }
                if rx.changed().await.is_err() {
                    return Ok(());
                }
            }
        }
        async fn trigger(&self, _reason: TickSource) {}
        async fn set_paused(&self, _paused: bool) {}
        async fn state(&self) -> OrchestratorState {
            OrchestratorState::Idle { last_run_at: None }
        }
        async fn reconfigure(&self, _config: OrchestratorConfig) {}
        fn shutdown(&self) {
            let _ = self.shutdown.send(true);
        }
    }

    #[tokio::test]
    async fn shutdown_joins_every_per_account_task_no_orphans() {
        // R-P1-1: `AccountHandle::shutdown` must leave ZERO orphaned tasks - the
        // run loop, watcher bridge, event bridge, AND the forever-looping power
        // poller must all be finished (joined or aborted-and-awaited) when it
        // returns. This is the M5 "no orphaned tokio tasks" acceptance, modelled
        // on the EXACT task shapes assembly spawns.
        let orchestrator: Arc<dyn Orchestrator> = Arc::new(FakeOrchestrator::new());

        // The bridge shutdown signal both bridges select! on (the watcher bridge
        // never closes on its own; the poller loops forever).
        let (bridge_shutdown, _rx0) = watch::channel(false);

        // Watcher bridge: an mpsc whose Sender we KEEP (modelling NotifyWatcher
        // owning the sender), so recv() never returns None on its own - the
        // bridge can only end via the shutdown signal.
        let (_watch_tx, mut watch_rx) = tokio::sync::mpsc::channel::<u32>(4);
        let watcher_bridge = {
            let mut shutdown = bridge_shutdown.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = watch_rx.recv() => {}
                        res = shutdown.changed() => {
                            match res {
                                Ok(()) if *shutdown.borrow() => break,
                                Ok(()) => {}
                                Err(_) => break,
                            }
                        }
                    }
                }
            })
        };

        // Event bridge: a broadcast whose Sender we KEEP, so recv() blocks
        // indefinitely - it can only end via the shutdown signal.
        let (_evt_tx, mut evt_rx) = tokio::sync::broadcast::channel::<u32>(4);
        let event_bridge = {
            let mut shutdown = bridge_shutdown.subscribe();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        res = shutdown.changed() => {
                            match res {
                                Ok(()) if *shutdown.borrow() => break,
                                Ok(()) => {}
                                Err(_) => break,
                            }
                        }
                        _ = evt_rx.recv() => {}
                    }
                }
            })
        };

        // Power poller: loops FOREVER with no shutdown path (exactly the real
        // `RealPowerSource::spawn_poller` shape) - only abort can stop it. A flag
        // proves it actually ran (and was not a no-op) before being aborted.
        let poller_ran = Arc::new(AtomicBool::new(false));
        let power_poller = {
            let poller_ran = poller_ran.clone();
            tokio::spawn(async move {
                poller_ran.store(true, Ordering::SeqCst);
                let mut ticker = tokio::time::interval(Duration::from_millis(10));
                loop {
                    ticker.tick().await;
                }
            })
        };

        // Capture abort handles BEFORE moving the JoinHandles into the handle, so
        // the test can independently assert each task is finished afterwards.
        let run_loop = {
            let orch = orchestrator.clone();
            tokio::spawn(async move {
                let _ = orch.run().await;
            })
        };
        let run_loop_abort = run_loop.abort_handle();
        let watcher_abort = watcher_bridge.abort_handle();
        let event_abort = event_bridge.abort_handle();
        let poller_abort = power_poller.abort_handle();

        let handle = AccountHandle::new(
            orchestrator,
            AccountTasks {
                crypto: test_crypto(),
                run_loop,
                watcher_bridge: Some(watcher_bridge),
                event_bridge,
                power_poller,
                bridge_shutdown,
            },
        );

        // Let the poller actually start before we shut down.
        tokio::time::sleep(Duration::from_millis(30)).await;

        // The whole shutdown must complete well within the per-task budgets (the
        // run loop + bridges drain cleanly; the poller is aborted). Bound it so a
        // regression (a task that never stops) fails instead of hanging.
        tokio::time::timeout(Duration::from_secs(10), handle.shutdown())
            .await
            .expect("shutdown must complete (no task left orphaned)");

        assert!(
            poller_ran.load(Ordering::SeqCst),
            "the power poller task must have actually started"
        );

        // EVERY task is now finished (cleanly joined or aborted-and-awaited).
        assert!(run_loop_abort.is_finished(), "run loop must be finished");
        assert!(
            watcher_abort.is_finished(),
            "watcher bridge must be finished"
        );
        assert!(event_abort.is_finished(), "event bridge must be finished");
        assert!(
            poller_abort.is_finished(),
            "power poller must be finished (aborted, not orphaned)"
        );

        // Idempotent: a second shutdown is a no-op and does not panic / hang.
        tokio::time::timeout(Duration::from_secs(2), handle.shutdown())
            .await
            .expect("second shutdown is an immediate no-op");
    }

    /// An [`Orchestrator`] whose `run()` keeps working for `drain` AFTER the
    /// shutdown signal (modelling an in-flight upload that needs time to finish
    /// the current cycle), setting `finished_cleanly` only when it returns of
    /// its OWN accord (never if it was aborted mid-sleep).
    struct SlowDrainOrchestrator {
        shutdown: watch::Sender<bool>,
        shutdown_rx: watch::Receiver<bool>,
        drain: Duration,
        finished_cleanly: Arc<AtomicBool>,
    }

    impl SlowDrainOrchestrator {
        fn new(drain: Duration, finished_cleanly: Arc<AtomicBool>) -> Self {
            let (shutdown, shutdown_rx) = watch::channel(false);
            Self {
                shutdown,
                shutdown_rx,
                drain,
                finished_cleanly,
            }
        }
    }

    #[async_trait::async_trait]
    impl Orchestrator for SlowDrainOrchestrator {
        async fn run(&self) -> anyhow::Result<()> {
            let mut rx = self.shutdown_rx.clone();
            // Wait for the shutdown signal, then "finish the in-flight cycle"
            // (sleep `drain`) before returning. If aborted during the sleep,
            // `finished_cleanly` stays false.
            while !*rx.borrow() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
            tokio::time::sleep(self.drain).await;
            self.finished_cleanly.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn trigger(&self, _reason: TickSource) {}
        async fn set_paused(&self, _paused: bool) {}
        async fn state(&self) -> OrchestratorState {
            OrchestratorState::Idle { last_run_at: None }
        }
        async fn reconfigure(&self, _config: OrchestratorConfig) {}
        fn shutdown(&self) {
            let _ = self.shutdown.send(true);
        }
    }

    /// R2-P2-1: a run loop that needs LONGER than the short 5s
    /// [`TASK_DRAIN_TIMEOUT`] to finish its in-flight cycle - but less than the
    /// full [`RUN_LOOP_DRAIN_TIMEOUT`] - must drain CLEANLY (DESIGN s5.10.2),
    /// NOT be aborted at the bridge budget. Uses paused time so the long drain
    /// is instant.
    #[tokio::test]
    async fn run_loop_gets_full_drain_budget_not_the_short_bridge_timeout() {
        // Paused virtual time so the long in-flight drain is instant (no real
        // multi-second sleep). `tokio::time::pause` auto-advances when every
        // task is waiting on a timer.
        tokio::time::pause();
        // The in-flight cycle takes ~12s to wind down: well past the 5s short
        // budget, comfortably inside the 20s run-loop budget.
        let drain = Duration::from_secs(12);
        assert!(
            drain > TASK_DRAIN_TIMEOUT && drain < RUN_LOOP_DRAIN_TIMEOUT,
            "test fixture must straddle the two budgets"
        );

        let finished_cleanly = Arc::new(AtomicBool::new(false));
        let orchestrator: Arc<dyn Orchestrator> =
            Arc::new(SlowDrainOrchestrator::new(drain, finished_cleanly.clone()));

        let (bridge_shutdown, _rx0) = watch::channel(false);
        let run_loop = {
            let orch = orchestrator.clone();
            tokio::spawn(async move {
                let _ = orch.run().await;
            })
        };
        let run_loop_abort = run_loop.abort_handle();

        // No watcher/event/poller for this focused test: the run loop is what
        // matters. `None` slots are drained as no-ops.
        let (_evt_tx, evt_rx) = tokio::sync::broadcast::channel::<u32>(4);
        drop(evt_rx);
        let event_bridge = tokio::spawn(async {});
        let power_poller = tokio::spawn(async {});

        let handle = AccountHandle::new(
            orchestrator,
            AccountTasks {
                crypto: test_crypto(),
                run_loop,
                watcher_bridge: None,
                event_bridge,
                power_poller,
                bridge_shutdown,
            },
        );

        // Bound by the full run-loop budget + a margin: the run loop must finish
        // by draining cleanly (not by being aborted at 5s).
        tokio::time::timeout(
            RUN_LOOP_DRAIN_TIMEOUT + Duration::from_secs(5),
            handle.shutdown(),
        )
        .await
        .expect("shutdown must complete within the full run-loop budget");

        assert!(
            finished_cleanly.load(Ordering::SeqCst),
            "the run loop must drain CLEANLY (finish its in-flight cycle), not be aborted at the 5s bridge budget (R2-P2-1)"
        );
        assert!(run_loop_abort.is_finished(), "run loop must be finished");
    }

    /// One account's tracked task set plus the abort handles needed to assert
    /// every task finished after shutdown - built to the EXACT shapes assembly
    /// spawns (slow-draining run loop + forever-looping power poller).
    struct BuiltAccount {
        handle: AccountHandle,
        run_loop_abort: tokio::task::AbortHandle,
        poller_abort: tokio::task::AbortHandle,
    }

    /// Build a slow account: a run loop that needs `drain` to wind down after the
    /// stop signal (modelling a large in-flight upload) and a power poller that
    /// loops forever (only abort can stop it). Mirrors the per-account task set
    /// in `assembly::build_account` so the R3-P1-1 concurrent-drain test is real.
    fn build_slow_account(drain: Duration) -> BuiltAccount {
        let finished_cleanly = Arc::new(AtomicBool::new(false));
        let orchestrator: Arc<dyn Orchestrator> =
            Arc::new(SlowDrainOrchestrator::new(drain, finished_cleanly));
        let (bridge_shutdown, _rx0) = watch::channel(false);

        let run_loop = {
            let orch = orchestrator.clone();
            tokio::spawn(async move {
                let _ = orch.run().await;
            })
        };
        let run_loop_abort = run_loop.abort_handle();

        // Power poller: loops FOREVER with no shutdown path (only abort stops it).
        let power_poller = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(10));
            loop {
                ticker.tick().await;
            }
        });
        let poller_abort = power_poller.abort_handle();

        let event_bridge = tokio::spawn(async {});

        let handle = AccountHandle::new(
            orchestrator,
            AccountTasks {
                crypto: test_crypto(),
                run_loop,
                watcher_bridge: None,
                event_bridge,
                power_poller,
                bridge_shutdown,
            },
        );
        BuiltAccount {
            handle,
            run_loop_abort,
            poller_abort,
        }
    }

    /// R3-P1-1: shutting down MULTIPLE accounts must run their `shutdown()`
    /// futures CONCURRENTLY (as `lib.rs::shutdown_orchestrators` now does via
    /// `futures::future::join_all`) and leave ZERO orphaned tasks - even when
    /// every run loop is slow AND every poller would run forever. The bug this
    /// guards: a serial drain under one outer timeout could fire MID-drain and
    /// drop a cancellation-unsafe `drain_or_abort` (whose `JoinHandle` is already
    /// taken), detaching the aborted task. Uses paused virtual time so the long
    /// per-account drains are instant, and asserts the WHOLE concurrent sweep
    /// finishes well under the SUM of the two accounts' budgets.
    #[tokio::test]
    async fn concurrent_shutdown_of_multiple_slow_accounts_leaves_no_orphans() {
        tokio::time::pause();
        // Each run loop needs ~15s to wind down (inside the 20s run-loop budget),
        // and each poller loops forever (always aborted). Two accounts.
        let drain = Duration::from_secs(15);
        assert!(
            drain < RUN_LOOP_DRAIN_TIMEOUT,
            "each account's drain must fit inside its own run-loop budget"
        );

        let a = build_slow_account(drain);
        let b = build_slow_account(drain);

        // Let both pollers actually start before shutting down.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Drive BOTH accounts' shutdown CONCURRENTLY, exactly like the quit sweep.
        // If they ran serially the elapsed virtual time would be ~2x the run-loop
        // drain; concurrently it is ~1x. Bound the whole sweep by ONE run-loop
        // budget + margin (which is LESS than two serial drains would need), so a
        // regression to serial draining fails this timeout.
        tokio::time::timeout(
            RUN_LOOP_DRAIN_TIMEOUT + Duration::from_secs(5),
            futures::future::join_all([a.handle.shutdown(), b.handle.shutdown()]),
        )
        .await
        .expect(
            "concurrent multi-account shutdown must complete (no task orphaned, drains overlap)",
        );

        // EVERY task across BOTH accounts is finished (joined or aborted-and-
        // awaited) - no orphans.
        assert!(
            a.run_loop_abort.is_finished(),
            "account A run loop must be finished"
        );
        assert!(
            a.poller_abort.is_finished(),
            "account A poller must be finished (aborted, not orphaned)"
        );
        assert!(
            b.run_loop_abort.is_finished(),
            "account B run loop must be finished"
        );
        assert!(
            b.poller_abort.is_finished(),
            "account B poller must be finished (aborted, not orphaned)"
        );
    }

    /// Open a temp-backed state repo for the AppState dialog-token tests.
    async fn temp_state() -> (Arc<dyn StateRepo>, std::path::PathBuf) {
        use driven_core::state::sqlite::SqliteStateRepo;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("driven-appstate-test-{nonce}-{:p}", &nonce));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let repo = SqliteStateRepo::open(&dir.join("state.db"))
            .await
            .expect("open state repo");
        (Arc::new(repo), dir)
    }

    /// An empty fake-remote-store registry for the AppState tests.
    fn default_fake_registry() -> super::FakeRemoteStores {
        Arc::new(std::sync::Mutex::new(HashMap::new()))
    }

    #[tokio::test]
    async fn dialog_token_round_trips_and_is_single_use() {
        // C1 (SPEC s11.6.1): a minted token maps to its path exactly once; a
        // second take (or an unknown token) returns None so a leaked / replayed
        // token cannot authorise a second write.
        let (state, dir) = temp_state().await;
        let app_state = AppState::new(
            state,
            HashMap::new(),
            RemoteMode::Fake,
            default_fake_registry(),
        );
        let path = std::path::PathBuf::from("/home/u/backups");
        let token = app_state.mint_dialog_token(path.clone());

        // First take resolves the bound path.
        assert_eq!(app_state.take_dialog_token(&token), Some(path));
        // Single-use: a second take is rejected.
        assert_eq!(app_state.take_dialog_token(&token), None);
        // An unknown token is rejected.
        assert_eq!(app_state.take_dialog_token("not-a-real-token"), None);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn peek_dialog_token_is_non_consuming() {
        // R1-P1-2: `preview_exclusions` PEEKS the dialog token (non-consuming) so
        // the user can re-run the preview as they tweak globs AND the subsequent
        // `add_source` still has the single TAKE it needs. Peeking N times then
        // taking once must all resolve the same path; a take after that is
        // rejected.
        let (state, dir) = temp_state().await;
        let app_state = AppState::new(
            state,
            HashMap::new(),
            RemoteMode::Fake,
            default_fake_registry(),
        );
        let path = std::path::PathBuf::from("/home/u/preview-root");
        let token = app_state.mint_dialog_token(path.clone());

        // Multiple peeks all resolve the path WITHOUT consuming the token.
        assert_eq!(app_state.peek_dialog_token(&token), Some(path.clone()));
        assert_eq!(app_state.peek_dialog_token(&token), Some(path.clone()));
        // The single TAKE (what add_source uses) still works after the peeks.
        assert_eq!(app_state.take_dialog_token(&token), Some(path));
        // Now consumed: a further peek AND take both return None.
        assert_eq!(app_state.peek_dialog_token(&token), None);
        assert_eq!(app_state.take_dialog_token(&token), None);
        // An unknown token never resolves.
        assert_eq!(app_state.peek_dialog_token("nope"), None);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn ensure_master_key_lock_is_shared_per_account_and_serialises() {
        // R2-P1-1: two `add_source` calls for the SAME account must get the SAME
        // lock (so they serialise); different accounts get DISTINCT locks (so they
        // do not block each other). Also prove the lock actually serialises a
        // critical section (no overlap).
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (state, dir) = temp_state().await;
        let app_state = Arc::new(AppState::new(
            state,
            HashMap::new(),
            RemoteMode::Fake,
            default_fake_registry(),
        ));
        let acct_a = AccountId::new_v4();
        let acct_b = AccountId::new_v4();

        // Same account -> same Arc; different account -> different Arc.
        let l1 = app_state.ensure_master_key_lock(acct_a);
        let l2 = app_state.ensure_master_key_lock(acct_a);
        let l3 = app_state.ensure_master_key_lock(acct_b);
        assert!(Arc::ptr_eq(&l1, &l2), "same account shares one lock");
        assert!(
            !Arc::ptr_eq(&l1, &l3),
            "different accounts get distinct locks"
        );

        // Serialisation: two tasks contend on acct_a's lock; the in-section
        // counter must never exceed 1 (no overlap).
        let max_in_section = Arc::new(AtomicUsize::new(0));
        let in_section = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let app_state = app_state.clone();
            let in_section = in_section.clone();
            let max_in_section = max_in_section.clone();
            handles.push(tokio::spawn(async move {
                let lock = app_state.ensure_master_key_lock(acct_a);
                let _guard = lock.lock().await;
                let now = in_section.fetch_add(1, Ordering::SeqCst) + 1;
                max_in_section.fetch_max(now, Ordering::SeqCst);
                // Yield so an overlapping task would be observed if the lock
                // failed to serialise.
                tokio::task::yield_now().await;
                in_section.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(
            max_in_section.load(Ordering::SeqCst),
            1,
            "the per-account lock must serialise the critical section (no overlap)"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Build a minimal terminal/active restore status for the job tests.
    fn restore_status(job_id: &str, done: bool) -> crate::commands::dtos::RestoreJobStatus {
        crate::commands::dtos::RestoreJobStatus {
            job_id: job_id.to_string(),
            total_files: 1,
            completed_files: 0,
            failed_files: 0,
            total_bytes: 0,
            bytes_done: 0,
            current_file: None,
            done,
            cancelled: false,
            files: Vec::new(),
        }
    }

    #[tokio::test]
    async fn cancel_restore_job_sets_flag_and_returns_handle_and_drains() {
        // M8-P1-1: cancelling a registered job sets its cancel flag (the task
        // observes it + exits), returns the task handle so the caller can await
        // it, and is idempotent for an unknown / already-cancelled id.
        let (state, dir) = temp_state().await;
        let app_state = AppState::new(
            state,
            HashMap::new(),
            RemoteMode::Fake,
            default_fake_registry(),
        );

        let cancel: RestoreCancel = Arc::new(AtomicBool::new(false));
        let observed_cancel = cancel.clone();
        // A task that loops until the cancel flag is set (models the stream loop).
        let handle = tokio::spawn(async move {
            loop {
                if observed_cancel.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        app_state.seed_restore_job(restore_status("job-1", false), cancel.clone(), handle);

        // Cancel: flag set + handle returned.
        let returned = app_state.cancel_restore_job("job-1");
        assert!(cancel.load(Ordering::SeqCst), "cancel flag must be set");
        let returned = returned.expect("a running job returns its handle");
        // The task observes the flag and exits; awaiting it completes.
        tokio::time::timeout(Duration::from_secs(2), returned)
            .await
            .expect("cancelled task must finish")
            .expect("task joined cleanly");

        // Idempotent: a second cancel (handle already taken) returns None.
        assert!(app_state.cancel_restore_job("job-1").is_none());
        // Unknown id: None.
        assert!(app_state.cancel_restore_job("nope").is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn ui_cancel_sets_flag_but_keeps_handle_for_shutdown_drain() {
        // R1-P1-2: the UI cancel path (`signal_cancel_restore_job`) must ONLY set
        // the cancel flag and LEAVE the handle tracked, so the M5-style shutdown
        // drain still awaits/aborts the task - a UI cancel never orphans it. This
        // models the exact sequence: UI cancel signals the flag, the task is still
        // tracked, then the shutdown drain (`cancel_all_restore_jobs`) takes the
        // handle and joins it (no orphan).
        let (state, dir) = temp_state().await;
        let app_state = AppState::new(
            state,
            HashMap::new(),
            RemoteMode::Fake,
            default_fake_registry(),
        );

        let cancel: RestoreCancel = Arc::new(AtomicBool::new(false));
        let observed_cancel = cancel.clone();
        // A task that loops until the cancel flag is set (models the stream loop).
        let handle = tokio::spawn(async move {
            loop {
                if observed_cancel.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let task_finished = handle.abort_handle();
        app_state.seed_restore_job(restore_status("ui-job", false), cancel.clone(), handle);

        // UI cancel: flag set, but the handle is LEFT tracked (not taken/dropped).
        assert!(
            app_state.signal_cancel_restore_job("ui-job"),
            "signalling a tracked job returns true"
        );
        assert!(cancel.load(Ordering::SeqCst), "cancel flag must be set");

        // The shutdown drain still finds the handle (it was NOT detached by the UI
        // cancel) and can await it - i.e. the task is accounted-for, not orphaned.
        let handles = app_state.cancel_all_restore_jobs();
        assert_eq!(
            handles.len(),
            1,
            "the UI-cancelled job's handle must STILL be tracked for the shutdown drain"
        );
        for h in handles {
            tokio::time::timeout(Duration::from_secs(2), h)
                .await
                .expect("the UI-cancelled task must be joinable on shutdown (not orphaned)")
                .expect("joined cleanly");
        }
        assert!(
            task_finished.is_finished(),
            "the task must have actually finished (cancel flag observed)"
        );

        // Idempotent: signalling an unknown id is a benign false.
        assert!(!app_state.signal_cancel_restore_job("nope"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn cancel_all_restore_jobs_drains_every_in_flight_job() {
        // M8-P1-1: the shutdown path cancels EVERY in-flight restore job and
        // returns their handles so quit can await them (no orphaned restore task).
        let (state, dir) = temp_state().await;
        let app_state = AppState::new(
            state,
            HashMap::new(),
            RemoteMode::Fake,
            default_fake_registry(),
        );

        for i in 0..3 {
            let cancel: RestoreCancel = Arc::new(AtomicBool::new(false));
            let observed = cancel.clone();
            let handle = tokio::spawn(async move {
                loop {
                    if observed.load(Ordering::SeqCst) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
            let id = format!("job-{i}");
            app_state.seed_restore_job(restore_status(&id, false), cancel, handle);
        }

        let handles = app_state.cancel_all_restore_jobs();
        assert_eq!(handles.len(), 3, "every in-flight job handle is returned");
        for h in handles {
            tokio::time::timeout(Duration::from_secs(2), h)
                .await
                .expect("each cancelled restore task must finish")
                .expect("joined cleanly");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn terminal_restore_jobs_are_count_capped() {
        // M8-P2-3: terminal restore-job records are count-capped so the map does
        // not leak across many restores. Register MORE than the cap terminal jobs
        // and assert only the cap remains (plus any active jobs, which we add one
        // of to prove active jobs are NEVER pruned).
        let (state, dir) = temp_state().await;
        let app_state = AppState::new(
            state,
            HashMap::new(),
            RemoteMode::Fake,
            default_fake_registry(),
        );

        // A trivial already-finished handle (the pruning test does not exercise the
        // task itself; seed_restore_job now requires a handle, R3-P1-2).
        let noop_handle = || tokio::spawn(async {});

        // One ACTIVE job that must survive pruning.
        let active_cancel: RestoreCancel = Arc::new(AtomicBool::new(false));
        app_state.seed_restore_job(
            restore_status("active", false),
            active_cancel,
            noop_handle(),
        );

        // Register many TERMINAL jobs (seed active, then put a done snapshot to
        // stamp terminal_at + trigger pruning).
        let total = MAX_RETAINED_TERMINAL_JOBS + 10;
        for i in 0..total {
            let id = format!("done-{i}");
            let cancel: RestoreCancel = Arc::new(AtomicBool::new(false));
            app_state.seed_restore_job(restore_status(&id, false), cancel, noop_handle());
            app_state.put_restore_job(restore_status(&id, true));
        }

        let map = app_state.lock_restore_jobs();
        let terminal = map.values().filter(|e| e.terminal_at.is_some()).count();
        assert!(
            terminal <= MAX_RETAINED_TERMINAL_JOBS,
            "terminal jobs must be capped at {MAX_RETAINED_TERMINAL_JOBS}, got {terminal}"
        );
        assert!(
            map.contains_key("active"),
            "an active job must never be pruned"
        );
        drop(map);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn dialog_tokens_are_distinct_per_mint() {
        // Two mints yield distinct tokens each bound to its own path.
        let (state, dir) = temp_state().await;
        let app_state = AppState::new(
            state,
            HashMap::new(),
            RemoteMode::Fake,
            default_fake_registry(),
        );
        let a = app_state.mint_dialog_token(std::path::PathBuf::from("/a"));
        let b = app_state.mint_dialog_token(std::path::PathBuf::from("/b"));
        assert_ne!(a, b);
        assert_eq!(
            app_state.take_dialog_token(&b),
            Some(std::path::PathBuf::from("/b"))
        );
        assert_eq!(
            app_state.take_dialog_token(&a),
            Some(std::path::PathBuf::from("/a"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
