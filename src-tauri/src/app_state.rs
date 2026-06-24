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
use std::time::Duration;

use driven_core::orchestrator::Orchestrator;
use driven_core::state::StateRepo;
use driven_core::types::AccountId;
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

/// The per-task graceful-drain budget on quit (DESIGN s5.10.2): await each task
/// this long before aborting it. The run loop's own in-flight cycle is bounded
/// by the larger `SHUTDOWN_DRAIN_TIMEOUT` in `lib.rs` (which calls
/// [`AccountHandle::shutdown`] inside its own outer timeout); this per-task
/// budget keeps a single wedged bridge from holding the join indefinitely.
const TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// The collected per-account task handles + the bridge shutdown sender, returned
/// by `assembly::build_account` and stored on [`AccountHandle`]. Groups the four
/// tracked tasks so the constructor signature stays readable (R-P1-1).
pub struct AccountTasks {
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
    /// Order:
    /// 1. signal the orchestrator to stop after its in-flight cycle
    ///    (`Orchestrator::shutdown()`), and signal the bridges via
    ///    [`Self::bridge_shutdown`] (the watcher bridge's source never closes on
    ///    its own);
    /// 2. for EVERY tracked handle: await-with-timeout, and on timeout abort
    ///    the task AND AWAIT the aborted handle - so the task is truly GONE, not
    ///    merely abort-requested. The power poller loops forever, so it always
    ///    takes the abort path; the others normally drain cleanly.
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

        // 2) Drain each tracked task: cleanly within the budget, else abort +
        //    await so it cannot outlive quit.
        drain_or_abort(&self.run_loop).await;
        drain_or_abort(&self.watcher_bridge).await;
        drain_or_abort(&self.event_bridge).await;
        drain_or_abort(&self.power_poller).await;
    }
}

/// Take the handle out of `slot` and drive it to a true stop: await it up to
/// [`TASK_DRAIN_TIMEOUT`]; on timeout `abort()` it and AWAIT the aborted handle
/// so the task is genuinely finished before this returns (R-P1-1). A `None`
/// slot (already drained / never spawned) is a no-op.
///
/// `tokio::time::timeout` MOVES the `JoinHandle` into itself and, on elapse,
/// DROPS it - and a dropped `JoinHandle` does NOT cancel its task (it merely
/// detaches it). So we capture an [`tokio::task::AbortHandle`] BEFORE the
/// timeout, and on elapse abort via it, then RE-AWAIT the same task via a second
/// `JoinHandle` we also kept... which `timeout` consumed. To avoid that, we do
/// not hand the original handle to `timeout`; we `select!` between the handle and
/// a sleep so the handle stays in scope and can be re-awaited after an abort.
async fn drain_or_abort(slot: &Mutex<Option<JoinHandle<()>>>) {
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
        () = tokio::time::sleep(TASK_DRAIN_TIMEOUT) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use driven_core::orchestrator::{Orchestrator, OrchestratorConfig, TickSource};
    use driven_core::types::OrchestratorState;
    use std::sync::atomic::{AtomicBool, Ordering};

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
}
