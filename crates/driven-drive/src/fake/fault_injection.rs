//! Fault-injection builder methods on [`InMemoryRemoteStore`].
//!
//! Each builder takes `self` and returns `self`, so call sites can
//! chain them:
//!
//! ```ignore
//! let store = InMemoryRemoteStore::new()
//!     .with_rate_limit_after(50)
//!     .with_5xx_after(200);
//! ```
//!
//! These mirror the `with_*` API surface listed in STRESS_HARNESS s5
//! (Drive-side fault injection). The chaos harness binds them
//! per-scenario; the M3 executor tests bind them to exercise the retry
//! and backoff paths. The contract suite in `tests/fake_contract.rs`
//! uses one fault explicitly ([`with_session_invalidated_after`]).
//!
//! ## Semantics
//!
//! - "After N" means "the (N+1)-th matching request trips". Set N=0
//!   to make the very next request fail.
//! - Transient faults (rate-limit, 5xx, network-drop) reset to "never
//!   trip" once they fire; the next request after the trip succeeds.
//! - "Stay-broken" faults (auth.invalid_grant, dest-folder missing /
//!   readonly, trashed-visible-in-find) latch on first trigger and
//!   remain set for the lifetime of the store.
//! - `md5_mismatch_after` latches **on the affected entry**: when it
//!   trips during a write, the entry's wrong md5 is stamped onto the
//!   entry so every subsequent read of that entry (metadata,
//!   list_folder, find_by_op_uuid) returns the bad value until the
//!   entry is re-uploaded (which clears the latch).
//! - `session_invalidated_after_chunks` is bound at session-open time:
//!   the next session opened consumes the armed value (which is then
//!   reset on the global counter so later sessions are unaffected);
//!   use [`crate::fake::InMemoryRemoteStore::arm_session_invalidated_after`]
//!   to arm a specific, already-open session by URL.
//! - Quota is a byte-budget rather than a request count - calls that
//!   would push the cumulative committed-byte total over the budget
//!   are rejected with `drive.quota_exhausted`; smaller-or-equal calls
//!   succeed and decrement the remaining budget.
//!
//! All counters share the underlying [`crate::fake::Faults`] struct
//! reached via `self.faults` - the atomics make the per-request hot
//! path lock-free.

use std::sync::atomic::Ordering;

use super::InMemoryRemoteStore;

impl InMemoryRemoteStore {
    /// Trips a `drive.rate_limited` error after `n` more requests have
    /// gone through. Single-shot (the next request after the trip
    /// succeeds, matching SPEC s24 `drive.rate_limited` retry semantics).
    pub fn with_rate_limit_after(self, n: u64) -> Self {
        self.faults.rate_limit_after.store(n + 1, Ordering::Release);
        self
    }

    /// Trips a `drive.unreachable` 5xx after `n` more requests. Single-
    /// shot (the executor retries 5xx with backoff per SPEC s24).
    pub fn with_5xx_after(self, n: u64) -> Self {
        self.faults.five_xx_after.store(n + 1, Ordering::Release);
        self
    }

    /// Trips an `auth.invalid_grant` after `n` more requests, then
    /// latches: every subsequent request returns the same error until
    /// the store is replaced. Mirrors the Drive refresh-token-revoked
    /// path that drops an account into `needs_reauth` (SPEC s24).
    pub fn with_invalid_grant_after(self, n: u64) -> Self {
        self.faults
            .invalid_grant_after
            .store(n + 1, Ordering::Release);
        self
    }

    /// Trips a `net.intermittent` error after `n` more requests. Single-
    /// shot. Models the lower-level network failures the executor's
    /// circuit breaker (DESIGN s5.8.3) treats as transient.
    pub fn with_network_drop_after(self, n: u64) -> Self {
        self.faults
            .network_drop_after
            .store(n + 1, Ordering::Release);
        self
    }

    /// Convenience: trip a `net.intermittent` error on the VERY NEXT
    /// request (= [`Self::with_network_drop_after`] with `n == 0`). The
    /// ROADMAP M1 fault-injection surface names this builder; it reads
    /// cleaner than `with_network_drop_after(0)` at call sites.
    pub fn with_network_drop(self) -> Self {
        self.with_network_drop_after(0)
    }

