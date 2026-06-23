//! Orphan-snapshot cleanup ledger (ROADMAP M3.5 acceptance).
//!
//! A [`VssSnapshot`](crate::VssSnapshot) releases its shadow copy on [`Drop`]
//! at end-of-cycle. But a `kill -9` (or a power loss) skips `Drop`, stranding
//! the shadow copy on the volume - it consumes diff-area space until something
//! releases it. So on startup Driven sweeps the snapshots it *recorded as its
//! own* and releases any older than one hour.
//!
//! # Ownership is proven, not guessed
//!
//! Releasing another backup app's (or the user's) shadow copy is destructive,
//! so cleanup must NEVER release a shadow it cannot prove is Driven's. We do
//! not enumerate every shadow on the box and heuristically guess ownership.
//! Instead, [`VssSnapshot::create`](crate::VssSnapshot) records each shadow's
//! GUID + creation time into an [`OrphanRegistry`] (persisted by the
//! orchestrator into the state dir) the instant it is created, and removes the
//! entry when [`Drop`] releases it cleanly. On startup we delete ONLY the
//! recorded GUIDs that are still older than the age cutoff; a GUID that no
//! longer exists on the volume (already released) is a no-op.
//!
//! This module is the pure ledger + age filter: the orchestrator owns
//! persistence and calls [`VssSnapshot::delete_by_id`](crate::VssSnapshot) for
//! each id [`prune_orphans`] selects.

use serde::{Deserialize, Serialize};

/// Default age past which a recorded-but-still-present shadow copy is treated
/// as orphaned and released on startup (ROADMAP M3.5: "older than 1h").
///
/// One hour comfortably exceeds the longest healthy backup cycle, so a live
/// in-use snapshot from a still-running sibling process is never swept.
pub const DEFAULT_ORPHAN_MAX_AGE_MS: i64 = 60 * 60 * 1000;

/// One recorded shadow copy: its GUID plus the wall-clock time (Unix ms) the
/// COM sequence created it. Persisted so a later process can prove ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedSnapshot {
    /// The shadow-copy GUID as a braced `{...}` string (round-trips to a
    /// Windows `GUID`).
    pub snapshot_id: String,
    /// Wall-clock creation time, Unix epoch milliseconds.
    pub created_at_ms: i64,
}

/// The persisted ledger of Driven-created shadow copies (the cleanup
/// authority). Serialised by the orchestrator into the state dir; the in-memory
/// form here is pure so its add/remove/prune logic is unit-testable off
/// Windows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanRegistry {
    /// Every shadow Driven currently believes it owns.
    pub snapshots: Vec<RecordedSnapshot>,
}

impl OrphanRegistry {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a freshly-created shadow copy as Driven-owned. Idempotent on the
    /// GUID (a re-record updates the timestamp rather than duplicating).
    pub fn record(&mut self, snapshot_id: impl Into<String>, created_at_ms: i64) {
        let snapshot_id = snapshot_id.into();
        if let Some(existing) = self
            .snapshots
            .iter_mut()
            .find(|s| s.snapshot_id == snapshot_id)
        {
            existing.created_at_ms = created_at_ms;
        } else {
            self.snapshots.push(RecordedSnapshot {
                snapshot_id,
                created_at_ms,
            });
        }
    }

    /// Drop a shadow from the ledger once it has been released (clean [`Drop`]
    /// or a successful prune). No-op when the id is absent.
    pub fn forget(&mut self, snapshot_id: &str) {
        self.snapshots.retain(|s| s.snapshot_id != snapshot_id);
    }

    /// The GUIDs of shadows recorded more than `max_age_ms` before `now_ms`.
    /// These are the orphans the caller should release.
    pub fn orphans_older_than(&self, now_ms: i64, max_age_ms: i64) -> Vec<String> {
        prune_orphans(&self.snapshots, now_ms, max_age_ms)
    }
}

/// Select the GUIDs of recorded snapshots older than `max_age_ms` relative to
/// `now_ms` (the >1h orphan filter). Pure - the actual `DeleteSnapshots` COM
/// call stays in [`VssSnapshot::delete_by_id`](crate::VssSnapshot).
///
/// A snapshot whose `created_at_ms` is in the future (a clock that jumped
/// backwards) is conservatively NOT pruned: its age reads as <= 0, well under
/// the cutoff, so a backwards wall-clock jump never releases a live snapshot.
pub fn prune_orphans(snapshots: &[RecordedSnapshot], now_ms: i64, max_age_ms: i64) -> Vec<String> {
    snapshots
        .iter()
        .filter(|s| now_ms.saturating_sub(s.created_at_ms) > max_age_ms)
        .map(|s| s.snapshot_id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, created: i64) -> RecordedSnapshot {
        RecordedSnapshot {
            snapshot_id: id.to_string(),
            created_at_ms: created,
        }
    }

    #[test]
    fn prunes_only_snapshots_older_than_the_cutoff() {
        let now = 10_000_000;
        let snaps = vec![
            // 2h old -> orphan.
            snap("{old}", now - 2 * DEFAULT_ORPHAN_MAX_AGE_MS),
            // 30m old -> still live, keep.
            snap("{fresh}", now - DEFAULT_ORPHAN_MAX_AGE_MS / 2),
            // exactly 1h old -> NOT strictly older, keep (boundary).
            snap("{exactly_1h}", now - DEFAULT_ORPHAN_MAX_AGE_MS),
        ];
        let orphans = prune_orphans(&snaps, now, DEFAULT_ORPHAN_MAX_AGE_MS);
        assert_eq!(orphans, vec!["{old}".to_string()]);
    }

    #[test]
    fn future_dated_snapshot_is_never_pruned() {
        let now = 5_000_000;
        // created_at in the future (clock skew): age is negative.
        let snaps = vec![snap("{future}", now + 1_000_000)];
        assert!(prune_orphans(&snaps, now, DEFAULT_ORPHAN_MAX_AGE_MS).is_empty());
    }

    #[test]
    fn registry_record_is_idempotent_on_guid() {
        let mut reg = OrphanRegistry::new();
        reg.record("{a}", 100);
        reg.record("{a}", 200); // updates timestamp, no duplicate
        assert_eq!(reg.snapshots.len(), 1);
        assert_eq!(reg.snapshots[0].created_at_ms, 200);
    }

    #[test]
    fn registry_forget_removes_the_entry() {
        let mut reg = OrphanRegistry::new();
        reg.record("{a}", 100);
        reg.record("{b}", 100);
        reg.forget("{a}");
        assert_eq!(reg.snapshots.len(), 1);
        assert_eq!(reg.snapshots[0].snapshot_id, "{b}");
        // forgetting an absent id is a no-op
        reg.forget("{missing}");
        assert_eq!(reg.snapshots.len(), 1);
    }

    #[test]
    fn registry_orphans_older_than_delegates_to_prune() {
        let now = 10_000_000;
        let mut reg = OrphanRegistry::new();
        reg.record("{old}", now - 2 * DEFAULT_ORPHAN_MAX_AGE_MS);
        reg.record("{fresh}", now);
        assert_eq!(
            reg.orphans_older_than(now, DEFAULT_ORPHAN_MAX_AGE_MS),
            vec!["{old}".to_string()]
        );
    }

    #[test]
    fn registry_serde_round_trips() {
        let mut reg = OrphanRegistry::new();
        reg.record("{a}", 100);
        reg.record("{b}", 200);
        let json = serde_json::to_string(&reg).unwrap();
        let back: OrphanRegistry = serde_json::from_str(&json).unwrap();
        assert_eq!(reg, back);
    }
}
