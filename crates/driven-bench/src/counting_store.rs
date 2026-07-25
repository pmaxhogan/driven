//! A [`RemoteStore`] decorator that counts the Drive requests underneath it.
//!
//! Neither `driven-core` nor `driven-drive` keeps a request counter, and "how
//! many API calls did that cost" is one of the more interesting numbers a backup
//! benchmark can report - on the million-tiny-files shape it is usually the
//! binding constraint, not bandwidth. Wrapping the store is the same seam the
//! executor already uses for `BreakerReportingStore`, so it needs no core change
//! and adds one relaxed atomic increment per call.
//!
//! One caveat the report repeats: `resume_chunk` is counted per CHUNK, which is
//! the honest unit (each chunk is its own HTTP PUT), so a resumable upload of a
//! large file contributes many calls.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

use driven_drive::remote_store::{
    AboutInfo, DownloadStream, DriveContext, RemoteEntry, RemoteStore, ResumableKind,
    ResumableSession, ResumeProgress, SharedDrive, UploadBody,
};

/// Per-operation request counts collected during one run.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ApiCounts {
    pub ensure_folder: u64,
    pub list_folder: u64,
    pub list_shared_drives: u64,
    pub create: u64,
    pub update: u64,
    pub resumable_session: u64,
    pub resume_chunk: u64,
    pub trash: u64,
    pub delete_permanent: u64,
    pub metadata: u64,
    pub download: u64,
    pub find_by_op_uuid: u64,
    pub list_source_object_ids: u64,
    pub about: u64,
    /// The sum of every field above.
    pub total: u64,
}

/// The live counters behind a [`CountingStore`].
#[derive(Debug, Default)]
pub struct Counters {
    ensure_folder: AtomicU64,
    list_folder: AtomicU64,
    list_shared_drives: AtomicU64,
    create: AtomicU64,
    update: AtomicU64,
    resumable_session: AtomicU64,
    resume_chunk: AtomicU64,
    trash: AtomicU64,
    delete_permanent: AtomicU64,
    metadata: AtomicU64,
    download: AtomicU64,
    find_by_op_uuid: AtomicU64,
    list_source_object_ids: AtomicU64,
    about: AtomicU64,
}

impl Counters {
    /// Takes a snapshot of every counter.
    pub fn snapshot(&self) -> ApiCounts {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let mut counts = ApiCounts {
            ensure_folder: load(&self.ensure_folder),
            list_folder: load(&self.list_folder),
            list_shared_drives: load(&self.list_shared_drives),
            create: load(&self.create),
            update: load(&self.update),
            resumable_session: load(&self.resumable_session),
            resume_chunk: load(&self.resume_chunk),
            trash: load(&self.trash),
            delete_permanent: load(&self.delete_permanent),
            metadata: load(&self.metadata),
            download: load(&self.download),
            find_by_op_uuid: load(&self.find_by_op_uuid),
            list_source_object_ids: load(&self.list_source_object_ids),
            about: load(&self.about),
            total: 0,
        };
        counts.total = counts.ensure_folder
            + counts.list_folder
            + counts.list_shared_drives
            + counts.create
            + counts.update
            + counts.resumable_session
            + counts.resume_chunk
            + counts.trash
            + counts.delete_permanent
            + counts.metadata
            + counts.download
            + counts.find_by_op_uuid
            + counts.list_source_object_ids
            + counts.about;
        counts
    }
}

/// Wraps a [`RemoteStore`], counting every call before delegating.
pub struct CountingStore {
    inner: Arc<dyn RemoteStore>,
    counters: Arc<Counters>,
}

impl CountingStore {
    /// Wraps `inner`, returning the store and the counters to read afterwards.
    pub fn new(inner: Arc<dyn RemoteStore>) -> (Arc<Self>, Arc<Counters>) {
        let counters = Arc::new(Counters::default());
        let store = Arc::new(Self {
            inner,
            counters: counters.clone(),
        });
        (store, counters)
    }
}

/// Increments one counter.
fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

#[async_trait]
impl RemoteStore for CountingStore {
    async fn ensure_folder(
        &self,
        parent_id: &str,
        name: &str,
        drive_context: &DriveContext,
    ) -> anyhow::Result<RemoteEntry> {
        bump(&self.counters.ensure_folder);
        self.inner
            .ensure_folder(parent_id, name, drive_context)
            .await
    }

