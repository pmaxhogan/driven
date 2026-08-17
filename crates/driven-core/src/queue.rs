//! The per-account PENDING-WORK QUEUE (issue #303).
//!
//! # Why this exists
//!
//! Backup work arrives from four independent places - a crash-recovery
//! reconcile, the filesystem watcher, the user's "Back up now", and the
//! scheduled timer - but only ONE cycle may run at a time per account (the
//! single-in-flight guard in [`crate::orchestrator::SyncOrchestrator::run`]).
//! Until this module existed, everything that arrived while a cycle was running
//! was funnelled into a capacity-1 mpsc channel: a burst of ten different
//! requests silently collapsed into one anonymous follow-up, the user could not
//! see that anything was waiting, and there was no way to cancel any of it.
//!
//! This queue makes that wait VISIBLE and CANCELLABLE. It is a plain,
//! synchronous data structure with no channels and no async: the orchestrator
//! owns one, mutates it from the run loop and from its control methods, and
//! broadcasts a [`QueueSnapshot`] after every mutation. Keeping it inert makes
//! the coalescing / ordering / cancel rules directly unit-testable.
//!
//! # Invariants
//!
//! - **One running item per account.** [`WorkQueue::start_next`] refuses to
//!   hand out a second item while one is running, so the queue can never break
//!   the single-in-flight-cycle guarantee even if a caller loops.
//! - **In-memory only.** Unstarted items do NOT survive a restart (locked spec
//!   2026-08-17). Nothing here is persisted; the durable safety net is
//!   unchanged (`pending_ops` + the next scheduled scan re-derive any work that
//!   was dropped).
//! - **Cancel never tears a file.** Cancelling a PENDING item just drops it.
//!   Cancelling the RUNNING item only REQUESTS a cancel - the orchestrator
//!   honours it through the existing pause-drain mechanism, so in-flight
//!   uploads still finish and commit.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::orchestrator::TickSource;
use crate::types::{AccountId, SourceId, UnixMs};

/// Stable id of one queued work item, unique within one process run.
///
/// Handed to the webview so a "cancel this one" click names exactly the item
/// the user clicked, even after the list around it has changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkItemId(pub u64);

impl std::fmt::Display for WorkItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// What one queued item represents - the four ways backup work is requested.
///
/// This is the display kind AND the coalescing key (together with the source):
/// two watcher ticks for the same source are the same pending work, but a
/// watcher tick and a manual "Back up now" are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    /// Crash recovery: adopt / re-run the ops an interrupted run left pending
    /// (DESIGN s5.6). Runs FIRST - see [`WorkQueue::enqueue`].
    Recovery,
    /// The filesystem watcher saw changes under a source (DESIGN s5.9.1).
    Watcher,
    /// The user asked for a backup now (the "Back up now" button, the tray, or
    /// the paused banner's "Sync now").
    Manual,
    /// The scheduled-scan timer fired. Enqueued only once the run is ACTUALLY
    /// due - a future tick is not pending work and never appears in the queue.
    Scheduled,
}

impl WorkKind {
    /// Does this kind jump the queue? Only crash recovery does: it adopts
    /// orphaned remote objects from an interrupted run, and running a fresh
    /// scan ahead of it would re-upload bytes that are already on the remote.
    #[must_use]
    pub fn runs_first(self) -> bool {
        matches!(self, WorkKind::Recovery)
    }
}

