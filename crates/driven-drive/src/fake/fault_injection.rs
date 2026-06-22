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
//! - Transient faults (rate-limit, 5xx, network-drop, single-shot
//!   md5-mismatch, single-session-invalidation) reset to "never trip"
//!   once they fire; the next request after the trip succeeds.
//! - "Stay-broken" faults (auth.invalid_grant, dest-folder missing /
//!   readonly) latch on first trigger and remain set for the lifetime
//!   of the store.
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

    /// Trips a session-invalidating 4xx after `n_chunks` more
    /// `resume_chunk` calls have gone through. The session that was
    /// being chunked into stays dead - the caller must open a new
    /// session and re-upload from byte 0 (SPEC s3 `resume_chunk` +
    /// DESIGN s5.4 "4xx during in-flight resumable").
    pub fn with_session_invalidated_after(self, n_chunks: u32) -> Self {
        self.faults
            .session_invalidated_after_chunks
            .store(u64::from(n_chunks) + 1, Ordering::Release);
        self
    }

    /// Trips an md5 mismatch on the next `n+1`-th response. The
    /// committed bytes are correct but the returned [`crate::remote_store::RemoteEntry`]
    /// carries a deliberately-wrong md5, exercising the executor's
    /// `drive.checksum_mismatch` -> retry path (SPEC s24).
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

    /// Enables Drive's documented (rare) file_id-reuse behaviour:
    /// `find_by_op_uuid` will consider trashed children as candidates,
    /// modelling the case where a previously-trashed object's id is
    /// recycled and matched. The reconciliation pass (DESIGN s5.6) must
    /// tolerate this.
    pub fn with_fileid_recycle(self) -> Self {
        self.faults.fileid_recycle.store(true, Ordering::Release);
        self
    }
}
