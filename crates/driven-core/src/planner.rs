//! diff -> ops; SPEC s7.
//!
//! The planner turns a [`ScanResult`](crate::types::ScanResult) (the diff a
//! single scan of one source produced, SPEC s6) into a [`Plan`] of
//! [`Op`]s the executor (SPEC s8) runs. It is the second stage of the
//! per-source pipeline the orchestrator drives:
//! `scan -> plan -> execute -> verify` (SPEC s5).
//!
//! The planner performs no I/O of its own beyond the two `file_state`
//! lookups every deleted path needs; it is otherwise a pure fold over the
//! scan result. All persistence flows through the injected
//! [`StateRepo`] trait (SPEC s2) so this module is exercisable against an
//! in-memory fake without SQLite.
//!
//! ## Rename detection: not done in V1 (ROADMAP M2)
//!
//! V1 does NOT detect renames. A rename on disk surfaces in the scan as a
//! delete of the old relative path plus an add of the new one, so the
//! planner emits exactly **one upload** (for the new path) and **one
//! trash** (for the old path's remote object) - it never recognises that
//! the two paths share content and reuses the existing Drive object. This
//! is the documented V1 behaviour (ROADMAP M2 "Rename -> currently
//! produces one upload + one trash; we don't detect renames in V1").
//! Content-addressed rename detection is deferred to a later milestone.

use anyhow::Result;

use crate::state::SourceRow;
use crate::state::StateRepo;
use crate::types::{Op, Plan, ScanResult};

/// Module-level tracing target (SPEC s0 logging convention).
const TARGET: &str = "driven::core::planner";