/// One item in the queue: what was requested, for which source, and when.
///
/// `source_id` is the source the request was ATTRIBUTED to (a watcher tick
/// names its source; the scheduled timer names none). It is the coalescing key
/// and the display subtitle. It is deliberately NOT a promise that the cycle
/// will touch only that source: a cycle still runs every enabled source of the
/// account, which is what the queue's "items run one at a time per account"
/// footer tells the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    /// Stable id for cancel-by-id.
    pub id: WorkItemId,
    /// What kind of work this is (display + coalescing key).
    pub kind: WorkKind,
    /// The source this request came from, when it came from one.
    pub source_id: Option<SourceId>,
    /// Wall-clock ms the item was FIRST enqueued (a coalesced duplicate keeps
    /// the original time, so "queued 2 min ago" means what it says).
    pub enqueued_at: UnixMs,
    /// The tick the orchestrator runs the cycle with. Carried so the queue is
    /// the single source of truth for what runs next, rather than the tick
    /// being re-derived at the call site.
    pub tick: TickSource,
}

/// The queue as the webview sees it (payload of `queue:changed` and of the
/// `get_work_queue` hydration command).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSnapshot {
    /// The account this queue belongs to.
    pub account_id: AccountId,
    /// The item currently running, if any.
    pub running: Option<WorkItem>,
    /// Whether a cancel has been REQUESTED for the running item (it is
    /// draining: no new ops are dispatched, in-flight ones still commit).
    pub running_cancelled: bool,
    /// Items waiting, in the order they will run.
    pub pending: Vec<WorkItem>,
    /// Wall-clock ms of the next scheduled scan, when one is armed. Drives the
    /// empty state's "next scheduled backup HH:MM".
    pub next_scheduled_at: Option<UnixMs>,
}

impl QueueSnapshot {
    /// Items the user would count as outstanding: pending plus the running one.
    /// The number the top-bar badge shows.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.pending.len() + usize::from(self.running.is_some())
    }
}

/// What [`WorkQueue::enqueue`] did with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// A new item was appended (or, for recovery, prepended).
    Enqueued(WorkItemId),
    /// An equivalent item was already pending; this request folded into it.
    Coalesced(WorkItemId),
}

impl EnqueueOutcome {
    /// The id of the item this request is represented by.
    #[must_use]
    pub fn id(self) -> WorkItemId {
        match self {
            EnqueueOutcome::Enqueued(id) | EnqueueOutcome::Coalesced(id) => id,
        }
    }

    /// Did this request add a NEW item (as opposed to folding into one)?
    #[must_use]
    pub fn is_new(self) -> bool {
        matches!(self, EnqueueOutcome::Enqueued(_))
    }
}

/// What [`WorkQueue::cancel`] found for the requested id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// No item with that id is pending or running (already finished, or a
    /// stale click from a webview showing an old snapshot).
    NotFound,
    /// A pending item was removed. Nothing was running for it, so nothing has
    /// to drain.
    Pending,
    /// The RUNNING item was flagged for cancel; the caller must now drive the
    /// pause-drain.
    Running,
}

/// What [`WorkQueue::clear`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClearOutcome {
    /// How many pending items were dropped.
    pub cancelled_pending: usize,
    /// Whether the running item was newly flagged for cancel.
    pub cancelled_running: bool,
}

impl ClearOutcome {
    /// Did clearing change anything at all? (A clear of an empty queue must
    /// not broadcast a snapshot or log.)
    #[must_use]
    pub fn changed(self) -> bool {
        self.cancelled_pending > 0 || self.cancelled_running
    }
}

/// The running item plus its cancel flag.
#[derive(Debug, Clone)]
struct Running {
    item: WorkItem,
    cancel_requested: bool,
}

#[derive(Debug, Default)]
struct Inner {
    pending: VecDeque<WorkItem>,
    running: Option<Running>,
    next_scheduled_at: Option<UnixMs>,
}

/// The per-account pending-work queue. See the module docs.
#[derive(Debug, Default)]
pub struct WorkQueue {
    inner: Mutex<Inner>,
    next_id: AtomicU64,
}