    /// Injects an artificial `delay` before EVERY request the fake
    /// serves (read or write). Models DESIGN s5.8.1's "lossy: +500ms
    /// latency" so the M3 network-resilience tests can exercise the
    /// orchestrator's latency / timeout paths deterministically.
    ///
    /// The delay is awaited in
    /// [`crate::fake::InMemoryRemoteStore::maybe_delay`] - the single
    /// insertion point at the top of every trait method, BEFORE the
    /// internal store mutex is acquired - so the `tokio::time::sleep`
    /// never spans a held `parking_lot` guard. Unlike the transient
    /// `with_*_after` faults this latches: every subsequent request waits
    /// `delay` until the store is replaced.
    pub fn with_slow_responses(self, delay: std::time::Duration) -> Self {
        // Saturate to u64 nanos; a delay longer than ~584 years is not a
        // realistic test input and clamping avoids an overflow panic.
        let nanos = u64::try_from(delay.as_nanos()).unwrap_or(u64::MAX);
        self.faults
            .response_delay_nanos
            .store(nanos, Ordering::Release);
        self
    }

    /// Arms a session-invalidating 4xx on the NEXT resumable session
    /// opened by this store: after `n_chunks` accepted chunks the
    /// session invalidates with [`ResumeProgress::SessionInvalid`] and
    /// stays dead. The caller must open a new session and re-upload
    /// from byte 0 (SPEC s3 `resume_chunk` + DESIGN s5.4 "4xx during
    /// in-flight resumable").
    ///
    /// Each [`crate::fake::InMemoryRemoteStore::resumable_session`]
    /// call consumes the armed value (resetting the global counter to
    /// `u64::MAX`) so later sessions are unaffected - the C.2 fix
    /// guards against "session B consumes A's chunk budget" cross-talk.
    /// Use [`crate::fake::InMemoryRemoteStore::arm_session_invalidated_after`]
    /// to arm a specific, already-open session by URL.
    pub fn with_session_invalidated_after(self, n_chunks: u32) -> Self {
        self.faults
            .session_invalidated_after_chunks
            .store(u64::from(n_chunks) + 1, Ordering::Release);
        self
    }

    /// Trips an md5 mismatch on the next `(n+1)`-th write. The
    /// committed bytes are correct but the affected entry's
    /// `corrupted_md5` is latched onto a deliberately-wrong value -
    /// every subsequent read of that entry (metadata, list_folder,
    /// find_by_op_uuid, etc.) returns the bad md5 until the entry is
    /// re-uploaded (which clears the latch). This matches Drive's
    /// real failure mode: a checksum-mismatched object stays bad until
    /// you replace its bytes. Exercises the executor's
    /// `drive.checksum_mismatch` -> retry path (SPEC s8 + s24).
    pub fn with_md5_mismatch_after(self, n: u64) -> Self {
        self.faults
            .md5_mismatch_after
            .store(n + 1, Ordering::Release);
        self
    }

    /// Caps the total committed-bytes budget at `n_bytes`. Subsequent
    /// `create` / `update` / final-chunk `resume_chunk` calls that
    /// would push the cumulative committed bytes past the cap are
    /// rejected with `drive.quota_exhausted` (SPEC s24).
    pub fn with_quota_exhausted_after(self, n_bytes: u64) -> Self {
        self.faults
            .quota_exhausted_after_bytes
            .store(n_bytes, Ordering::Release);
        self
    }

    /// Latches the destination-folder-missing state. Every subsequent
    /// `create` / `update` / `ensure_folder` / `resumable_session` /
    /// `resume_chunk` request returns `drive.dest_folder_missing`
    /// (SPEC s24). Read-only calls (`list_folder`, `metadata`,
    /// `download`, `find_by_op_uuid`, `about`) keep working - they
    /// model the user inspecting Drive after the configured destination
    /// has been moved or trashed in the web UI.
    pub fn with_dest_folder_missing(self) -> Self {
        self.faults
            .dest_folder_missing
            .store(true, Ordering::Release);
        self
    }

    /// Latches the destination-folder-readonly state. Every subsequent
    /// write-target request returns `drive.dest_folder_permission_denied`
    /// (SPEC s24). Read-only calls keep working - mirrors the user
    /// changing the destination folder's sharing to "view only" for
    /// this account.
    pub fn with_dest_folder_readonly(self) -> Self {
        self.faults
            .dest_folder_readonly
            .store(true, Ordering::Release);
        self
    }

    /// Latches `find_by_op_uuid` to surface trashed children alongside
    /// live ones, modelling the "I trashed a row but did not delete its
    /// `file_state` entry" reconciliation case (DESIGN s5.6). The fault
    /// is misnamed in early drafts as "fileid_recycle" - the behaviour
    /// here is NOT actual Drive file_id recycling (that would need a
    /// dedicated id-pool flag and lands in M3 design). The renamed
    /// builder makes the intent obvious at call sites.
    pub fn with_trashed_visible_in_find_by_op_uuid(self) -> Self {
        self.faults
            .trashed_visible_in_find
            .store(true, Ordering::Release);
        self
    }
}