/// Turn one source's [`ScanResult`] into a [`Plan`] (SPEC s7).
///
/// For each entry in `scan.new_or_changed` emit one
/// [`Op::HashThenUpload`] carrying the pre-open size captured by the
/// scanner. For each path in `scan.deleted` consult the stored
/// `file_state` row:
///
/// - If the row has a `drive_file_id`, the file reached Drive, so emit one
///   [`Op::Trash`] targeting that remote object. The executor trashes
///   rather than hard-deletes so the user can recover from a mistaken
///   delete via the Drive web UI (SPEC s7).
/// - If the row has no `drive_file_id`, the file never made it to Drive,
///   so there is nothing to trash: delete the `file_state` row directly
///   and emit no op (SPEC s7).
///
/// A deleted path always traces back to a `file_state` row (the scanner
/// derives `deleted` from the loaded `file_state` keys, SPEC s6), so a
/// missing row is unreachable in practice. House rules forbid `expect()`
/// in non-test code, so rather than panic on the `None` the SPEC s7
/// pseudocode glosses with `.expect("must exist")`, this treats it as a
/// graceful no-op (nothing on disk, nothing on Drive) and logs a warning -
/// "anything ambiguous is a bug" (SPEC s0).
///
/// Ops are emitted uploads-first then trashes, in scan-iteration order.
/// The executor is free to reorder for concurrency but must preserve
/// happens-before semantics per `(source_id, relative_path)` (see
/// [`Plan::ops`]).
///
/// See the module docs for why renames yield one upload + one trash in V1.
pub async fn plan(source: &SourceRow, scan: &ScanResult, state: &dyn StateRepo) -> Result<Plan> {
    let mut ops = Vec::with_capacity(scan.new_or_changed.len() + scan.deleted.len());

    for entry in &scan.new_or_changed {
        ops.push(Op::HashThenUpload {
            source_id: source.id,
            relative_path: entry.rel.clone(),
            size: entry.size,
        });
    }

    for path in &scan.deleted {
        match state.get_file_state(source.id, path).await? {
            Some(row) => match row.drive_file_id {
                Some(file_id) => ops.push(Op::Trash {
                    source_id: source.id,
                    relative_path: path.clone(),
                    drive_file_id: file_id,
                }),
                // Never reached Drive: drop the row, emit no op (SPEC s7).
                None => state.delete_file_state(source.id, path).await?,
            },
            // Unreachable in practice (see fn docs); treat as a no-op.
            None => {
                tracing::warn!(
                    target: TARGET,
                    source_id = %source.id,
                    relative_path = %path,
                    "deleted path had no file_state row; skipping (SPEC s7 invariant violated)"
                );
            }
        }
    }

    Ok(Plan { ops })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use anyhow::Result;
    use async_trait::async_trait;

    use super::*;
    use crate::state::{
        AccountRow, ActivityFilter, ActivityPage, FileSearchHit, FileStateRow, NewActivity,
        NewPendingOp, PageRequest, PendingOpRow, SourceRow, StateRepo,
    };
    use crate::types::{
        AccountId, AccountState, ActivityId, FileStateStatus, LocalEntry, Op, PendingOpId,
        RelativePath, ScanResult, SourceId, UnixMs,
    };

    /// In-memory `StateRepo` fake. Only the two methods `plan()` exercises
    /// (`get_file_state`, `delete_file_state`) have real bodies; every other
    /// method is `unimplemented!()` (allowed under `#[cfg(test)]`).
    ///
    /// State lives behind `std::sync::Mutex` (not `RefCell`) because
    /// `StateRepo: Send + Sync` and the `#[async_trait]` futures must be
    /// `Send` - `RefCell` is `!Sync` and would fail the bound.
    #[derive(Default)]
    struct FakeStateRepo {
        rows: Mutex<HashMap<(SourceId, RelativePath), FileStateRow>>,
        /// Paths passed to `delete_file_state`, for assertions.
        deleted: Mutex<Vec<(SourceId, RelativePath)>>,
    }

    impl FakeStateRepo {
        fn with_rows(rows: Vec<FileStateRow>) -> Self {
            let map = rows
                .into_iter()
                .map(|r| ((r.source_id, r.relative_path.clone()), r))
                .collect();
            Self {
                rows: Mutex::new(map),
                deleted: Mutex::new(Vec::new()),
            }
        }

        fn deleted_paths(&self) -> Vec<(SourceId, RelativePath)> {
            self.deleted.lock().expect("lock").clone()
        }

        fn contains(&self, source: SourceId, path: &RelativePath) -> bool {
            self.rows
                .lock()
                .expect("lock")
                .contains_key(&(source, path.clone()))
        }
    }

    #[async_trait]
    impl StateRepo for FakeStateRepo {
        async fn get_file_state(
            &self,
            source: SourceId,
            path: &RelativePath,
        ) -> Result<Option<FileStateRow>> {
            Ok(self
                .rows
                .lock()
                .expect("lock")
                .get(&(source, path.clone()))
                .cloned())
        }

        async fn delete_file_state(&self, source: SourceId, path: &RelativePath) -> Result<()> {
            self.rows
                .lock()
                .expect("lock")
                .remove(&(source, path.clone()));
            self.deleted
                .lock()
                .expect("lock")
                .push((source, path.clone()));
            Ok(())
        }

        // --- everything below is untouched by plan(); test-only stubs. -------

        async fn list_accounts(&self) -> Result<Vec<AccountRow>> {
            unimplemented!()
        }
        async fn upsert_account(&self, _row: &AccountRow) -> Result<()> {
            unimplemented!()
        }
        async fn mark_account_state(&self, _id: AccountId, _state: AccountState) -> Result<()> {
            unimplemented!()
        }
        async fn delete_account(&self, _id: AccountId) -> Result<()> {
            unimplemented!()
        }
        async fn list_sources(&self) -> Result<Vec<SourceRow>> {
            unimplemented!()
        }
        async fn list_enabled_sources_for(&self, _account: AccountId) -> Result<Vec<SourceRow>> {
            unimplemented!()
        }
        async fn upsert_source(&self, _row: &SourceRow) -> Result<()> {
            unimplemented!()
        }
        async fn delete_source(&self, _id: SourceId) -> Result<()> {
            unimplemented!()
        }
        async fn load_source_file_state(
            &self,
            _source: SourceId,
        ) -> Result<HashMap<RelativePath, FileStateRow>> {
            unimplemented!()
        }
        async fn upsert_file_state(&self, _row: &FileStateRow) -> Result<()> {
            unimplemented!()
        }
        async fn enqueue_pending_op(&self, _row: NewPendingOp) -> Result<PendingOpId> {
            unimplemented!()
        }
        async fn get_pending_ops_due(
            &self,
            _now_ms: UnixMs,
            _limit: u32,
        ) -> Result<Vec<PendingOpRow>> {
            unimplemented!()
        }
        async fn get_pending_ops_for_source(&self, _source: SourceId) -> Result<Vec<PendingOpRow>> {
            unimplemented!()
        }
        async fn mark_pending_op_attempted(
            &self,
            _id: PendingOpId,
            _error: Option<&str>,
            _next_attempt_ms: UnixMs,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn delete_pending_op(&self, _id: PendingOpId) -> Result<()> {
            unimplemented!()
        }
        async fn commit_create_result(
            &self,
            _op_id: PendingOpId,
            _file_state: &FileStateRow,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn commit_update_result(
            &self,
            _op_id: PendingOpId,
            _file_state: &FileStateRow,
        ) -> Result<()> {
            unimplemented!()
        }
        async fn write_activity(&self, _row: NewActivity) -> Result<ActivityId> {
            unimplemented!()
        }
        async fn query_activity(
            &self,
            _filter: ActivityFilter,
            _page: PageRequest,
        ) -> Result<ActivityPage> {
            unimplemented!()
        }
        async fn prune_activity_older_than(
            &self,
            _before_ms: UnixMs,
            _hard_cap: u64,
            _batch_size: Option<u32>,
        ) -> Result<u64> {
            unimplemented!()
        }
        async fn delete_activity_by_source(&self, _source: SourceId) -> Result<u64> {
            unimplemented!()
        }
        async fn get_setting(&self, _key: &str) -> Result<Option<serde_json::Value>> {
            unimplemented!()
        }
        async fn set_setting(&self, _key: &str, _value: &serde_json::Value) -> Result<()> {
            unimplemented!()
        }
        async fn search_files(
            &self,
            _source: Option<SourceId>,
            _query: &str,
            _limit: u32,
        ) -> Result<Vec<FileSearchHit>> {
            unimplemented!()
        }
    }

    // --- test helpers --------------------------------------------------------

    fn rel(s: &str) -> RelativePath {
        RelativePath::try_from(s.to_string()).expect("valid relative path")
    }

    fn test_source(id: SourceId) -> SourceRow {
        SourceRow {
            id,
            account_id: AccountId::new_v4(),
            display_name: "test".into(),
            enabled: true,
            local_path: "/tmp/src".into(),
            drive_folder_id: "folder-1".into(),
            drive_folder_path: "/Backups/test".into(),
            encryption_enabled: false,
            wrapped_source_key: None,
            respect_gitignore: true,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            schedule_json_v2_reserved: None,
            deep_verify_interval_secs: 604_800,
            last_full_scan_at: None,
            last_deep_verify_at: None,
            created_at: 0,
        }
    }

    fn local_entry(path: &str, size: u64) -> LocalEntry {
        LocalEntry {
            rel: rel(path),
            size,
            mtime_ns: 0,
        }
    }

    /// A `file_state` row that HAS reached Drive (has a `drive_file_id`).
    fn synced_row(source: SourceId, path: &str, file_id: &str) -> FileStateRow {
        FileStateRow {
            source_id: source,
            relative_path: rel(path),
            size: 10,
            mtime_ns: 0,
            hash_blake3: [0u8; 32],
            drive_file_id: Some(file_id.into()),
            drive_md5: Some([0u8; 16]),
            encrypted_remote_path: None,
            status: FileStateStatus::Synced,
            last_uploaded_at: Some(1),
            last_verified_at: None,
        }
    }

    /// A `file_state` row that NEVER reached Drive (`drive_file_id` is None).
    fn pending_row(source: SourceId, path: &str) -> FileStateRow {
        FileStateRow {
            source_id: source,
            relative_path: rel(path),
            size: 10,
            mtime_ns: 0,
            hash_blake3: [0u8; 32],
            drive_file_id: None,
            drive_md5: None,
            encrypted_remote_path: None,
            status: FileStateStatus::Pending,
            last_uploaded_at: None,
            last_verified_at: None,
        }
    }

    /// Empty remote, first scan: every observed file is new -> all uploads,
    /// no trashes (ROADMAP M2 "empty remote first scan").
    #[tokio::test]
    async fn empty_remote_first_scan_all_uploads() {
        let source = test_source(SourceId::new_v4());
        let scan = ScanResult {
            new_or_changed: vec![local_entry("a.txt", 1), local_entry("b/c.txt", 2)],
            deleted: Vec::new(),
            collisions: Vec::new(),
            excluded_orphans: Vec::new(),
        };
        let state = FakeStateRepo::default();

        let plan = plan(&source, &scan, &state).await.unwrap();

        let summary = plan.summary();
        assert_eq!(summary.uploads, 2);
        assert_eq!(summary.trashes, 0);
        assert_eq!(summary.bytes, 3);
        assert!(plan
            .ops
            .iter()
            .all(|op| matches!(op, Op::HashThenUpload { .. })));
    }

    /// Nothing changed: the scan is empty -> the plan is empty.
    #[tokio::test]
    async fn unchanged_yields_empty_plan() {
        let source = test_source(SourceId::new_v4());
        let scan = ScanResult::default();
        let state = FakeStateRepo::default();

        let plan = plan(&source, &scan, &state).await.unwrap();

        assert!(plan.ops.is_empty());
        let summary = plan.summary();
        assert_eq!(summary.uploads, 0);
        assert_eq!(summary.trashes, 0);
        assert_eq!(summary.bytes, 0);
    }

    /// A single changed file -> exactly one upload op carrying its size.
    #[tokio::test]
    async fn single_change_one_upload() {
        let source = test_source(SourceId::new_v4());
        let scan = ScanResult {
            new_or_changed: vec![local_entry("doc.txt", 42)],
            deleted: Vec::new(),
            collisions: Vec::new(),
            excluded_orphans: Vec::new(),
        };
        let state = FakeStateRepo::default();

        let plan = plan(&source, &scan, &state).await.unwrap();

        assert_eq!(plan.ops.len(), 1);
        match &plan.ops[0] {
            Op::HashThenUpload {
                source_id,
                relative_path,
                size,
            } => {
                assert_eq!(*source_id, source.id);
                assert_eq!(relative_path, &rel("doc.txt"));
                assert_eq!(*size, 42);
            }
            other => panic!("expected HashThenUpload, got {other:?}"),
        }
    }

    /// A single deleted file whose row HAS a `drive_file_id` -> one trash
    /// op targeting that remote object (ROADMAP M2 "single file deleted").
    #[tokio::test]
    async fn single_delete_with_drive_id_one_trash() {
        let source = test_source(SourceId::new_v4());
        let row = synced_row(source.id, "gone.txt", "drive-file-99");
        let scan = ScanResult {
            new_or_changed: Vec::new(),
            deleted: vec![rel("gone.txt")],
            collisions: Vec::new(),
            excluded_orphans: Vec::new(),
        };
        let state = FakeStateRepo::with_rows(vec![row]);

        let plan = plan(&source, &scan, &state).await.unwrap();

        assert_eq!(plan.ops.len(), 1);
        match &plan.ops[0] {
            Op::Trash {
                source_id,
                relative_path,
                drive_file_id,
            } => {
                assert_eq!(*source_id, source.id);
                assert_eq!(relative_path, &rel("gone.txt"));
                assert_eq!(drive_file_id, "drive-file-99");
            }
            other => panic!("expected Trash, got {other:?}"),
        }
        let summary = plan.summary();
        assert_eq!(summary.trashes, 1);
        assert_eq!(summary.uploads, 0);
        // The row is NOT deleted here; the executor removes it after the
        // trash op succeeds (SPEC s8). The planner only emits the op.
        assert!(state.deleted_paths().is_empty());
    }

    /// A deleted path that never reached Drive (row has no `drive_file_id`)
    /// -> zero ops and the `file_state` row is removed directly (SPEC s7).
    #[tokio::test]
    async fn delete_never_uploaded_zero_ops_row_removed() {
        let source = test_source(SourceId::new_v4());
        let row = pending_row(source.id, "never-up.txt");
        let scan = ScanResult {
            new_or_changed: Vec::new(),
            deleted: vec![rel("never-up.txt")],
            collisions: Vec::new(),
            excluded_orphans: Vec::new(),
        };
        let state = FakeStateRepo::with_rows(vec![row]);

        let plan = plan(&source, &scan, &state).await.unwrap();

        assert!(plan.ops.is_empty());
        // delete_file_state was called for the never-uploaded path...
        assert_eq!(
            state.deleted_paths(),
            vec![(source.id, rel("never-up.txt"))]
        );
        // ...and the row is actually gone.
        assert!(!state.contains(source.id, &rel("never-up.txt")));
    }

    /// A rename surfaces as delete(old) + add(new). V1 does NOT detect
    /// renames, so the plan is exactly one upload (new path) + one trash
    /// (old path's remote object) - see module docs / ROADMAP M2.
    #[tokio::test]
    async fn rename_yields_one_upload_and_one_trash() {
        let source = test_source(SourceId::new_v4());
        let old_row = synced_row(source.id, "old-name.txt", "drive-old");
        let scan = ScanResult {
            new_or_changed: vec![local_entry("new-name.txt", 7)],
            deleted: vec![rel("old-name.txt")],
            collisions: Vec::new(),
            excluded_orphans: Vec::new(),
        };
        let state = FakeStateRepo::with_rows(vec![old_row]);

        let plan = plan(&source, &scan, &state).await.unwrap();

        let summary = plan.summary();
        assert_eq!(summary.uploads, 1, "rename uploads the new path");
        assert_eq!(summary.trashes, 1, "rename trashes the old remote object");
        assert_eq!(summary.bytes, 7);

        // Uploads are emitted before trashes.
        assert!(matches!(plan.ops[0], Op::HashThenUpload { .. }));
        match &plan.ops[1] {
            Op::Trash {
                relative_path,
                drive_file_id,
                ..
            } => {
                assert_eq!(relative_path, &rel("old-name.txt"));
                assert_eq!(drive_file_id, "drive-old");
            }
            other => panic!("expected Trash, got {other:?}"),
        }
    }
}