impl WorkQueue {
    /// A new, empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks the inner state, recovering from a poisoned lock.
    ///
    /// A panic elsewhere must never make the queue permanently unusable: the
    /// inner value is a plain deque with no invariant that a panic could tear,
    /// and refusing to queue work would silently stop backups.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Requests work.
    ///
    /// Coalescing: if an item with the SAME `(kind, source_id)` is already
    /// PENDING, this request folds into it (keeping its position and its
    /// original `enqueued_at`) and no second item appears. The RUNNING item is
    /// deliberately not a coalescing target - work requested after a cycle
    /// started may well have been missed by that cycle's scan, so it earns its
    /// own follow-up.
    ///
    /// Ordering: FIFO, except [`WorkKind::Recovery`], which is placed at the
    /// FRONT (see [`WorkKind::runs_first`]).
    pub fn enqueue(
        &self,
        kind: WorkKind,
        tick: TickSource,
        source_id: Option<SourceId>,
        now: UnixMs,
    ) -> EnqueueOutcome {
        let mut inner = self.lock();
        if let Some(existing) = inner
            .pending
            .iter()
            .find(|i| i.kind == kind && i.source_id == source_id)
        {
            return EnqueueOutcome::Coalesced(existing.id);
        }
        let id = WorkItemId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let item = WorkItem {
            id,
            kind,
            source_id,
            enqueued_at: now,
            tick,
        };
        if kind.runs_first() {
            inner.pending.push_front(item);
        } else {
            inner.pending.push_back(item);
        }
        EnqueueOutcome::Enqueued(id)
    }

    /// Takes the head of the queue and marks it RUNNING, or returns `None` when
    /// the queue is empty OR an item is already running (the single-in-flight
    /// guard - see the module docs).
    pub fn start_next(&self) -> Option<WorkItem> {
        let mut inner = self.lock();
        if inner.running.is_some() {
            return None;
        }
        let item = inner.pending.pop_front()?;
        inner.running = Some(Running {
            item: item.clone(),
            cancel_requested: false,
        });
        Some(item)
    }

    /// Clears the running slot once its cycle has finished, returning the item
    /// that was running (if any).
    pub fn finish_running(&self) -> Option<WorkItem> {
        self.lock().running.take().map(|r| r.item)
    }

    /// Has a cancel been requested for the item currently running?
    #[must_use]
    pub fn running_cancel_requested(&self) -> bool {
        self.lock()
            .running
            .as_ref()
            .is_some_and(|r| r.cancel_requested)
    }

    /// Cancels one item by id. See [`CancelOutcome`].
    pub fn cancel(&self, id: WorkItemId) -> CancelOutcome {
        let mut inner = self.lock();
        if let Some(pos) = inner.pending.iter().position(|i| i.id == id) {
            inner.pending.remove(pos);
            return CancelOutcome::Pending;
        }
        match inner.running.as_mut() {
            Some(running) if running.item.id == id => {
                running.cancel_requested = true;
                CancelOutcome::Running
            }
            _ => CancelOutcome::NotFound,
        }
    }

    /// Cancels EVERY pending item and requests a graceful stop of the running
    /// one ("Clear all"). Returns what actually changed, so a clear of an empty
    /// queue can stay silent.
    pub fn clear(&self) -> ClearOutcome {
        let mut inner = self.lock();
        let cancelled_pending = inner.pending.len();
        inner.pending.clear();
        let cancelled_running = match inner.running.as_mut() {
            Some(running) if !running.cancel_requested => {
                running.cancel_requested = true;
                true
            }
            _ => false,
        };
        ClearOutcome {
            cancelled_pending,
            cancelled_running,
        }
    }

    /// Records when the next scheduled scan is due (wall-clock ms), for the
    /// empty state's "next scheduled backup HH:MM". `None` clears it.
    pub fn set_next_scheduled_at(&self, at: Option<UnixMs>) {
        self.lock().next_scheduled_at = at;
    }

