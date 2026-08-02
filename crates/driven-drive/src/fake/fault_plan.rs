//! Declarative fault plans for [`InMemoryRemoteStore`] (agent QA harness).
//!
//! The `with_*` builders in [`super::fault_injection`] are a compile-time API:
//! perfect for the chaos harness and unit tests, unusable for a RUNNING app.
//! The e2e harness drives the real desktop app (fake remote selected via
//! `DRIVEN_USE_FAKE_REMOTE=1`) and needs to arm the same faults from OUTSIDE
//! the process - so this module maps a JSON document onto that builder surface.
//!
//! The app shell reads the plan from the file named by the
//! `DRIVEN_TEST_FAULT_PLAN` env var and applies it to every fake store it
//! creates. That seam is DOUBLY gated: the env var must be set AND the fake
//! remote must be the selected remote (`DRIVEN_USE_FAKE_REMOTE=1`) - the plan
//! can never touch a real backend, because it only exists on the fake.
//!
//! ## Shape
//!
//! Every field is optional; an absent field arms nothing. Counters follow the
//! builder semantics ("after N" = the (N+1)-th matching request trips):
//!
//! ```json
//! {
//!   "rate_limit_after": 50,
//!   "http_5xx_after": 200,
//!   "invalid_grant_after": 10,
//!   "network_drop_after": 0,
//!   "network_drop_every_request": false,
//!   "slow_responses_ms": 500,
//!   "session_invalidated_after_chunks": 2,
//!   "md5_mismatch_after": 3,
//!   "quota_exhausted_after_bytes": 1048576,
//!   "daily_quota_after": 100,
//!   "dest_folder_missing": true,
//!   "dest_folder_readonly": false,
//!   "update_not_found": false,
//!   "source_listing_broken": false,
//!   "trashed_visible_in_find_by_op_uuid": false,
//!   "fileid_recycle": false,
//!   "content_oracle": false
//! }
//! ```
//!
//! Unknown fields are REJECTED (`deny_unknown_fields`): a typo'd fault name in
//! a harness scenario must fail loudly, not silently arm nothing and let the
//! scenario "pass" without its fault ever firing (the STRESS_HARNESS s3.9
//! "every row must PROVE its fault fired" principle, applied at parse time).

use serde::Deserialize;

use super::InMemoryRemoteStore;

/// A declarative fault plan, deserialized from JSON (see the module docs for
/// the shape). Applied via [`FaultPlan::apply_to`].
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultPlan {
    /// [`InMemoryRemoteStore::with_rate_limit_after`].
    pub rate_limit_after: Option<u64>,
    /// [`InMemoryRemoteStore::with_5xx_after`].
    pub http_5xx_after: Option<u64>,
    /// [`InMemoryRemoteStore::with_invalid_grant_after`].
    pub invalid_grant_after: Option<u64>,
    /// [`InMemoryRemoteStore::with_network_drop_after`].
    pub network_drop_after: Option<u64>,
    /// [`InMemoryRemoteStore::with_network_drop`] - EVERY request fails.
    #[serde(default)]
    pub network_drop_every_request: bool,
    /// [`InMemoryRemoteStore::with_slow_responses`], in milliseconds.
    pub slow_responses_ms: Option<u64>,
    /// [`InMemoryRemoteStore::with_session_invalidated_after`].
    pub session_invalidated_after_chunks: Option<u32>,
    /// [`InMemoryRemoteStore::with_md5_mismatch_after`].
    pub md5_mismatch_after: Option<u64>,
    /// [`InMemoryRemoteStore::with_quota_exhausted_after`].
    pub quota_exhausted_after_bytes: Option<u64>,
    /// [`InMemoryRemoteStore::with_daily_quota_after`].
    pub daily_quota_after: Option<u64>,
    /// [`InMemoryRemoteStore::with_dest_folder_missing`].
    #[serde(default)]
    pub dest_folder_missing: bool,
    /// [`InMemoryRemoteStore::with_dest_folder_readonly`].
    #[serde(default)]
    pub dest_folder_readonly: bool,
    /// [`InMemoryRemoteStore::with_update_not_found`].
    #[serde(default)]
    pub update_not_found: bool,
    /// [`InMemoryRemoteStore::with_source_listing_broken`].
    #[serde(default)]
    pub source_listing_broken: bool,
    /// [`InMemoryRemoteStore::with_trashed_visible_in_find_by_op_uuid`].
    #[serde(default)]
    pub trashed_visible_in_find_by_op_uuid: bool,
    /// [`InMemoryRemoteStore::with_fileid_recycle`].
    #[serde(default)]
    pub fileid_recycle: bool,
    /// [`InMemoryRemoteStore::with_content_oracle`].
    #[serde(default)]
    pub content_oracle: bool,
}

