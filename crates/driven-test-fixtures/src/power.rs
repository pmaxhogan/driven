//! [`FakePowerSource`] - a test double implementing
//! [`driven_power::PowerSource`].
//!
//! Tests drive battery / AC / metered / reachability transitions via
//! [`FakePowerSource::set`]; the orchestrator under test consumes the
//! transitions through [`PowerSource::subscribe`] /
//! [`PowerSource::current`] as it would in production.
//!
//! Implementation choice:
//!
//! The SPEC s10 trait mandates `subscribe(&self) -> broadcast::Receiver
//! <PowerState>`, so the fake fans out on a [`broadcast::Sender`]
//! internally. The latest-state snapshot for `current()` is kept in a
//! [`parking_lot::Mutex`] alongside the channel. The task brief suggested
//! a [`watch::Sender`](tokio::sync::watch::Sender) for last-value
//! snapshot semantics; we chose the Mutex+broadcast pairing because the
//! trait signature forces a broadcast receiver on `subscribe` regardless,
//! and adding a watch channel would mean two parallel state-of-truth
//! channels for the test fixture to keep in sync - inviting drift between
//! "what `current()` says" and "what `subscribe()`-emitted state
//! transitions said". One [`Mutex<PowerState>`] is the single source of
//! truth, snapshotted on `current()` and broadcast on every `set()`.

use async_trait::async_trait;
use driven_power::{PowerSource, PowerState};
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Default capacity for the broadcast channel.
///
/// 32 is generous - tests rarely produce more than a handful of
/// transitions, and a lagged receiver only loses *intermediate* states
/// (consumers re-read `current()` after a lag).
const CHANNEL_CAPACITY: usize = 32;

/// In-memory [`PowerSource`] used by every orchestrator / scheduler /
/// activity-log test.
///
/// `set()` atomically updates the snapshot and broadcasts the new state
/// to all active subscribers in one call - concurrent `set(A)`/`set(B)`
/// callers produce a totally-ordered (snapshot, broadcast) sequence,
/// not an interleaved one. `broadcast::send` is non-blocking so
/// holding the sync mutex across it is safe.
///
/// ```ignore
/// use driven_power::{PowerSource, PowerState};
/// use driven_test_fixtures::power::FakePowerSource;
///
/// # tokio_test::block_on(async {
/// let p = FakePowerSource::new(PowerState {
///     ac_connected: true,
///     battery_percent: Some(100),
///     on_metered_network: false,
///     network_reachable: true,
/// });
/// let mut rx = p.subscribe();
/// p.set(PowerState {
///     ac_connected: false,
///     battery_percent: Some(99),
///     on_metered_network: false,
///     network_reachable: true,
/// });
/// let next = rx.recv().await.unwrap();
/// assert!(!next.ac_connected);
/// # });
/// ```
#[derive(Debug, Clone)]
pub struct FakePowerSource {
    inner: Arc<FakePowerInner>,
}

#[derive(Debug)]
struct FakePowerInner {
    state: Mutex<PowerState>,
    tx: broadcast::Sender<PowerState>,
}

impl FakePowerSource {
    /// Constructs a fake holding `initial` as the starting state.
    pub fn new(initial: PowerState) -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(FakePowerInner {
                state: Mutex::new(initial),
                tx,
            }),
        }
    }

    /// Pushes a new [`PowerState`], updating the snapshot and broadcasting
    /// the transition under a single mutex hold so concurrent callers
    /// produce a totally-ordered sequence of `(snapshot, broadcast)`
    /// pairs - never an interleaving where one caller's broadcast lands
    /// after another caller's snapshot. `broadcast::send` is
    /// non-blocking, so holding the [`parking_lot::Mutex`] guard across
    /// it is safe (no `.await` and no risk of deadlock).
    ///
    /// If no subscribers are active, the broadcast send-result is
    /// `Err(SendError)` and ignored - tests routinely set initial state
    /// before subscribing.
    pub fn set(&self, next: PowerState) {
        let mut g = self.inner.state.lock();
        *g = next.clone();
        let _ = self.inner.tx.send(next);
    }

    /// Returns a snapshot of the current state without subscribing.
    ///
    /// Equivalent to [`PowerSource::current`] but synchronous - useful in
    /// test assertions that do not want to enter an async context.
    pub fn snapshot(&self) -> PowerState {
        self.inner.state.lock().clone()
    }
}

#[async_trait]
impl PowerSource for FakePowerSource {
    async fn current(&self) -> PowerState {
        self.inner.state.lock().clone()
    }

    fn subscribe(&self) -> broadcast::Receiver<PowerState> {
        self.inner.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(ac: bool) -> PowerState {
        PowerState {
            ac_connected: ac,
            battery_percent: Some(50),
            on_metered_network: false,
            network_reachable: true,
        }
    }

    #[tokio::test]
    async fn current_returns_initial() {
        let p = FakePowerSource::new(state(true));
        assert!(p.current().await.ac_connected);
    }

    #[tokio::test]
    async fn subscribe_observes_transitions() {
        let p = FakePowerSource::new(state(true));
        let mut rx = p.subscribe();
        p.set(state(false));
        let next = rx.recv().await.unwrap();
        assert!(!next.ac_connected);
    }

    #[tokio::test]
    async fn set_before_subscribe_does_not_panic() {
        let p = FakePowerSource::new(state(true));
        p.set(state(false));
        assert!(!p.snapshot().ac_connected);
    }

    #[tokio::test]
    async fn clone_shares_state() {
        let a = FakePowerSource::new(state(true));
        let b = a.clone();
        a.set(state(false));
        assert!(!b.snapshot().ac_connected);
    }
}