    /// Is there anything left to run (excluding the item already running)?
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.lock().pending.is_empty()
    }

    /// The current queue as the webview sees it.
    #[must_use]
    pub fn snapshot(&self, account_id: AccountId) -> QueueSnapshot {
        let inner = self.lock();
        QueueSnapshot {
            account_id,
            running: inner.running.as_ref().map(|r| r.item.clone()),
            running_cancelled: inner.running.as_ref().is_some_and(|r| r.cancel_requested),
            pending: inner.pending.iter().cloned().collect(),
            next_scheduled_at: inner.next_scheduled_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src() -> SourceId {
        SourceId::new_v4()
    }

    fn account() -> AccountId {
        AccountId::new_v4()
    }

    #[test]
    fn enqueue_is_fifo_and_snapshots_in_run_order() {
        let q = WorkQueue::new();
        let a = q.enqueue(WorkKind::Manual, TickSource::Manual, None, 10);
        let b = q.enqueue(WorkKind::Scheduled, TickSource::Scheduled, None, 20);
        assert!(a.is_new() && b.is_new());
        let snap = q.snapshot(account());
        assert_eq!(
            snap.pending.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![a.id(), b.id()],
            "pending order is the order the items will run"
        );
        assert_eq!(snap.outstanding(), 2);
        assert_eq!(q.start_next().map(|i| i.id), Some(a.id()));
    }

    #[test]
    fn watcher_ticks_coalesce_per_source_but_not_across_sources() {
        let q = WorkQueue::new();
        let (s1, s2) = (src(), src());
        let first = q.enqueue(WorkKind::Watcher, TickSource::Watcher, Some(s1), 100);
        let dup = q.enqueue(WorkKind::Watcher, TickSource::Watcher, Some(s1), 900);
        let other = q.enqueue(WorkKind::Watcher, TickSource::Watcher, Some(s2), 950);

        assert!(first.is_new());
        assert_eq!(
            dup,
            EnqueueOutcome::Coalesced(first.id()),
            "a second tick for the SAME source folds into the pending one"
        );
        assert!(other.is_new(), "a different source is different work");

        let snap = q.snapshot(account());
        assert_eq!(snap.pending.len(), 2);
        assert_eq!(
            snap.pending[0].enqueued_at, 100,
            "a coalesced duplicate keeps the ORIGINAL enqueue time"
        );
    }

    #[test]
    fn manual_coalesces_per_source_and_never_with_a_watcher_tick() {
        let q = WorkQueue::new();
        let s = src();
        let manual = q.enqueue(WorkKind::Manual, TickSource::Manual, Some(s), 1);
        let manual_again = q.enqueue(WorkKind::Manual, TickSource::Manual, Some(s), 2);
        let watcher = q.enqueue(WorkKind::Watcher, TickSource::Watcher, Some(s), 3);
        assert_eq!(manual_again, EnqueueOutcome::Coalesced(manual.id()));
        assert!(watcher.is_new(), "different kinds are different work");
        assert_eq!(q.snapshot(account()).pending.len(), 2);
    }

    #[test]
    fn recovery_jumps_the_queue() {
        let q = WorkQueue::new();
        let manual = q.enqueue(WorkKind::Manual, TickSource::Manual, None, 1);
        let recovery = q.enqueue(WorkKind::Recovery, TickSource::Scheduled, None, 2);
        let ids: Vec<_> = q.snapshot(account()).pending.iter().map(|i| i.id).collect();
        assert_eq!(
            ids,
            vec![recovery.id(), manual.id()],
            "crash recovery runs before a fresh scan so it can adopt orphaned uploads"
        );
    }

    #[test]
    fn only_one_item_runs_at_a_time() {
        let q = WorkQueue::new();
        q.enqueue(WorkKind::Manual, TickSource::Manual, None, 1);
        q.enqueue(WorkKind::Scheduled, TickSource::Scheduled, None, 2);
        let first = q.start_next().expect("first item starts");
        assert!(
            q.start_next().is_none(),
            "a second item must not start while one is running"
        );
        assert_eq!(q.finish_running().map(|i| i.id), Some(first.id));
        assert!(q.start_next().is_some(), "the next item starts once free");
    }

    #[test]
    fn a_request_made_while_a_cycle_runs_does_not_coalesce_into_it() {
        let q = WorkQueue::new();
        let s = src();
        q.enqueue(WorkKind::Watcher, TickSource::Watcher, Some(s), 1);
        let running = q.start_next().expect("starts");
        let follow_up = q.enqueue(WorkKind::Watcher, TickSource::Watcher, Some(s), 2);
        assert!(
            follow_up.is_new(),
            "changes seen after the cycle's scan need their own follow-up"
        );
        assert_ne!(follow_up.id(), running.id);
        assert!(q.has_pending());
    }

    #[test]
    fn cancelling_a_pending_item_removes_it_and_keeps_the_rest_in_order() {
        let q = WorkQueue::new();
        let a = q.enqueue(WorkKind::Manual, TickSource::Manual, Some(src()), 1);
        let b = q.enqueue(WorkKind::Manual, TickSource::Manual, Some(src()), 2);
        let c = q.enqueue(WorkKind::Manual, TickSource::Manual, Some(src()), 3);
        assert_eq!(q.cancel(b.id()), CancelOutcome::Pending);
        assert_eq!(
            q.snapshot(account())
                .pending
                .iter()
                .map(|i| i.id)
                .collect::<Vec<_>>(),
            vec![a.id(), c.id()]
        );
        assert_eq!(
            q.cancel(b.id()),
            CancelOutcome::NotFound,
            "cancelling twice is a no-op, not a panic"
        );
    }

    #[test]
    fn cancelling_the_running_item_flags_it_rather_than_dropping_it() {
        let q = WorkQueue::new();
        let a = q.enqueue(WorkKind::Manual, TickSource::Manual, None, 1);
        let running = q.start_next().expect("starts");
        assert!(!q.running_cancel_requested());
        assert_eq!(q.cancel(running.id), CancelOutcome::Running);
        assert!(q.running_cancel_requested());
        let snap = q.snapshot(account());
        assert_eq!(
            snap.running.map(|i| i.id),
            Some(a.id()),
            "the item stays visible while it drains"
        );
        assert!(snap.running_cancelled);
    }

    #[test]
    fn clear_all_drops_every_pending_item_and_drains_the_running_one() {
        let q = WorkQueue::new();
        q.enqueue(WorkKind::Manual, TickSource::Manual, None, 1);
        q.start_next().expect("starts");
        q.enqueue(WorkKind::Watcher, TickSource::Watcher, Some(src()), 2);
        q.enqueue(WorkKind::Watcher, TickSource::Watcher, Some(src()), 3);

        let outcome = q.clear();
        assert_eq!(outcome.cancelled_pending, 2);
        assert!(outcome.cancelled_running);
        assert!(outcome.changed());
        assert!(!q.has_pending());
        assert!(q.running_cancel_requested());

        let again = q.clear();
        assert!(
            !again.changed(),
            "clearing an already-clear queue changes nothing (no event, no log)"
        );
    }

    #[test]
    fn next_scheduled_at_rides_on_the_snapshot() {
        let q = WorkQueue::new();
        assert_eq!(q.snapshot(account()).next_scheduled_at, None);
        q.set_next_scheduled_at(Some(1_700_000_000_000));
        assert_eq!(
            q.snapshot(account()).next_scheduled_at,
            Some(1_700_000_000_000)
        );
    }

    #[test]
    fn ids_are_unique_across_cancel_and_reuse() {
        let q = WorkQueue::new();
        let a = q.enqueue(WorkKind::Manual, TickSource::Manual, None, 1);
        q.cancel(a.id());
        let b = q.enqueue(WorkKind::Manual, TickSource::Manual, None, 2);
        assert_ne!(
            a.id(),
            b.id(),
            "a cancelled id is never reused, so a stale click cannot hit the wrong item"
        );
    }
}