impl FaultPlan {
    /// Parse a plan from its JSON document. Unknown fields are an error (see
    /// the module docs for why).
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Arm every configured fault on `store`. Consumes and returns the store
    /// (builder style); because the store's fault state is behind a shared
    /// `Arc`, the faults are visible to every clone of the same store.
    #[must_use]
    pub fn apply_to(&self, mut store: InMemoryRemoteStore) -> InMemoryRemoteStore {
        if let Some(n) = self.rate_limit_after {
            store = store.with_rate_limit_after(n);
        }
        if let Some(n) = self.http_5xx_after {
            store = store.with_5xx_after(n);
        }
        if let Some(n) = self.invalid_grant_after {
            store = store.with_invalid_grant_after(n);
        }
        if let Some(n) = self.network_drop_after {
            store = store.with_network_drop_after(n);
        }
        if self.network_drop_every_request {
            store = store.with_network_drop();
        }
        if let Some(ms) = self.slow_responses_ms {
            store = store.with_slow_responses(std::time::Duration::from_millis(ms));
        }
        if let Some(n) = self.session_invalidated_after_chunks {
            store = store.with_session_invalidated_after(n);
        }
        if let Some(n) = self.md5_mismatch_after {
            store = store.with_md5_mismatch_after(n);
        }
        if let Some(n) = self.quota_exhausted_after_bytes {
            store = store.with_quota_exhausted_after(n);
        }
        if let Some(n) = self.daily_quota_after {
            store = store.with_daily_quota_after(n);
        }
        if self.dest_folder_missing {
            store = store.with_dest_folder_missing();
        }
        if self.dest_folder_readonly {
            store = store.with_dest_folder_readonly();
        }
        if self.update_not_found {
            store = store.with_update_not_found();
        }
        if self.source_listing_broken {
            store = store.with_source_listing_broken();
        }
        if self.trashed_visible_in_find_by_op_uuid {
            store = store.with_trashed_visible_in_find_by_op_uuid();
        }
        if self.fileid_recycle {
            store = store.with_fileid_recycle();
        }
        if self.content_oracle {
            store = store.with_content_oracle();
        }
        store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_store::{DriveContext, RemoteStore};

    #[test]
    fn empty_plan_arms_nothing() {
        let plan = FaultPlan::from_json("{}").unwrap();
        assert_eq!(plan, FaultPlan::default());
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = FaultPlan::from_json(r#"{"rate_limit_afer": 1}"#).unwrap_err();
        assert!(
            err.to_string().contains("rate_limit_afer"),
            "the typo'd field must be named in the error: {err}"
        );
    }

    #[tokio::test]
    async fn network_drop_after_zero_fails_the_next_request() {
        let plan = FaultPlan::from_json(r#"{"network_drop_after": 0}"#).unwrap();
        let store = plan.apply_to(InMemoryRemoteStore::new());
        let root = store.root_id().to_string();
        // The very next request trips the (single-shot) network drop...
        let err = store
            .list_folder(&root, &DriveContext::MyDrive)
            .await
            .expect_err("first request after arming must fail");
        assert!(
            err.to_string().contains("network"),
            "expected a network-class error, got: {err}"
        );
        // ...and the one after that succeeds (transient fault semantics).
        store
            .list_folder(&root, &DriveContext::MyDrive)
            .await
            .expect("transient fault resets after tripping");
    }

    #[tokio::test]
    async fn latched_faults_apply_to_clones_of_the_store() {
        // The app registry hands CLONES of one store to the picker + the
        // orchestrator; a plan applied at creation must bite on every clone.
        let plan = FaultPlan::from_json(r#"{"dest_folder_missing": true}"#).unwrap();
        let store = plan.apply_to(InMemoryRemoteStore::new());
        let clone = store.clone();
        // dest_folder_missing bites WRITE-target calls (ensure_folder), not
        // reads - see `with_dest_folder_missing`.
        let err = clone
            .ensure_folder(store.root_id(), "sub", &DriveContext::MyDrive)
            .await
            .expect_err("dest_folder_missing must bite on a clone");
        assert!(
            err.to_string().contains("dest_folder_missing"),
            "expected the SPEC s24 dest_folder_missing error, got: {err}"
        );
    }
}