    async fn list_folder(
        &self,
        folder_id: &str,
        drive_context: &DriveContext,
    ) -> anyhow::Result<Vec<RemoteEntry>> {
        bump(&self.counters.list_folder);
        self.inner.list_folder(folder_id, drive_context).await
    }

    async fn list_shared_drives(&self) -> anyhow::Result<Vec<SharedDrive>> {
        bump(&self.counters.list_shared_drives);
        self.inner.list_shared_drives().await
    }

    async fn create(
        &self,
        parent_id: &str,
        name: &str,
        mime: &str,
        body: UploadBody,
        app_properties: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        bump(&self.counters.create);
        self.inner
            .create(parent_id, name, mime, body, app_properties)
            .await
    }

    async fn update(
        &self,
        file_id: &str,
        body: UploadBody,
        app_properties_patch: HashMap<String, String>,
    ) -> anyhow::Result<RemoteEntry> {
        bump(&self.counters.update);
        self.inner.update(file_id, body, app_properties_patch).await
    }

    async fn resumable_session(
        &self,
        kind: ResumableKind,
        mime: &str,
        size: u64,
    ) -> anyhow::Result<ResumableSession> {
        bump(&self.counters.resumable_session);
        self.inner.resumable_session(kind, mime, size).await
    }

    async fn resume_chunk(
        &self,
        session: &ResumableSession,
        offset: u64,
        chunk: Bytes,
    ) -> anyhow::Result<ResumeProgress> {
        bump(&self.counters.resume_chunk);
        self.inner.resume_chunk(session, offset, chunk).await
    }

    async fn trash(&self, file_id: &str) -> anyhow::Result<()> {
        bump(&self.counters.trash);
        self.inner.trash(file_id).await
    }

    async fn delete_permanent(&self, file_id: &str) -> anyhow::Result<()> {
        bump(&self.counters.delete_permanent);
        self.inner.delete_permanent(file_id).await
    }

    async fn metadata(&self, file_id: &str) -> anyhow::Result<RemoteEntry> {
        bump(&self.counters.metadata);
        self.inner.metadata(file_id).await
    }

    async fn download(&self, file_id: &str) -> anyhow::Result<DownloadStream> {
        bump(&self.counters.download);
        self.inner.download(file_id).await
    }

    async fn find_by_op_uuid(
        &self,
        parent_id: &str,
        op_uuid: &str,
        drive_context: &DriveContext,
    ) -> anyhow::Result<Option<RemoteEntry>> {
        bump(&self.counters.find_by_op_uuid);
        self.inner
            .find_by_op_uuid(parent_id, op_uuid, drive_context)
            .await
    }

    async fn list_source_object_ids(
        &self,
        source_id: &str,
        drive_context: &DriveContext,
    ) -> anyhow::Result<HashSet<String>> {
        bump(&self.counters.list_source_object_ids);
        self.inner
            .list_source_object_ids(source_id, drive_context)
            .await
    }

    async fn about(&self) -> anyhow::Result<AboutInfo> {
        bump(&self.counters.about);
        self.inner.about().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use driven_drive::fake::InMemoryRemoteStore;

    #[tokio::test]
    async fn counts_each_delegated_call_and_totals_them() {
        let fake = Arc::new(InMemoryRemoteStore::new());
        let root = fake.root_id().to_string();
        let (store, counters) = CountingStore::new(fake);

        assert_eq!(counters.snapshot().total, 0);

        let ctx = DriveContext::MyDrive;
        let folder = store.ensure_folder(&root, "a", &ctx).await.unwrap();
        store
            .create(
                &folder.id,
                "f.bin",
                "application/octet-stream",
                UploadBody::Bytes(vec![1, 2, 3].into()),
                HashMap::new(),
            )
            .await
            .unwrap();
        store.list_folder(&folder.id, &ctx).await.unwrap();
        store.list_folder(&folder.id, &ctx).await.unwrap();

        let counts = counters.snapshot();
        assert_eq!(counts.ensure_folder, 1);
        assert_eq!(counts.create, 1);
        assert_eq!(counts.list_folder, 2);
        assert_eq!(counts.total, 4, "total must be the sum of every counter");
    }

    #[tokio::test]
    async fn a_failing_call_is_still_counted() {
        let fake = Arc::new(InMemoryRemoteStore::new());
        let (store, counters) = CountingStore::new(fake);
        // An id that was never created: the call fails, but it still cost a
        // request, so the benchmark must count it.
        assert!(store.metadata("no-such-id").await.is_err());
        assert_eq!(counters.snapshot().metadata, 1);
    }
}
